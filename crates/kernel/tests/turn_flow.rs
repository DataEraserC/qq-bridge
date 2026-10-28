//! 端到端：一条入站消息进来，到 agent 回话、确认水位，整条链路走一遍。
//!
//! 这里用内存存储 + 一个记账用的出站通道，把 harness 那侧换成直接调桥的方法——
//! 链路本身和真接 harness 时一模一样：票、工具、水位都是真的。

use std::sync::{Arc, Mutex};

use qqab_kernel::{
    Bridge, Cursor, Element, ErrorCode, Ingest, MemoryStore, NoteKey, NoteKind, Notes, Outbound,
    OutboundError, RecordDraft, RoomId, Sender, Sent, Transcript, TurnReason, Watermarks, unread,
};
use serde_json::json;

const AT: i64 = 1_793_000_000_000;

type Sends = Arc<Mutex<Vec<(String, RoomId, String, Vec<Element>)>>>;

/// 记账用的出站通道：把「发了什么」存起来，不真的往外发。
struct Recorder {
    sends: Sends,
}

impl Outbound for Recorder {
    fn send(
        &mut self,
        device: &str,
        room: &RoomId,
        account: &str,
        elements: &[Element],
    ) -> Result<Sent, OutboundError> {
        let mut sends = self.sends.lock().expect("没被毒死");
        sends.push((
            device.to_owned(),
            room.clone(),
            account.to_owned(),
            elements.to_vec(),
        ));
        Ok(Sent {
            platform_id: format!("p-{}", sends.len()),
            at: AT,
        })
    }
}

fn config() -> &'static str {
    r#"
        [[devices]]
        id = "main"
        persona = "p"
        memory = "m"

        [[devices.subscriptions]]
        room = "qq:123"
        send_account = "bot"

        [[devices.subscriptions]]
        room = "discord:456"
        send_account = "dc-bot"

        [devices.subscriptions.wake]
        at_mention = false
        name_mention = false
        poke = false
        accumulate = 50
    "#
}

fn bridge() -> (Bridge<MemoryStore>, Sends) {
    let sends: Sends = Arc::new(Mutex::new(Vec::new()));
    let bridge = Bridge::new(
        qqab_kernel::Config::parse(config()).expect("配置合法"),
        MemoryStore::new(),
        Box::new(Recorder {
            sends: sends.clone(),
        }),
    );
    (bridge, sends)
}

fn room(text: &str) -> RoomId {
    RoomId::parse(text).expect("房间合法")
}

fn message(room: &str, sender: &str, text: &str, at_self: bool) -> RecordDraft {
    RecordDraft::message(
        room_of(room),
        Sender {
            id: sender.to_owned(),
            name: "老王".to_owned(),
        },
        AT,
        vec![Element::text(text)],
        at_self,
    )
}

fn room_of(text: &str) -> RoomId {
    RoomId::parse(text).expect("房间合法")
}

fn poke(room: &str, at_self: bool) -> RecordDraft {
    RecordDraft::event(
        room_of(room),
        Sender {
            id: "10001".to_owned(),
            name: "老王".to_owned(),
        },
        AT,
        "poke",
        vec![Element::text("[拍了拍我]")],
        at_self,
    )
}

fn cursor(device: &str, room: &str) -> Cursor {
    Cursor::new(device, room_of(room))
}

// ---------------------------------------------------------------------------
// 主链路
// ---------------------------------------------------------------------------

