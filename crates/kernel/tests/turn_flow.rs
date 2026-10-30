//! 端到端：一条入站消息进来，到 agent 回话、确认水位，整条链路走一遍。
//!
//! 这里用内存存储 + 一个记账用的出站通道，把 harness 那侧换成直接调桥的方法——
//! 链路本身和真接 harness 时一模一样：票、工具、水位都是真的。

use std::sync::{Arc, Mutex};

use acb_kernel::{
    Bridge, Cursor, Element, ErrorCode, Feedbacks, Ingest, MemoryStore, NoteKey, NoteKind, Notes, Outbound,
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

/// 一次接入代表的会话组（测试里就一个）。
fn sessions() -> Vec<String> {
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

        [[sessions.subscriptions]]
        connection = "dc"
        room = "channel/456"

        [sessions.subscriptions.wake]
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
                role: None,
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
                role: None,
        },
        AT,
        "poke",
        vec![Element::text("[拍了拍我]")],
        at_self,
    )
}

fn cursor(session: &str, room: &str) -> Cursor {
    Cursor::new(session, room_of(room))
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
    assert_eq!(delivery.session, "main");
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
            &sessions(),
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
        .tool_call(&ticket, &sessions(), "ack_messages", json!({"ids": [1]}))
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
    bridge.end_turn(&ticket, None).expect("能收工");
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
            &sessions(),
            "send_message",
            json!({"elements": [{"type": "text", "text": "在"}]}),
        )
        .expect("能发");

    // 故意不确认第一条就收工——水位被挡住，正是「自己的话会不会被当成未读」的极端情况。
    bridge.end_turn(&first, None).expect("能收工");
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

    bridge.end_turn(&first.ticket, None).expect("能收工");

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
    bridge.end_turn(&first.ticket, None).expect("能收工");

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
            &sessions(),
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
            &sessions(),
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
        .tool_call(&ticket, &sessions(), "ack_messages", json!({"ids": [99]}))
        .expect_err("没投过的编号不能确认");
    assert_eq!(error.code, ErrorCode::BadRequest);

    // 投过的那两条就能确认。
    bridge
        .tool_call(&ticket, &sessions(), "ack_messages", json!({"ids": [1, 2]}))
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
    bridge.end_turn(&ticket, None).expect("能收工");

    let error = bridge
        .tool_call(
            &ticket,
            &sessions(),
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
        .tool_call(
            &ticket,
            &sessions(),
            "run_shell",
            json!({"cmd": "rm -rf /"}),
        )
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
        .tool_call(&ticket, &sessions(), "get_history", json!({}))
        .expect("能查");
    assert_eq!(history["records"].as_array().expect("是数组").len(), 2);

    let events = bridge
        .tool_call(&ticket, &sessions(), "get_platform_events", json!({}))
        .expect("能查");
    let events = events["events"].as_array().expect("是数组");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["kind"]["event"], "poke");

    let rooms = bridge
        .tool_call(&ticket, &sessions(), "list_rooms", json!({}))
        .expect("能查");
    assert_eq!(rooms["rooms"].as_array().expect("是数组").len(), 2);
    assert_eq!(rooms["rooms"][0]["room"]["connection"], "qq");
}

