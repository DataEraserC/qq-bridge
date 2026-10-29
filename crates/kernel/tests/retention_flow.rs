//! 流水保留：只削读过的，未读一条不动；编号不清零。
//!
//! 这一层验的是**判据**——谁算读过、什么时候可以删。存储层怎么执行判据在
//! `storage.rs` 与 `sqlite_store.rs` 里各自验。
//!
//! 判据有一条最容易搞错、后果也最严重：一个房间有两个会话订阅时，安全线取两者水位的
//! **最小值**。任何一方没读到，这个房间就一条都不删——那几条还是人家下一轮的投递原料。

use acb_kernel::{
    Bridge, Cursor, Element, MemoryStore, Outbound, OutboundError, RecordDraft, RoomId, Sender,
    Sent, Transcript, unread,
};

const AT: i64 = 1_793_000_000_000;
const DAY: i64 = 86_400_000;

struct Sink;

impl Outbound for Sink {
    fn send(&mut self, _room: &RoomId, _elements: &[Element]) -> Result<Sent, OutboundError> {
        Ok(Sent {
            platform_id: "p-1".into(),
            at: AT,
        })
    }
}

fn config(max_records: u64, max_days: u64) -> String {
    format!(
        r#"
        [[connections]]
        id = "qq"
        platform = "qq"
        kind = "onebot11"
        url = "ws://127.0.0.1:3001"
        account = "bot"

        [[personas]]
        id = "p"
        name = "人格"
        text = "你是桥。"

        [[harnesses]]
        id = "dsh"
        name = "DeepSeek"

        [[sessions]]
        id = "main"
        harness = "dsh"
        persona = "p"
        token = "t-main"

        [[sessions.subscriptions]]
        connection = "qq"
        room = "group/123"

        [sessions.subscriptions.wake]
        accumulate = 1

        [[sessions]]
        id = "helper"
        harness = "dsh"
        persona = "p"
        token = "t-helper"

        [[sessions.subscriptions]]
        connection = "qq"
        room = "group/123"

        [sessions.subscriptions.wake]
        accumulate = 1

        [retention]
        max_records_per_room = {max_records}
        max_age_days = {max_days}
        "#
    )
}

fn bridge(max_records: u64, max_days: u64) -> Bridge<MemoryStore> {
    Bridge::new(
        acb_kernel::Config::parse(&config(max_records, max_days)).expect("配置合法"),
        MemoryStore::new(),
        Box::new(Sink),
    )
}

fn room() -> RoomId {
    RoomId::parse("qq:group/123").expect("房间合法")
}

fn message(text: &str, at: i64) -> RecordDraft {
    RecordDraft::message(
        room(),
        Sender {
            id: "323622442".into(),
            name: "萨卡班甲鱼".into(),
        },
        at,
        vec![Element::text(text)],
        false,
    )
}

/// 让某个会话领一轮，并且把这些编号确认掉。走的是真工具调用，不是内部捷径。
fn ack(bridge: &mut Bridge<MemoryStore>, session: &str, ids: &[u64]) {
    let delivery = bridge
        .poll_turn_for(&[session.to_owned()])
        .expect("能取")
        .expect("该有活干");
    bridge
        .tool_call(
            &delivery.ticket,
            &[session.to_owned()],
            "ack_messages",
            serde_json::json!({ "ids": ids }),
        )
        .expect("能确认");
    bridge.end_turn(&delivery.ticket).expect("能收工");
}

fn left(bridge: &Bridge<MemoryStore>) -> Vec<u64> {
    bridge
        .store()
        .since(&room(), 0, 100)
        .expect("可读")
        .iter()
        .map(|record| record.id)
        .collect()
}

#[test]
fn one_unread_subscriber_blocks_the_whole_room() {
    let mut bridge = bridge(2, 0);
    for index in 1..=5 {
        bridge
            .ingest(message(&format!("第 {index} 句"), AT))
            .expect("能记");
    }

    // 只有 main 读了；helper 一条都没读。
    ack(&mut bridge, "main", &[1, 2, 3]);

    let report = bridge.prune(AT).expect("能清");
    assert!(report.is_empty(), "helper 还没读，一条都不该删：{report:?}");
    assert_eq!(left(&bridge), vec![1, 2, 3, 4, 5]);
}

#[test]
fn both_read_then_only_the_old_ones_go() {
    let mut bridge = bridge(2, 0);
    for index in 1..=5 {
        bridge
            .ingest(message(&format!("第 {index} 句"), AT))
            .expect("能记");
    }

    ack(&mut bridge, "main", &[1, 2, 3]);
    ack(&mut bridge, "helper", &[1, 2, 3]);

    let report = bridge.prune(AT).expect("能清");
    assert_eq!(report.len(), 1);
    assert_eq!(report[0].deleted, 2, "1、2 删掉");
    assert_eq!(report[0].kept, 3);
    assert_eq!(left(&bridge), vec![3, 4, 5], "读到哪儿就从哪儿留着");
}

#[test]
fn the_time_line_takes_only_what_is_old() {
    let mut bridge = bridge(0, 30); // 不限条数，只看时间
    bridge
        .ingest(message("很久以前", AT - 60 * DAY))
        .expect("能记");
    bridge.ingest(message("刚才", AT)).expect("能记");

    ack(&mut bridge, "main", &[1, 2]);
    ack(&mut bridge, "helper", &[1, 2]);

    let report = bridge.prune(AT).expect("能清");
    assert_eq!(report[0].deleted, 1, "只删 60 天前那条");
    assert_eq!(left(&bridge), vec![2]);
}

#[test]
fn numbering_survives_a_room_being_pruned_to_one() {
    let mut bridge = bridge(0, 30);
    for index in 1..=3 {
        bridge
            .ingest(message(&format!("旧话 {index}"), AT - 60 * DAY))
            .expect("能记");
    }

    ack(&mut bridge, "main", &[1, 2, 3]);
    ack(&mut bridge, "helper", &[1, 2, 3]);

    let report = bridge.prune(AT).expect("能清");
    assert_eq!(report[0].deleted, 2);
    assert_eq!(left(&bridge), vec![3], "每房间至少留一条，作为编号基准");

    // 新消息接着 4 编号；如果编号回到 1，它会落在水位后面，直接消失。
    let ingest = bridge.ingest(message("新话", AT)).expect("能记");
    assert_eq!(
        ingest,
        acb_kernel::Ingest::Recorded {
            id: 4,
            woke: vec!["main".into(), "helper".into()]
        }
    );

    let pending = unread(bridge.store(), &Cursor::new("main", room()), 100).expect("可读");
    assert_eq!(
        pending.iter().map(|record| record.id).collect::<Vec<_>>(),
        vec![4],
        "新消息仍然是未读，没被水位吃掉"
    );
}

#[test]
fn zero_on_both_limits_means_never_prune() {
    let mut bridge = bridge(0, 0);
    for index in 1..=3 {
        bridge
            .ingest(message(&format!("第 {index} 句"), AT - 600 * DAY))
            .expect("能记");
    }
    ack(&mut bridge, "main", &[1, 2, 3]);
    ack(&mut bridge, "helper", &[1, 2, 3]);

    assert!(
        bridge.prune(AT).expect("能清").is_empty(),
        "两项都是 0 就是完全不清理"
    );
    assert_eq!(left(&bridge), vec![1, 2, 3]);
}