#[test]
fn mention_wakes_then_reply_and_ack_advance_the_watermark() {
    let (mut bridge, sends) = bridge();

    // 1. 群里有人 @ 我。
    let ingested = bridge
        .ingest(message("qq:123", "10001", "在吗", true))
        .expect("能记下来");
    assert_eq!(
        ingested,
        Ingest::Recorded {
            id: 1,
            woke: vec!["main".to_owned()]
        }
    );

    // 2. 取到一轮，投的就是刚才那条。
    let delivery = bridge.poll_turn().expect("能开轮").expect("有活");
    assert_eq!(delivery.device, "main");
    assert_eq!(delivery.room, room("qq:123"));
    assert_eq!(delivery.reason, TurnReason::Wake);
    assert_eq!(delivery.records.len(), 1);
    assert_eq!(delivery.records[0].id, 1);
    assert_eq!(delivery.records[0].redelivery, None, "第一次投递不带标注");

    let ticket = delivery.ticket;

    // 3. 回一句话：真的走到出站，账号用的是订阅里配的。
    let result = bridge
        .tool_call(
            &ticket,
            "send_message",
            json!({"elements": [{"type": "text", "text": "在"}]}),
        )
        .expect("能发");
    assert_eq!(result["id"], 2, "自己的话也进流水，编号接着排");

    let sends = sends.lock().expect("没被毒死");
    assert_eq!(sends.len(), 1);
    assert_eq!(sends[0].0, "main");
    assert_eq!(sends[0].1, room("qq:123"));
    assert_eq!(sends[0].2, "bot", "用订阅里配的账号发");
    drop(sends);

    // 4. 确认收到的那条：水位推到 2（1 是收到的，2 是自己发的、已自动确认）。
    let acked = bridge
        .tool_call(&ticket, "ack_messages", json!({"ids": [1]}))
        .expect("能确认");
    assert_eq!(acked["watermark"], 2);
    assert_eq!(
        bridge
            .store()
            .watermark(&cursor("main", "qq:123"))
            .expect("可读"),
        2
    );

    // 5. 收工，队列空了。
    bridge.end_turn(&ticket).expect("能收工");
    assert!(bridge.poll_turn().expect("能开轮").is_none());
    assert!(
        unread(bridge.store(), &cursor("main", "qq:123"), 100)
            .expect("可读")
            .is_empty()
    );
}

#[test]
fn own_messages_never_come_back_as_unread() {
    let (mut bridge, _sends) = bridge();

    bridge
        .ingest(message("qq:123", "10001", "在吗", true))
        .expect("能记下来");
    let first = bridge.poll_turn().expect("能开轮").expect("有活").ticket;

    bridge
        .tool_call(
            &first,
            "send_message",
            json!({"elements": [{"type": "text", "text": "在"}]}),
        )
        .expect("能发");

    // 故意不确认第一条就收工——水位被挡住，正是「自己的话会不会被当成未读」的极端情况。
    bridge.end_turn(&first).expect("能收工");
    bridge
        .ingest(message("qq:123", "10001", "还有话", true))
        .expect("能记下来");

    let second = bridge.poll_turn().expect("能开轮").expect("有活");
    let ids: Vec<u64> = second.records.iter().map(|record| record.id).collect();
    assert_eq!(
        ids,
        vec![1, 3],
        "自己发的 2 号不该出现在投递里，哪怕水位还没推过它"
    );
}

#[test]
fn platform_echo_of_our_own_message_wakes_nobody() {
    let (mut bridge, _sends) = bridge();

    let ingested = bridge
        .ingest(message("qq:123", "bot", "我自己说的", false))
        .expect("能记下来");
    assert_eq!(
        ingested,
        Ingest::Recorded {
            id: 1,
            woke: Vec::new()
        },
        "自己的回声不叫醒自己"
    );
    assert!(
        unread(bridge.store(), &cursor("main", "qq:123"), 100)
            .expect("可读")
            .is_empty()
    );
}

// ---------------------------------------------------------------------------
// 唤醒策略的差异
// ---------------------------------------------------------------------------

#[test]
fn each_subscription_uses_its_own_wake_policy() {
    let (mut bridge, _sends) = bridge();

    // Discord 那个订阅关了 @ 和拍一拍，一条普通消息 + 一次拍一拍都不该叫醒它。
    let ingested = bridge
        .ingest(message("discord:456", "10001", "看这个", true))
        .expect("能记下来");
    assert_eq!(
        ingested,
        Ingest::Recorded {
            id: 1,
            woke: Vec::new()
        }
    );
    let ingested = bridge.ingest(poke("discord:456", true)).expect("能记下来");
    assert_eq!(
        ingested,
        Ingest::Recorded {
            id: 2,
            woke: Vec::new()
        },
        "这个订阅关了 poke"
    );

    // QQ 那个订阅是默认策略：拍一拍就醒。
    let ingested = bridge.ingest(poke("qq:123", true)).expect("能记下来");
    assert_eq!(
        ingested,
        Ingest::Recorded {
            id: 1,
            woke: vec!["main".to_owned()]
        }
    );

    // 但两边都记了流水——不叫醒不等于不记录。
    assert_eq!(
        bridge.store().latest(&room("discord:456")).expect("可读"),
        Some(2)
    );
    assert_eq!(
        bridge.store().latest(&room("qq:123")).expect("可读"),
        Some(1)
    );
}