/// 记录里的 `columns` 状态表要用布尔，不能是「查了 / 没查」这种描述文字。
///
/// 那是给机器判断用的字段：布尔能直接当条件用，中文值只能让调用方再猜一遍。
#[test]
fn record_column_status_is_boolean_not_wording() {
    let (mut bridge, _sends) = bridge();
    bridge
        .ingest(message("qq:group/123", "10001", "在吗", true))
        .expect("能记下来");
    let ticket = bridge.poll_turn().expect("能开轮").expect("有活").ticket;

    let history = bridge
        .tool_call(&ticket, &sessions(), "get_history", json!({}))
        .expect("能查");

    let records = history["records"].as_array().expect("是数组");
    assert_eq!(records.len(), 1);
    let status = records[0]["columns"].as_object().expect("每条记录带列状态");

    // 默认列被取了 → true；没要的列 → false。全是布尔，没有一种是字符串。
    assert_eq!(status.get("id"), Some(&json!(true)));
    assert_eq!(status.get("elements"), Some(&json!(true)));
    assert_eq!(status.get("kind"), Some(&json!(false)));
    assert_eq!(status.get("platform_id"), Some(&json!(false)));
    for (column, value) in status {
        assert!(
            value.is_boolean(),
            "{column} 的状态该是布尔，实际是 {value}"
        );
    }

    // 取了的列照旧在记录上；没取的列不出现（状态表说了没取）。
    assert!(records[0].get("id").is_some());
    assert!(records[0].get("kind").is_none());
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
            &sessions(),
            "set_note",
            json!({"kind": "term", "key": "部署", "value": "蓝绿发布"}),
        )
        .expect("能写");

    let mine = bridge
        .tool_call(&ticket, &sessions(), "get_notes", json!({}))
        .expect("能读");
    assert_eq!(mine["notes"].as_array().expect("是数组").len(), 1);

    let other = bridge
        .tool_call(
            &ticket,
            &sessions(),
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
            &sessions(),
            "send_message",
            json!({"elements": [{"type": "text", "text": "在"}]}),
        )
        .expect("能发");
    bridge
        .tool_call(&ticket, &sessions(), "ack_messages", json!({"ids": [1]}))
        .expect("能确认");
    bridge.end_turn(&ticket, None).expect("能收尾");

    // 2. 对方「引用」我那条再说一句，既没 @ 也没叫名字，条数更不到 50。
    let quoted = RecordDraft::message(
        room("qq:group/123"),
        Sender {
            id: "10001".into(),
            name: "老王".into(),
                role: None,
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
                role: None,
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
        token = "t-shared"

        [[sessions.subscriptions]]
        connection = "qq"
        room = "group/1"

        [sessions.subscriptions.wake]
        at_mention = true
        name_mention = false
        poke = false
        accumulate = 50

        [[sessions]]
        id = "helper"
        harness = "dsh"
        persona = "p"
        token = "t-shared"

        [[sessions.subscriptions]]
        connection = "qq"
        room = "group/2"

        [sessions.subscriptions.wake]
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

/// 一个 harness 接管的两个会话。
fn group() -> Vec<String> {
    vec!["main".to_owned(), "helper".to_owned()]
}

#[test]
fn a_session_can_only_send_to_rooms_it_subscribes() {
    let (mut bridge, sends) = group_bridge(group_config());

    // 主会话被叫醒，领到一轮。
    bridge
        .ingest(message("qq:group/1", "10001", "在吗", true))
        .expect("能记下来");
    let delivery = bridge.poll_turn_for(&group()).expect("能取").expect("有活");
    assert_eq!(delivery.session, "main");

    // 发到本会话订阅的群 1：可以。
    bridge
        .tool_call(
            &delivery.ticket,
            &group(),
            "send_message_to",
            json!({
                "to": "qq:group/1",
                "elements": [{"type": "text", "text": "接着说"}]
            }),
        )
        .expect("本会话订阅的房间能发");

    // 发到助手会话订阅的群 2：跨会话发送已砍，拒绝。
    let error = bridge
        .tool_call(
            &delivery.ticket,
            &group(),
            "send_message_to",
            json!({
                "to": "qq:group/2",
                "elements": [{"type": "text", "text": "去别的会话的群说"}]
            }),
        )
        .expect_err("跨会话必须拒");
    assert_eq!(error.code, ErrorCode::RoomNotSubscribed);

    let sends = sends.lock().expect("没被毒死");
    assert_eq!(sends.len(), 1);
    assert_eq!(sends[0].0, room("qq:group/1"));
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
fn reading_another_sessions_room_is_allowed_inside_the_group() {
    let (mut bridge, _sends) = group_bridge(group_config());

    // 助手那个群的流水：主会话这一轮里读得到（「get from」——读可跨到组内别的房间）。
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
fn different_sessions_run_turns_independently() {
    // 串行键是「harness 名 + 会话 id」，两个会话各跑各的，不再因为共用记忆而排队。
    let (mut bridge, _sends) = group_bridge(group_config());

    bridge
        .ingest(message("qq:group/1", "10001", "在吗", true))
        .expect("能记下来");
    let first = bridge.poll_turn_for(&group()).expect("能取").expect("有活");
    assert_eq!(first.session, "main");

    // 另一个会话也有活，且主会话还没收工——不同会话能各自开轮，不排队。
    bridge
        .ingest(message("qq:group/2", "10001", "这边也有事", true))
        .expect("能记下来");
    let second = bridge
        .poll_turn_for(&group())
        .expect("能取")
        .expect("不同会话能并发开轮");
    assert_eq!(second.session, "helper");
}

// ---------------------------------------------------------------------------
// 失败通知（on_turn_error）
// ---------------------------------------------------------------------------

/// 一个会话没配 on_turn_error，只有全局默认；失败后应回退到全局默认去发通知。
fn fallback_config() -> &'static str {
    r#"
        default_on_turn_error = ["send:qq:group/ops"]

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
        at_mention = true
        name_mention = false
        poke = false
        accumulate = 50
    "#
}

/// 会话没配 on_turn_error（空），失败时必须回退到全局默认——不能因为「会话存在」
/// 就拿到一个空列表而什么都不发。
#[test]
fn turn_failure_falls_back_to_default_when_session_has_no_override() {
    let (mut bridge, sends) = group_bridge(fallback_config());

    bridge
        .ingest(message("qq:group/123", "10001", "在吗", true))
        .expect("能记下来");
    let delivery = bridge.poll_turn_for(&group()).expect("能取").expect("有活");

    // 收工时报错，触发失败通知。
    bridge
        .end_turn(&delivery.ticket, Some("模型 400 content-blocked"))
        .expect("能收工");

    let sends = sends.lock().expect("没被毒死");
    assert_eq!(sends.len(), 1, "全局默认的失败通知要真的发出去");
    assert_eq!(sends[0].0, room("qq:group/ops"), "发到全局默认指定的房间");
    let text = match &sends[0].1[0] {
        Element::Text { text } => text,
        other => panic!("失败通知应是文本，收到 {other:?}"),
    };
    assert!(text.contains("一轮失败"), "要带「一轮失败」字样：{text}");
    assert!(text.contains("content-blocked"), "要带原始错误：{text}");
}

/// 会话显式配了 on_turn_error 时，用会话自己的，不用全局默认。
#[test]
fn turn_failure_prefers_session_override_over_default() {
    let config = r#"
        default_on_turn_error = ["send:qq:group/ops"]

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
        on_turn_error = ["send:qq:group/own"]

        [[sessions.subscriptions]]
        connection = "qq"
        room = "group/123"

        [sessions.subscriptions.wake]
        at_mention = true
        name_mention = false
        poke = false
        accumulate = 50
    "#;

    let (mut bridge, sends) = group_bridge(config);

    bridge
        .ingest(message("qq:group/123", "10001", "在吗", true))
        .expect("能记下来");
    let delivery = bridge.poll_turn_for(&group()).expect("能取").expect("有活");
    bridge
        .end_turn(&delivery.ticket, Some("boom"))
        .expect("能收工");

    let sends = sends.lock().expect("没被毒死");
    assert_eq!(sends.len(), 1, "只发会话自己配的那一个");
    assert_eq!(sends[0].0, room("qq:group/own"), "会话级配置优先");
}

// ---------------------------------------------------------------------------
// 确认要幂等、票不能一收工就死（ADR-0010 修订）、反馈通道（ADR-0021）
// ---------------------------------------------------------------------------

/// 重复确认不算错。以前它会报 bad_request，模型以为「确认失败」，于是不再确认，
/// 那几条就被每轮重投一遍——真机上同一条消息被投了 6 次。
#[test]
fn acking_twice_is_not_an_error() {
    let (mut bridge, _sends) = bridge();
    bridge
        .ingest(message("qq:group/123", "10001", "在吗", true))
        .expect("能记下来");
    let ticket = bridge.poll_turn().expect("能开轮").expect("有活").ticket;

    let first = bridge
        .tool_call(&ticket, &sessions(), "ack_messages", json!({"ids": [1]}))
        .expect("第一次确认");
    assert_eq!(first["watermark"], 1);
    assert_eq!(first["acked"], json!([1]));

    let again = bridge
        .tool_call(&ticket, &sessions(), "ack_messages", json!({"ids": [1]}))
        .expect("重复确认不该报错");
    assert_eq!(again["watermark"], 1);
    assert_eq!(again["acked"], json!([]));
    assert_eq!(again["already"], json!([1]));
}

/// 收工之后的 60 秒内还能补确认——模型经常在轮末才想起来。
#[test]
fn ack_still_works_right_after_the_turn_ends() {
    let (mut bridge, _sends) = bridge();
    bridge
        .ingest(message("qq:group/123", "10001", "在吗", true))
        .expect("能记下来");
    let ticket = bridge.poll_turn().expect("能开轮").expect("有活").ticket;
    bridge.end_turn(&ticket, None).expect("能收工");

    let result = bridge
        .tool_call(&ticket, &sessions(), "ack_messages", json!({"ids": [1]}))
        .expect("宽限窗内应当能确认");
    assert_eq!(result["watermark"], 1);
    assert_eq!(
        bridge
            .store()
            .watermark(&cursor("main", "qq:group/123"))
            .expect("可读"),
        1
    );
}

/// 收工后调别的工具：错误信息必须说清「这一轮不会再有工具可用，请结束本轮」。
#[test]
fn tools_after_the_turn_say_the_round_is_over() {
    let (mut bridge, _sends) = bridge();
    bridge
        .ingest(message("qq:group/123", "10001", "在吗", true))
        .expect("能记下来");
    let ticket = bridge.poll_turn().expect("能开轮").expect("有活").ticket;
    bridge.end_turn(&ticket, None).expect("能收工");

    let error = bridge
        .tool_call(&ticket, &sessions(), "get_history", json!({}))
        .expect_err("轮次结束就不该再开工具");
    assert_eq!(error.code, ErrorCode::Unauthorized);
    assert!(error.message.contains("[no_active_round]"), "{}", error.message);
    assert!(
        error.message.contains("请结束本轮"),
        "要让 agent 收手：{}",
        error.message
    );
}

/// 发消息的回执要像回执：ok / at / room 都在，failed 时根本拿不到这些字段。
#[test]
fn send_receipt_says_ok_and_when() {
    let (mut bridge, _sends) = bridge();
    bridge
        .ingest(message("qq:group/123", "10001", "在吗", true))
        .expect("能记下来");
    let ticket = bridge.poll_turn().expect("能开轮").expect("有活").ticket;

    let receipt = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "send_message",
            json!({"elements": [{"type": "text", "text": "在的"}]}),
        )
        .expect("能发");
    assert_eq!(receipt["ok"], json!(true));
    assert_eq!(receipt["room"], "qq:group/123");
    assert!(receipt["platform_id"].as_str().is_some());
    assert!(receipt["at"].as_i64().is_some());
}

/// 反馈：正文一段、贴出处、标次数。重复提不会新建，只把次数加上去。
#[test]
fn feedback_is_recorded_counted_and_deduped() {
    let (mut bridge, _sends) = bridge();
    bridge
        .ingest(message("qq:group/123", "10001", "在吗", true))
        .expect("能记下来");
    let ticket = bridge.poll_turn().expect("能开轮").expect("有活").ticket;

    let first = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "report_feedback",
            json!({
                "text": "群成员列表只回来 16 个，没有 total，我数不出 15 个管理员是不是全部\n真正的群里 24 人。",
                "refs": ["#620", "group.get_member_list"],
                "count": 3,
                "kind": "gap",
                "suggestion": "列表统一带 total / has_more",
            }),
        )
        .expect("能记反馈");
    assert_eq!(first["ok"], json!(true));
    assert_eq!(first["duplicate"], json!(false));
    assert_eq!(first["occurrences"], 3);
    let id = first["id"].as_i64().expect("有编号");

    let again = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "report_feedback",
            json!({
                "text": "群成员列表只回来 16 个，没有 total，我数不出 15 个管理员是不是全部\n又撞到了。",
                "count": 2,
            }),
        )
        .expect("能再记一次");
    assert_eq!(again["duplicate"], json!(true), "同一个标题算同一条");
    assert_eq!(again["id"].as_i64(), Some(id), "id 不变");
    assert_eq!(again["repeats"], 2);
    assert_eq!(again["occurrences"], 5, "自称次数相加");

    let stored = bridge.store().feedback(None, 10).expect("能读");
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].title, "群成员列表只回来 16 个，没有 total，我数不出 15 个管理员是不是全部");
    assert_eq!(stored[0].refs, vec!["#620", "group.get_member_list"]);
    assert_eq!(stored[0].kind, "gap");
    assert_eq!(stored[0].severity, "major", "gap 默认 major");
    assert_eq!(stored[0].session, "main");
    assert_eq!(stored[0].harness.as_deref(), Some("dsh"));
    assert_eq!(stored[0].room.as_deref(), Some("qq:group/123"));
    assert_eq!(stored[0].status, "open");
    assert_eq!(stored[0].context["turn_running"], true);
}

