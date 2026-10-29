//! 重启之后，桥接着上次的账继续做。
//!
//! 这条链路只有真落盘才能成立：编号、水位、重投次数都在库里，进程换一茬还是同一份。
//! 用 SQLite 文件而不是内存库——内存库测不出「关掉再开」。

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use acb_kernel::{
    Bridge, Config, Element, Ingest, Outbound, OutboundError, RecordDraft, RoomId, Sender, Sent,
    SqliteStore, Transcript, TurnReason,
};

const AT: i64 = 1_793_000_000_000;

struct TempDb {
    path: PathBuf,
}

impl TempDb {
    fn new(name: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("时间正常")
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("acb-restart-{}-{name}-{nanos}", std::process::id()));
        Self {
            path: dir.join("acb.db"),
        }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// 出站通道：这个测试不往外发，只保证桥拿得到一个。
struct Sink;

impl Outbound for Sink {
    fn send(&mut self, _room: &RoomId, _elements: &[Element]) -> Result<Sent, OutboundError> {
        Ok(Sent {
            platform_id: "p-1".into(),
            at: AT,
        })
    }
}

fn config() -> &'static str {
    r#"
        [[connections]]
        id = "qq"
        platform = "qq"
        kind = "onebot11"
        url = "ws://127.0.0.1:3001"
        account = "3683924053"

        [[devices]]
        id = "main"
        persona = "p"
        memory = "m"
        token = "t-main"

        [[devices.subscriptions]]
        connection = "qq"
        room = "private/323622442"

        [devices.subscriptions.wake]
        at_mention = false
        name_mention = false
        poke = false
        accumulate = 1
    "#
}

fn room() -> RoomId {
    RoomId::parse("qq:private/323622442").expect("房间合法")
}

/// 一次接入代表的设备组（测试里就一台）。
fn devices() -> Vec<String> {
    vec!["main".to_owned()]
}

fn message(text: &str) -> RecordDraft {
    RecordDraft::message(
        room(),
        Sender {
            id: "323622442".into(),
            name: "萨卡班甲鱼".into(),
        },
        AT,
        vec![Element::text(text)],
        false,
    )
}

fn open_bridge(path: &Path) -> Bridge<SqliteStore> {
    Bridge::new(
        Config::parse(config()).expect("配置合法"),
        SqliteStore::open(path).expect("库能打开"),
        Box::new(Sink),
    )
}

#[test]
fn a_restart_continues_numbering_and_redelivery_counts() {
    let db = TempDb::new("flow");

    // 第一程：一条消息进来，开一轮投出去，但**不确认**（模拟 harness 没调 ack）。
    let first_id;
    {
        let mut bridge = open_bridge(db.path());
        let ingested = bridge.ingest(message("第一句")).expect("能记");
        assert!(matches!(ingested, Ingest::Recorded { .. }));

        let delivery = bridge.poll_turn().expect("能开轮").expect("应当有一轮");
        assert_eq!(delivery.reason, TurnReason::Wake);
        assert_eq!(delivery.records.len(), 1);
        first_id = delivery.records[0].id;
        assert_eq!(first_id, 1, "第一次投的是 #1");
        assert_eq!(delivery.records[0].redelivery, None, "第一次投不算重投");

        bridge.end_turn(&delivery.ticket).expect("能收工");
    }

    // 第二程：换了进程（新桥、新连接），库里还是那份东西。
    let mut bridge = open_bridge(db.path());
    let delivered = bridge
        .store()
        .latest(&room())
        .expect("能读")
        .expect("流水不为空");
    assert_eq!(delivered, first_id, "重启后流水还在，最后一条还是 #1");

    // 再来一条：开轮时应当把 #1 一起带上，并标成「第 2 次投递」。
    bridge.ingest(message("第二句")).expect("能记");
    let delivery = bridge.poll_turn().expect("能开轮").expect("应当有一轮");
    assert_eq!(delivery.records.len(), 2, "没确认的那条要跟着再来一次");

    let again = &delivery.records[0];
    assert_eq!(again.id, first_id, "还是同一条记录，编号不变");
    assert_eq!(again.redelivery, Some(2), "重启之后次数接着数，不回到 1");
    let note = again.note.as_deref().expect("重投要带旁白");
    assert!(note.contains("第 2 次投递"), "旁白要写清第几次：{note}");
    assert!(
        note.contains(&format!("#{first_id}")),
        "旁白要写清确认哪条：{note}"
    );
    assert_eq!(
        again.elements,
        vec![Element::text("第一句")],
        "旁白不能混进内容"
    );

    let fresh = &delivery.records[1];
    assert_eq!(fresh.id, 2, "新的一条接着编号");
    assert_eq!(fresh.redelivery, None, "新的一条是第一次投");
    assert!(fresh.note.is_none(), "没重投就不该有旁白");

    // 这一轮确认掉，水位推到底。
    let ticket = delivery.ticket.clone();
    let result = bridge
        .tool_call(
            &ticket,
            &devices(),
            "ack_messages",
            serde_json::json!({ "ids": [1, 2] }),
        )
        .expect("确认应当成功");
    assert_eq!(result["watermark"], 2);
    bridge.end_turn(&delivery.ticket).expect("能收工");

    // 第三程：水位也是落盘的，重启后不该重投。
    let mut bridge = open_bridge(db.path());
    bridge.ingest(message("第三句")).expect("能记");
    let delivery = bridge.poll_turn().expect("能开轮").expect("应当有一轮");
    assert_eq!(delivery.records.len(), 1, "已确认的不会再投");
    assert_eq!(delivery.records[0].id, 3);
    assert_eq!(delivery.records[0].redelivery, None);
}