#[test]
fn unsubscribed_rooms_are_not_even_recorded() {
    let (mut bridge, _sends) = bridge();

    let ingested = bridge
        .ingest(message("telegram:789", "10001", "你好", true))
        .expect("不报错");
    assert_eq!(
        ingested,
        Ingest::NotSubscribed {
            room: room("telegram:789")
        }
    );
    assert_eq!(
        bridge.store().latest(&room("telegram:789")).expect("可读"),
        None,
        "没订阅的房间连流水都不写"
    );
    assert!(bridge.poll_turn().expect("能开轮").is_none());
}

// ---------------------------------------------------------------------------
// 串行与重投
// ---------------------------------------------------------------------------

#[test]
fn a_busy_device_queues_and_does_not_run_two_turns() {
    let (mut bridge, _sends) = bridge();

    bridge
        .ingest(message("qq:123", "10001", "第一句", true))
        .expect("能记下来");
    let first = bridge.poll_turn().expect("能开轮").expect("有活");

    // 第一轮还在跑，第二条消息到了：记下来，但不并行开第二轮。
    let ingested = bridge
        .ingest(message("qq:123", "10001", "第二句", true))
        .expect("能记下来");
    assert_eq!(
        ingested,
        Ingest::Recorded {
            id: 2,
            woke: vec!["main".to_owned()]
        }
    );
    assert!(
        bridge.poll_turn().expect("能开轮").is_none(),
        "同一设备不能同时跑两轮"
    );

    bridge.end_turn(&first.ticket).expect("能收工");

    let second = bridge
        .poll_turn()
        .expect("能开轮")
        .expect("排队的活现在能跑了");
    assert_eq!(second.records.len(), 2, "没确认的第一条会跟着重投");
}

#[test]
fn unacked_records_come_back_marked_as_redelivery() {
    let (mut bridge, _sends) = bridge();

    bridge
        .ingest(message("qq:123", "10001", "第一句", true))
        .expect("能记下来");
    let first = bridge.poll_turn().expect("能开轮").expect("有活");
    // 故意不确认就收工。
    bridge.end_turn(&first.ticket).expect("能收工");

    bridge
        .ingest(message("qq:123", "10001", "第二句", true))
        .expect("能记下来");
    let second = bridge.poll_turn().expect("能开轮").expect("有活");

    assert_eq!(second.records.len(), 2);
    assert_eq!(second.records[0].id, 1);
    assert_eq!(
        second.records[0].redelivery,
        Some(2),
        "第二次投递要标出来，AI 才知道这条重复了"
    );
    assert_eq!(second.records[1].redelivery, None, "新的一条是第一次投");
}

// ---------------------------------------------------------------------------
// 工具面的边界
// ---------------------------------------------------------------------------

#[test]
fn send_message_to_is_fenced_by_subscriptions() {
    let (mut bridge, sends) = bridge();
    bridge
        .ingest(message("qq:123", "10001", "在吗", true))
        .expect("能记下来");
    let ticket = bridge.poll_turn().expect("能开轮").expect("有活").ticket;

    // 订阅内的目标可以发。
    bridge
        .tool_call(
            &ticket,
            "send_message_to",
            json!({
                "room": "discord:456",
                "elements": [{"type": "text", "text": "转过去"}]
            }),
        )
        .expect("订阅内的房间能发");
    assert_eq!(sends.lock().expect("没被毒死")[0].2, "dc-bot");

    // 订阅外的目标直接拒。
    let error = bridge
        .tool_call(
            &ticket,
            "send_message_to",
            json!({
                "room": "telegram:789",
                "elements": [{"type": "text", "text": "发不出去"}]
            }),
        )
        .expect_err("订阅外必须拒");
    assert_eq!(error.code, ErrorCode::RoomNotSubscribed);
}

#[test]
fn ack_messages_refuses_ids_that_were_not_delivered() {
    let (mut bridge, _sends) = bridge();
    bridge
        .ingest(message("qq:123", "10001", "在吗", true))
        .expect("能记下来");
    bridge
        .ingest(message("qq:123", "10001", "在吗在吗", true))
        .expect("能记下来");

    // 只投了 200 条的上限内全部——两条都在，先造一条「没投过」的需要另一条路径：
    // 这里直接确认一个跳到未来、根本没投过的编号。
    let ticket = bridge.poll_turn().expect("能开轮").expect("有活").ticket;
    let error = bridge
        .tool_call(&ticket, "ack_messages", json!({"ids": [99]}))
        .expect_err("没投过的编号不能确认");
    assert_eq!(error.code, ErrorCode::BadRequest);

    // 投过的那两条就能确认。
    bridge
        .tool_call(&ticket, "ack_messages", json!({"ids": [1, 2]}))
        .expect("投过的能确认");
    assert_eq!(
        bridge
            .store()
            .watermark(&cursor("main", "qq:123"))
            .expect("可读"),
        2
    );
}

