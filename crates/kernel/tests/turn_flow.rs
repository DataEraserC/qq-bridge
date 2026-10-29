//! 端到端：一条入站消息进来，到 agent 回话、确认水位，整条链路走一遍。
//!
//! 这里用内存存储 + 一个记账用的出站通道，把 harness 那侧换成直接调桥的方法——
//! 链路本身和真接 harness 时一模一样：票、工具、水位都是真的。

use std::sync::{Arc, Mutex};

use acb_kernel::{
    Bridge, Cursor, Element, ErrorCode, Ingest, MemoryStore, NoteKey, NoteKind, Notes, Outbound,
    OutboundError, RecordDraft, RoomId, Sender, Sent, Transcript, TurnReason, Watermarks, unread,
};
use serde_json::json;

const AT: i64 = 1_793_000_000_000;

type Sends = Arc<Mutex<Vec<(RoomId, Vec<Element>)>>>;

/// 记账用的出站通道：把「发到哪儿、发了什么」存起来，不真的往外发。
struct Recorder {
    sends: Sends,
}

impl Outbound for Recorder {
    fn send(&mut self, room: &RoomId, elements: &[Element]) -> Result<Sent, OutboundError> {
        let mut sends = self.sends.lock().expect("没被毒死");
        sends.push((room.clone(), elements.to_vec()));
        Ok(Sent {
            platform_id: format!("p-{}", sends.len()),
            at: AT,
        })
    }
}

/// 一次接入代表的设备组（测试里就一台）。
fn devices() -> Vec<String> {
    vec!["main".to_owned()]
}

fn config() -> &'static str {
    r#"
        [[connections]]
        id = "qq"
        platform = "qq"
        kind = "onebot11"
        url = "ws://127.0.0.1:3001"
        account = "bot"

        [[connections]]
        id = "dc"
        platform = "discord"
        kind = "onebot11"
        url = "ws://127.0.0.1:3002"
        account = "dc-bot"

        [[devices]]
        id = "main"
        persona = "p"
        memory = "m"
        token = "t-main"

        [[devices.subscriptions]]
        connection = "qq"
        room = "group/123"

        [[devices.subscriptions]]
        connection = "dc"
        room = "channel/456"

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
        acb_kernel::Config::parse(config()).expect("配置合法"),
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
        .ingest(message("qq:group/123", "10001", "在吗", true))
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
    assert_eq!(delivery.room, room("qq:group/123"));
    assert_eq!(delivery.reason, TurnReason::Wake);
    assert_eq!(delivery.records.len(), 1);
    assert_eq!(delivery.records[0].id, 1);
    assert_eq!(delivery.records[0].redelivery, None, "第一次投递不带标注");

    let ticket = delivery.ticket;

    // 3. 回一句话：真的走到出站，身份用的是这条连接登录的账号。
    let result = bridge
        .tool_call(
            &ticket,
            &devices(),
            "send_message",
            json!({"elements": [{"type": "text", "text": "在"}]}),
        )
        .expect("能发");
    assert_eq!(result["id"], 2, "自己的话也进流水，编号接着排");

    let sends = sends.lock().expect("没被毒死");
    assert_eq!(sends.len(), 1);
    assert_eq!(sends[0].0, room("qq:group/123"));
    drop(sends);

    let stored = bridge
        .store()
        .since(&room("qq:group/123"), 1, 10)
        .expect("能读");
    assert_eq!(
        stored.last().expect("有自己那条").sender.id,
        "bot",
        "用这条连接登录的账号发"
    );

    // 4. 确认收到的那条：水位推到 2（1 是收到的，2 是自己发的、已自动确认）。
    let acked = bridge
        .tool_call(&ticket, &devices(), "ack_messages", json!({"ids": [1]}))
        .expect("能确认");
    assert_eq!(acked["watermark"], 2);
    assert_eq!(
        bridge
            .store()
            .watermark(&cursor("main", "qq:group/123"))
            .expect("可读"),
        2
    );

    // 5. 收工，队列空了。
    bridge.end_turn(&ticket).expect("能收工");
    assert!(bridge.poll_turn().expect("能开轮").is_none());
    assert!(
        unread(bridge.store(), &cursor("main", "qq:group/123"), 100)
            .expect("可读")
            .is_empty()
    );
}