/// 收工后还能写反馈：最想说话的时刻，往往就是「这一轮什么都干不成」之后。
#[test]
fn feedback_survives_the_turn_end() {
    let (mut bridge, _sends) = bridge();
    bridge
        .ingest(message("qq:group/123", "10001", "在吗", true))
        .expect("能记下来");
    let ticket = bridge.poll_turn().expect("能开轮").expect("有活").ticket;
    bridge.end_turn(&ticket, None).expect("能收工");

    let result = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "report_feedback",
            json!({"text": "这一轮什么工具都调不动，只能干看着", "count": 6}),
        )
        .expect("收工后也该能写反馈");
    assert_eq!(result["ok"], json!(true));
    let stored = bridge.store().feedback(Some("open"), 10).expect("能读");
    assert_eq!(stored[0].occurrences, 6);
    assert_eq!(stored[0].context["turn_running"], false);
}

/// 空正文要拒；写错的 kind 要指出合法值。
#[test]
fn feedback_rejects_empty_text_and_unknown_kind() {
    let (mut bridge, _sends) = bridge();
    bridge
        .ingest(message("qq:group/123", "10001", "在吗", true))
        .expect("能记下来");
    let ticket = bridge.poll_turn().expect("能开轮").expect("有活").ticket;

    let empty = bridge
        .tool_call(&ticket, &sessions(), "report_feedback", json!({"text": "   "}))
        .expect_err("空正文要拒");
    assert_eq!(empty.code, ErrorCode::BadRequest);

    let bad = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "report_feedback",
            json!({"text": "x", "kind": "complaint"}),
        )
        .expect_err("不认识的类型要拒");
    assert!(bad.message.contains("friction"), "{}", bad.message);
}