#[test]
fn a_stale_ticket_is_rejected() {
    let (mut bridge, _sends) = bridge();
    bridge
        .ingest(message("qq:123", "10001", "在吗", true))
        .expect("能记下来");
    let ticket = bridge.poll_turn().expect("能开轮").expect("有活").ticket;
    bridge.end_turn(&ticket).expect("能收工");

    let error = bridge
        .tool_call(
            &ticket,
            "send_message",
            json!({"elements": [{"type": "text", "text": "迟到的"}]}),
        )
        .expect_err("收工之后的票作废");
    assert_eq!(error.code, ErrorCode::Unauthorized);
}

#[test]
fn unknown_tools_are_refused() {
    let (mut bridge, _sends) = bridge();
    bridge
        .ingest(message("qq:123", "10001", "在吗", true))
        .expect("能记下来");
    let ticket = bridge.poll_turn().expect("能开轮").expect("有活").ticket;

    let error = bridge
        .tool_call(&ticket, "run_shell", json!({"cmd": "rm -rf /"}))
        .expect_err("没这个工具");
    assert_eq!(error.code, ErrorCode::ToolNotAllowed);
}

// ---------------------------------------------------------------------------
// 读取类工具
// ---------------------------------------------------------------------------

#[test]
fn history_and_events_read_from_the_transcript() {
    let (mut bridge, _sends) = bridge();
    bridge
        .ingest(message("qq:123", "10001", "第一句", true))
        .expect("能记下来");
    bridge.ingest(poke("qq:123", true)).expect("能记下来");
    let ticket = bridge.poll_turn().expect("能开轮").expect("有活").ticket;

    let history = bridge
        .tool_call(&ticket, "get_history", json!({}))
        .expect("能查");
    assert_eq!(history["records"].as_array().expect("是数组").len(), 2);

    let events = bridge
        .tool_call(&ticket, "get_platform_events", json!({}))
        .expect("能查");
    let events = events["events"].as_array().expect("是数组");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["kind"]["event"], "poke");

    let rooms = bridge
        .tool_call(&ticket, "list_rooms", json!({}))
        .expect("能查");
    assert_eq!(rooms["rooms"].as_array().expect("是数组").len(), 2);
    assert_eq!(rooms["rooms"][0]["room"]["platform"], "qq");
}

#[test]
fn notes_written_in_one_room_are_invisible_in_another() {
    let (mut bridge, _sends) = bridge();
    bridge
        .ingest(message("qq:123", "10001", "在吗", true))
        .expect("能记下来");
    let ticket = bridge.poll_turn().expect("能开轮").expect("有活").ticket;

    bridge
        .tool_call(
            &ticket,
            "set_note",
            json!({"kind": "term", "key": "部署", "value": "蓝绿发布"}),
        )
        .expect("能写");

    let mine = bridge
        .tool_call(&ticket, "get_notes", json!({}))
        .expect("能读");
    assert_eq!(mine["notes"].as_array().expect("是数组").len(), 1);

    let other = bridge
        .tool_call(&ticket, "get_notes", json!({"room": "discord:456"}))
        .expect("能读");
    assert!(
        other["notes"].as_array().expect("是数组").is_empty(),
        "另一个房间看不到这条笔记"
    );
}

#[test]
fn notes_are_isolated_per_platform_down_to_storage() {
    // 直接打存储层：QQ 群 123 和 Discord 频道 123 是同一个号、不同平台。
    let mut store = MemoryStore::new();
    let qq = room("qq:123");
    let discord = room("discord:123");

    let qq_key = NoteKey::new(qq.clone(), NoteKind::Term, "部署");
    store
        .upsert_note(qqab_kernel::Note {
            key: qq_key.clone(),
            value: "蓝绿发布".to_owned(),
            label: None,
            status: None,
            corrected: false,
            updated_at: AT,
        })
        .expect("能写");

    assert!(store.note(&qq_key).expect("能读").is_some());
    assert!(
        store
            .note(&NoteKey::new(discord, NoteKind::Term, "部署"))
            .expect("能读")
            .is_none()
    );
}