#[test]
fn own_messages_never_come_back_as_unread() {
    let (mut bridge, _sends) = bridge();

    bridge
        .ingest(message("qq:group/123", "10001", "在吗", true))
        .expect("能记下来");
    let first = bridge.poll_turn().expect("能开轮").expect("有活").ticket;

    bridge
        .tool_call(
            &first,
            &devices(),
            "send_message",
            json!({"elements": [{"type": "text", "text": "在"}]}),
        )
        .expect("能发");

    // 故意不确认第一条就收工——水位被挡住，正是「自己的话会不会被当成未读」的极端情况。
    bridge.end_turn(&first).expect("能收工");
    bridge
        .ingest(message("qq:group/123", "10001", "还有话", true))
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
        .ingest(message("qq:group/123", "bot", "我自己说的", false))
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
        unread(bridge.store(), &cursor("main", "qq:group/123"), 100)
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
        .ingest(message("dc:channel/456", "10001", "看这个", true))
        .expect("能记下来");
    assert_eq!(
        ingested,
        Ingest::Recorded {
            id: 1,
            woke: Vec::new()
        }
    );
    let ingested = bridge
        .ingest(poke("dc:channel/456", true))
        .expect("能记下来");
    assert_eq!(
        ingested,
        Ingest::Recorded {
            id: 2,
            woke: Vec::new()
        },
        "这个订阅关了 poke"
    );

    // QQ 那个订阅是默认策略：拍一拍就醒。
    let ingested = bridge.ingest(poke("qq:group/123", true)).expect("能记下来");
    assert_eq!(
        ingested,
        Ingest::Recorded {
            id: 1,
            woke: vec!["main".to_owned()]
        }
    );

    // 但两边都记了流水——不叫醒不等于不记录。
    assert_eq!(
        bridge
            .store()
            .latest(&room("dc:channel/456"))
            .expect("可读"),
        Some(2)
    );
    assert_eq!(
        bridge.store().latest(&room("qq:group/123")).expect("可读"),
        Some(1)
    );
}