/// 查历史要给总数：不然「还有没有更早的」只能靠猜。
#[test]
fn history_reports_total_and_how_to_go_further_back() {
    let (mut bridge, _sends) = bridge();
    for index in 0..5 {
        bridge
            .ingest(message(
                "qq:group/123",
                "10001",
                &format!("第 {index} 条"),
                true,
            ))
            .expect("能记下来");
    }
    let ticket = bridge.poll_turn().expect("能开轮").expect("有活").ticket;

    let history = bridge
        .tool_call(&ticket, &sessions(), "get_history", json!({"limit": 2}))
        .expect("能查");
    assert_eq!(history["total"], 5);
    assert_eq!(history["returned"], 2);
    assert_eq!(history["has_more"], true);
    let next_before = history["next_before"].as_u64().expect("给往回翻的锚点");
    assert_eq!(next_before, 4, "最新两条是 4、5");

    let older = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "get_history",
            json!({"limit": 10, "before": next_before}),
        )
        .expect("能接着往前翻");
    assert_eq!(older["total"], 3);
    assert_eq!(older["has_more"], false);
}

// ---------------------------------------------------------------------------
// 引用对齐（ADR-0024）
// ---------------------------------------------------------------------------

/// 引用消息要把平台编号翻成本房间编号：两个号并排给，模型不用自己对齐。
#[test]
fn quotes_carry_the_local_watermark_number() {
    let (mut bridge, _sends) = bridge();

    // 第一条：原话，别人说的。第二条：引用它。
    bridge
        .ingest(message("qq:group/123", "10001", "只发子网", false).with_platform_id("p-100"))
        .expect("能记下来");
    let reply = RecordDraft::message(
        room_of("qq:group/123"),
        Sender {
            id: "10002".to_owned(),
            name: "小李".to_owned(),
            role: None,
        },
        AT,
        vec![
            Element::Quote {
                message_id: "p-100".to_owned(),
            },
            Element::text("对的对的"),
        ],
        true,
    );
    bridge
        .ingest(reply.with_platform_id("p-101"))
        .expect("能记下来");

    let delivery = bridge.poll_turn().expect("能开轮").expect("有活");
    assert_eq!(delivery.records.len(), 2);

    let quoted = &delivery.records[1];
    let quote_ref = quoted.quote_ref.as_ref().expect("引用了就该有 quote_ref");
    assert_eq!(quote_ref.platform_id, "p-100");
    assert_eq!(quote_ref.id, Some(1), "翻成的是本房间的 #1");
    assert_eq!(quoted.quote_of(), Some("p-100"));
}