#[test]
fn unsubscribed_rooms_are_not_even_recorded() {
    let (mut bridge, _sends) = bridge();

    let ingested = bridge
        .ingest(message("tg:group/789", "10001", "你好", true))
        .expect("不报错");
    assert_eq!(
        ingested,
        Ingest::NotSubscribed {
            room: room("tg:group/789")
        }
    );
    assert_eq!(
        bridge.store().latest(&room("tg:group/789")).expect("可读"),
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
        .ingest(message("qq:group/123", "10001", "第一句", true))
        .expect("能记下来");
    let first = bridge.poll_turn().expect("能开轮").expect("有活");

    // 第一轮还在跑，第二条消息到了：记下来，但不并行开第二轮。
    let ingested = bridge
        .ingest(message("qq:group/123", "10001", "第二句", true))
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
        .ingest(message("qq:group/123", "10001", "第一句", true))
        .expect("能记下来");
    let first = bridge.poll_turn().expect("能开轮").expect("有活");
    // 故意不确认就收工。
    bridge.end_turn(&first.ticket).expect("能收工");

    bridge
        .ingest(message("qq:group/123", "10001", "第二句", true))
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

    // 光有 `redelivery` 字段不够：模型看的是内容，得在内容旁边就告诉它怎么止投。
    // 但这句话不能进 elements——那是对方说的话，混进去就成了「对方打了这行字」。
    let note = second.records[0].note.as_deref().unwrap_or_default();
    assert!(note.contains("第 2 次投递"), "重投要写明第几次：{note}");
    assert!(note.contains("#1"), "要写清确认哪条：{note}");
    assert!(
        note.contains("ack_messages"),
        "要告诉 AI 用什么止投：{note}"
    );
    assert_eq!(second.records[0].elements.len(), 1, "旁白不能混进内容元素");
    assert_eq!(
        second.records[1].elements.len(),
        1,
        "第一次投递不该被塞任何提示"
    );
    assert!(second.records[1].note.is_none(), "第一次投递不需要旁白");
}

// ---------------------------------------------------------------------------
// 工具面的边界
// ---------------------------------------------------------------------------

#[test]
fn send_message_to_is_fenced_by_subscriptions() {
    let (mut bridge, sends) = bridge();
    bridge
        .ingest(message("qq:group/123", "10001", "在吗", true))
        .expect("能记下来");
    let ticket = bridge.poll_turn().expect("能开轮").expect("有活").ticket;

    // 订阅内的目标可以发。
    bridge
        .tool_call(
            &ticket,
            &devices(),
            "send_message_to",
            json!({
                "to": "dc:channel/456",
                "elements": [{"type": "text", "text": "转过去"}]
            }),
        )
        .expect("订阅内的房间能发");
    assert_eq!(
        sends.lock().expect("没被毒死")[0].0,
        room("dc:channel/456"),
        "发到的是 dc 那条连接上的频道"
    );

    // 订阅外的目标直接拒。
    let error = bridge
        .tool_call(
            &ticket,
            &devices(),
            "send_message_to",
            json!({
                "to": "tg:group/789",
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
        .ingest(message("qq:group/123", "10001", "在吗", true))
        .expect("能记下来");
    bridge
        .ingest(message("qq:group/123", "10001", "在吗在吗", true))
        .expect("能记下来");

    // 只投了 200 条的上限内全部——两条都在，先造一条「没投过」的需要另一条路径：
    // 这里直接确认一个跳到未来、根本没投过的编号。
    let ticket = bridge.poll_turn().expect("能开轮").expect("有活").ticket;
    let error = bridge
        .tool_call(&ticket, &devices(), "ack_messages", json!({"ids": [99]}))
        .expect_err("没投过的编号不能确认");
    assert_eq!(error.code, ErrorCode::BadRequest);

    // 投过的那两条就能确认。
    bridge
        .tool_call(&ticket, &devices(), "ack_messages", json!({"ids": [1, 2]}))
        .expect("投过的能确认");
    assert_eq!(
        bridge
            .store()
            .watermark(&cursor("main", "qq:group/123"))
            .expect("可读"),
        2
    );
}

#[test]
fn a_stale_ticket_is_rejected() {
    let (mut bridge, _sends) = bridge();
    bridge
        .ingest(message("qq:group/123", "10001", "在吗", true))
        .expect("能记下来");
    let ticket = bridge.poll_turn().expect("能开轮").expect("有活").ticket;
    bridge.end_turn(&ticket).expect("能收工");

    let error = bridge
        .tool_call(
            &ticket,
            &devices(),
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
        .ingest(message("qq:group/123", "10001", "在吗", true))
        .expect("能记下来");
    let ticket = bridge.poll_turn().expect("能开轮").expect("有活").ticket;

    let error = bridge
        .tool_call(&ticket, &devices(), "run_shell", json!({"cmd": "rm -rf /"}))
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
        .ingest(message("qq:group/123", "10001", "第一句", true))
        .expect("能记下来");
    bridge.ingest(poke("qq:group/123", true)).expect("能记下来");
    let ticket = bridge.poll_turn().expect("能开轮").expect("有活").ticket;

    let history = bridge
        .tool_call(&ticket, &devices(), "get_history", json!({}))
        .expect("能查");
    assert_eq!(history["records"].as_array().expect("是数组").len(), 2);

    let events = bridge
        .tool_call(&ticket, &devices(), "get_platform_events", json!({}))
        .expect("能查");
    let events = events["events"].as_array().expect("是数组");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["kind"]["event"], "poke");

    let rooms = bridge
        .tool_call(&ticket, &devices(), "list_rooms", json!({}))
        .expect("能查");
    assert_eq!(rooms["rooms"].as_array().expect("是数组").len(), 2);
    assert_eq!(rooms["rooms"][0]["room"]["connection"], "qq");
}

#[test]
fn notes_written_in_one_room_are_invisible_in_another() {
    let (mut bridge, _sends) = bridge();
    bridge
        .ingest(message("qq:group/123", "10001", "在吗", true))
        .expect("能记下来");
    let ticket = bridge.poll_turn().expect("能开轮").expect("有活").ticket;

    bridge
        .tool_call(
            &ticket,
            &devices(),
            "set_note",
            json!({"kind": "term", "key": "部署", "value": "蓝绿发布"}),
        )
        .expect("能写");

    let mine = bridge
        .tool_call(&ticket, &devices(), "get_notes", json!({}))
        .expect("能读");
    assert_eq!(mine["notes"].as_array().expect("是数组").len(), 1);

    let other = bridge
        .tool_call(
            &ticket,
            &devices(),
            "get_notes",
            json!({"room": "dc:channel/456"}),
        )
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
    let qq = room("qq:group/123");
    let discord = room("discord:123");

    let qq_key = NoteKey::new(qq.clone(), NoteKind::Term, "部署");
    store
        .upsert_note(acb_kernel::Note {
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

// ---------------------------------------------------------------------------
// 引用回复：QQ 里「回复」不带 @，也该叫醒
// ---------------------------------------------------------------------------

#[test]
fn quoting_my_message_wakes_without_a_mention() {
    let (mut bridge, _sends) = bridge();

    // 1. 先让我方把话说出去：这条会带着平台编号 p-1 进流水。
    bridge
        .ingest(message("qq:group/123", "10001", "在吗", true))
        .expect("能记下来");
    let delivery = bridge.poll_turn().expect("能开轮").expect("有活");
    let ticket = delivery.ticket;
    bridge
        .tool_call(
            &ticket,
            &devices(),
            "send_message",
            json!({"elements": [{"type": "text", "text": "在"}]}),
        )
        .expect("能发");
    bridge
        .tool_call(&ticket, &devices(), "ack_messages", json!({"ids": [1]}))
        .expect("能确认");
    bridge.end_turn(&ticket).expect("能收尾");

    // 2. 对方「引用」我那条再说一句，既没 @ 也没叫名字，条数更不到 50。
    let quoted = RecordDraft::message(
        room("qq:group/123"),
        Sender {
            id: "10001".into(),
            name: "老王".into(),
        },
        AT + 1000,
        vec![
            Element::Quote {
                message_id: "p-1".into(),
            },
            Element::text("只发子网"),
        ],
        false,
    );
    let ingested = bridge.ingest(quoted).expect("能记下来");

    assert_eq!(
        ingested,
        Ingest::Recorded {
            id: 3,
            woke: vec!["main".to_owned()]
        },
        "引用我的话就是在跟我说话，不 @ 也该醒"
    );
    assert!(
        bridge
            .store()
            .since(&room("qq:group/123"), 2, 10)
            .expect("能读")
            .last()
            .expect("有那条")
            .reply_to_self,
        "记录上要留下这个事实，不能只用来叫醒就丢了"
    );
}

#[test]
fn quoting_someone_elses_message_stays_quiet() {
    let (mut bridge, _sends) = bridge();

    // 别人的一条消息，带平台编号 m-9。它不会叫醒（没 @、没名字、不到 50 条）。
    let others = message("qq:group/123", "10001", "随便说说", false).with_platform_id("m-9");
    assert_eq!(
        bridge.ingest(others).expect("能记下来"),
        Ingest::Recorded {
            id: 1,
            woke: Vec::new()
        }
    );

    // 引用的是别人的话，不是我的 → 照旧不醒。
    let quoted = RecordDraft::message(
        room("qq:group/123"),
        Sender {
            id: "10001".into(),
            name: "老王".into(),
        },
        AT + 1000,
        vec![
            Element::Quote {
                message_id: "m-9".into(),
            },
            Element::text("我也这么想"),
        ],
        false,
    );
    assert_eq!(
        bridge.ingest(quoted).expect("能记下来"),
        Ingest::Recorded {
            id: 2,
            woke: Vec::new()
        },
        "引用别人的话不算冲我来的"
    );
}

// ---------------------------------------------------------------------------
// 一组设备：一把令牌代表几台设备（用户裁定：接入能替这组设备说话）
// ---------------------------------------------------------------------------

/// 两台设备、同一把令牌；主设备看群 1，助手看群 2。记忆各一份。
fn group_config() -> &'static str {
    r#"
        [[connections]]
        id = "qq"
        platform = "qq"
        kind = "onebot11"
        url = "ws://127.0.0.1:3001"
        account = "bot"

        [[devices]]
        id = "main"
        persona = "p"
        memory = "m-main"
        token = "t-shared"

        [[devices.subscriptions]]
        connection = "qq"
        room = "group/1"

        [devices.subscriptions.wake]
        at_mention = true
        name_mention = false
        poke = false
        accumulate = 50

        [[devices]]
        id = "helper"
        persona = "p"
        memory = "m-helper"
        token = "t-shared"

        [[devices.subscriptions]]
        connection = "qq"
        room = "group/2"

        [devices.subscriptions.wake]
        at_mention = true
        name_mention = false
        poke = false
        accumulate = 50
    "#
}

fn group_bridge(config: &str) -> (Bridge<MemoryStore>, Sends) {
    let sends: Sends = Arc::new(Mutex::new(Vec::new()));
    let bridge = Bridge::new(
        acb_kernel::Config::parse(config).expect("配置合法"),
        MemoryStore::new(),
        Box::new(Recorder {
            sends: sends.clone(),
        }),
    );
    (bridge, sends)
}

/// 一把令牌代表的两台设备。
fn group() -> Vec<String> {
    vec!["main".to_owned(), "helper".to_owned()]
}

#[test]
fn one_token_can_speak_for_every_device_it_covers() {
    let (mut bridge, sends) = group_bridge(group_config());

    // 主设备被叫醒，领到一轮。
    bridge
        .ingest(message("qq:group/1", "10001", "在吗", true))
        .expect("能记下来");
    let delivery = bridge.poll_turn_for(&group()).expect("能取").expect("有活");
    assert_eq!(delivery.device, "main");

    // 用 to 发到助手那台设备订阅的群：这是「跨房间」，不是「跨身份」。
    bridge
        .tool_call(
            &delivery.ticket,
            &group(),
            "send_message_to",
            json!({
                "to": "qq:group/2",
                "elements": [{"type": "text", "text": "去群里说一声"}]
            }),
        )
        .expect("本组订阅的房间能发");

    let sends = sends.lock().expect("没被毒死");
    assert_eq!(sends.len(), 1);
    assert_eq!(sends[0].0, room("qq:group/2"));
}

#[test]
fn a_room_outside_the_group_is_still_refused() {
    let (mut bridge, _sends) = group_bridge(group_config());
    bridge
        .ingest(message("qq:group/1", "10001", "在吗", true))
        .expect("能记下来");
    let delivery = bridge.poll_turn_for(&group()).expect("能取").expect("有活");

    let error = bridge
        .tool_call(
            &delivery.ticket,
            &group(),
            "send_message_to",
            json!({
                "to": "qq:group/999",
                "elements": [{"type": "text", "text": "发不出去"}]
            }),
        )
        .expect_err("组外必须拒");
    assert_eq!(error.code, ErrorCode::RoomNotSubscribed);
}

#[test]
fn reading_another_devices_room_is_allowed_inside_the_group() {
    let (mut bridge, _sends) = group_bridge(group_config());

    // 助手那个群的流水：主设备这一轮里读得到（这就是「get from」）。
    bridge
        .ingest(message("qq:group/2", "10001", "另一边的消息", false))
        .expect("能记下来");
    bridge
        .ingest(message("qq:group/1", "10001", "在吗", true))
        .expect("能记下来");

    let ticket = bridge
        .poll_turn_for(&group())
        .expect("能取")
        .expect("有活")
        .ticket;

    let history = bridge
        .tool_call(
            &ticket,
            &group(),
            "get_history",
            json!({"room": "qq:group/2"}),
        )
        .expect("组内能读");
    assert_eq!(history["records"].as_array().expect("是数组").len(), 1);

    let outside = bridge
        .tool_call(
            &ticket,
            &group(),
            "get_history",
            json!({"room": "qq:group/999"}),
        )
        .expect_err("组外读不到");
    assert_eq!(outside.code, ErrorCode::RoomNotSubscribed);
}

#[test]
fn one_room_two_devices_needs_an_explicit_sender() {
    let config = group_config().replace(r#"room = "group/2""#, r#"room = "group/1""#);
    let (mut bridge, _sends) = group_bridge(&config);

    bridge
        .ingest(message("qq:group/1", "10001", "在吗", true))
        .expect("能记下来");
    let ticket = bridge
        .poll_turn_for(&group())
        .expect("能取")
        .expect("有活")
        .ticket;

    let ambiguous = bridge
        .tool_call(
            &ticket,
            &group(),
            "send_message_to",
            json!({
                "to": "qq:group/1",
                "elements": [{"type": "text", "text": "谁在说"}]
            }),
        )
        .expect_err("两台设备都在这个房间，得指明");
    assert_eq!(ambiguous.code, ErrorCode::BadRequest);
    assert!(
        ambiguous.message.contains("device"),
        "{}",
        ambiguous.message
    );

    bridge
        .tool_call(
            &ticket,
            &group(),
            "send_message_to",
            json!({
                "to": "qq:group/1",
                "device": "helper",
                "elements": [{"type": "text", "text": "以助手的身份说"}]
            }),
        )
        .expect("指明之后能发");
}

#[test]
fn devices_sharing_one_memory_never_run_two_turns_at_once() {
    // 同一个记忆 id：串行键是记忆，所以两台设备也得排队（用户裁定）。
    let config = group_config().replace(r#"memory = "m-helper""#, r#"memory = "m-main""#);
    let (mut bridge, _sends) = group_bridge(&config);

    bridge
        .ingest(message("qq:group/1", "10001", "在吗", true))
        .expect("能记下来");
    let first = bridge.poll_turn_for(&group()).expect("能取").expect("有活");
    assert_eq!(first.device, "main");

    // 另一台设备也有活，但那份记忆正忙——不能开第二轮。
    bridge
        .ingest(message("qq:group/2", "10001", "这边也有事", true))
        .expect("能记下来");
    assert!(
        bridge.poll_turn_for(&group()).expect("能取").is_none(),
        "同一份记忆同一时刻只能跑一轮"
    );

    bridge.end_turn(&first.ticket).expect("能收工");
    let second = bridge
        .poll_turn_for(&group())
        .expect("能取")
        .expect("排队的那轮现在能跑");
    assert_eq!(second.device, "helper");
}