/// 引用一条本房间没有的消息（被清理了、或者别的房间的）：只能给平台编号，
/// 不能编一个编号出来。
#[test]
fn a_quote_outside_the_room_has_no_local_id() {
    let (mut bridge, _sends) = bridge();
    let reply = RecordDraft::message(
        room_of("qq:group/123"),
        Sender {
            id: "10002".to_owned(),
            name: "小李".to_owned(),
            role: None,
        },
        AT,
        vec![Element::Quote {
            message_id: "gone".to_owned(),
        }],
        true,
    );
    bridge
        .ingest(reply.with_platform_id("p-1"))
        .expect("能记下来");

    let delivery = bridge.poll_turn().expect("能开轮").expect("有活");
    let quote_ref = delivery.records[0]
        .quote_ref
        .as_ref()
        .expect("查不到也要留个说法");
    assert_eq!(quote_ref.platform_id, "gone");
    assert_eq!(quote_ref.id, None);
}

/// get_history 里也能专门要这一列：不对齐的时候不用再手算。
#[test]
fn history_can_return_quote_ref_as_a_column() {
    let (mut bridge, _sends) = bridge();
    bridge
        .ingest(message("qq:group/123", "10001", "只发子网", false).with_platform_id("p-100"))
        .expect("能记下来");
    let reply = RecordDraft::message(
        room_of("qq:group/123"),
        Sender {
            id: "10002".to_owned(),
            name: "小李".to_owned(),
            role: None,
        },
        AT,
        vec![Element::Quote {
            message_id: "p-100".to_owned(),
        }],
        true,
    );
    bridge
        .ingest(reply.with_platform_id("p-101"))
        .expect("能记下来");
    let ticket = bridge.poll_turn().expect("能开轮").expect("有活").ticket;

    let history = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "get_history",
            json!({"columns": ["id", "quote_ref"]}),
        )
        .expect("能查");
    let records = history["records"].as_array().expect("有记录");
    assert_eq!(records[0]["quote_ref"], serde_json::Value::Null, "没引用就是空");
    assert_eq!(records[1]["quote_ref"]["platform_id"], "p-100");
    assert_eq!(records[1]["quote_ref"]["id"], 1);
    // 状态表照样如实标：别的列这次没取。
    assert_eq!(records[1]["columns"]["quote_ref"], true);
    assert_eq!(records[1]["columns"]["elements"], false);
}
