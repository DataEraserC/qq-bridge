//! 端到端：一条入站消息进来，到 agent 回话、确认水位，整条链路走一遍。
//!
//! 这里用内存存储 + 一个记账用的出站通道，把 harness 那侧换成直接调桥的方法——
//! 链路本身和真接 harness 时一模一样：票、工具、水位都是真的。

use std::sync::{Arc, Mutex};

use acb_kernel::{
    Bridge, Capability, Cursor, DeliveryComposer, DeliveryPlan, DeliveryPolicy, DeliveryRequest,
    Element, ErrorCode, ExtensionPoint, Feedbacks, Ingest, ManagerManifest, ManagerPlugin,
    MemoryStore, NoteKey, NoteKind, Notes, Omitted, Outbound, OutboundError, PluginError,
    PluginManager, ProviderDescriptor, ProviderId, ProviderInstance, ProviderOptions, ReceiveDecision,
    ReceiveProvider, ReceiveRequest, RecordDraft, RoomId, SendContext, SendHook, Sender, Sent,
    Transcript, TurnErrorAction, TurnErrorProvider,
    TurnErrorRequest, TurnReason, Watermarks, unread, PLUGIN_API_VERSION,
};
use serde_json::{Value, json};

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

/// 带访问策略的桥。`rooms` 只有 `elevated_in_rooms` 用得到。
fn bridge_with_access(provider: &str, rooms: &[&str]) -> (Bridge<MemoryStore>, Sends) {
    let rooms = rooms
        .iter()
        .map(|room| format!("\"{room}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let access = format!(
        "        [sessions.access]\n        provider = \"{provider}\"\n        rooms = [{rooms}]"
    );
    let text = config().replacen(
        "        [[sessions.subscriptions]]",
        &format!("{access}\n\n        [[sessions.subscriptions]]"),
        1,
    );
    let sends: Sends = Arc::new(Mutex::new(Vec::new()));
    let bridge = Bridge::new(
        acb_kernel::Config::parse(&text).expect("配置合法"),
        MemoryStore::new(),
        Box::new(Recorder {
            sends: sends.clone(),
        }),
    );
    (bridge, sends)
}

/// 在会话订阅之前插一段配置（例如 `[sessions.delivery]` / `[sessions.outbound]`）。
fn config_with(insert: &str) -> String {
    config().replacen(
        "        [[sessions.subscriptions]]",
        &format!("{insert}\n\n        [[sessions.subscriptions]]"),
        1,
    )
}

/// 带自定义扩展宿主的桥。
fn bridge_with_plugins(text: &str, plugins: PluginManager) -> (Bridge<MemoryStore>, Sends) {
    let sends: Sends = Arc::new(Mutex::new(Vec::new()));
    let bridge = Bridge::with_plugins(
        acb_kernel::Config::parse(text).expect("配置合法"),
        MemoryStore::new(),
        Box::new(Recorder {
            sends: sends.clone(),
        }),
        plugins,
    );
    (bridge, sends)
}

/// 只投最新一条的 composer。
struct LatestOnly;

impl DeliveryComposer for LatestOnly {
    fn compose(&self, _options: &DeliveryPolicy, request: &DeliveryRequest<'_>) -> DeliveryPlan {
        let mut keep = Vec::new();
        if let Some(last) = request.records.last() {
            keep.push(last.id);
        }
        // 故意塞一个不存在的编号：core 必须忽略它，不能让水位出错。
        keep.push(9999);
        DeliveryPlan {
            keep,
            omitted: None,
        }
    }
}

struct LatestOnlyManager;

impl ManagerPlugin for LatestOnlyManager {
    fn manifest(&self) -> ManagerManifest {
        ManagerManifest::new("test", PLUGIN_API_VERSION, vec![Capability::DeliverPlan])
    }

    fn descriptors(&self) -> Vec<ProviderDescriptor> {
        vec![ProviderDescriptor::new(
            ProviderId::parse("test.latest").expect("合法"),
            ExtensionPoint::Delivery,
            PLUGIN_API_VERSION,
            vec![Capability::DeliverPlan],
        )]
    }

    fn instantiate(
        &self,
        id: &ProviderId,
        _options: ProviderOptions<'_>,
    ) -> Result<ProviderInstance, PluginError> {
        if id.name() == "latest" {
            Ok(ProviderInstance::Delivery(Arc::new(LatestOnly)))
        } else {
            Err(PluginError::UnknownProvider(id.to_string()))
        }
    }
}

/// 发送前加签名、发送后记账的钩子。
#[derive(Default)]
struct SignHook {
    after: Arc<Mutex<Vec<String>>>,
}

impl SendHook for SignHook {
    fn before_send(
        &self,
        _context: &SendContext<'_>,
        elements: &mut Vec<Element>,
    ) -> Result<(), PluginError> {
        elements.push(Element::text(" [签名]"));
        Ok(())
    }

    fn after_send(&self, context: &SendContext<'_>, sent: &Sent) {
        self.after
            .lock()
            .expect("没被毒死")
            .push(format!("{}:{}", context.session, sent.platform_id));
    }
}

struct SignHookManager {
    hook: Arc<SignHook>,
}

impl ManagerPlugin for SignHookManager {
    fn manifest(&self) -> ManagerManifest {
        ManagerManifest::new("hook", PLUGIN_API_VERSION, vec![Capability::SendHook])
    }

    fn descriptors(&self) -> Vec<ProviderDescriptor> {
        vec![ProviderDescriptor::new(
            ProviderId::parse("hook.sign").expect("合法"),
            ExtensionPoint::Send,
            PLUGIN_API_VERSION,
            vec![Capability::SendHook],
        )]
    }

    fn instantiate(
        &self,
        id: &ProviderId,
        _options: ProviderOptions<'_>,
    ) -> Result<ProviderInstance, PluginError> {
        if id.name() == "sign" {
            Ok(ProviderInstance::Send(self.hook.clone()))
        } else {
            Err(PluginError::UnknownProvider(id.to_string()))
        }
    }
}

/// 无论配置怎么写，都只发到 `qq:group/ops` 的失败通知 provider。
struct RedirectTurnError;

impl TurnErrorProvider for RedirectTurnError {
    fn decide(&self, _request: &TurnErrorRequest<'_>) -> Vec<TurnErrorAction> {
        vec![TurnErrorAction::Send {
            room: room("qq:group/ops"),
        }]
    }
}

struct RedirectTurnErrorManager;

impl ManagerPlugin for RedirectTurnErrorManager {
    fn manifest(&self) -> ManagerManifest {
        ManagerManifest::new(
            "test",
            PLUGIN_API_VERSION,
            vec![Capability::TurnErrorDecision],
        )
    }

    fn descriptors(&self) -> Vec<ProviderDescriptor> {
        vec![ProviderDescriptor::new(
            ProviderId::parse("test.failure").expect("合法"),
            ExtensionPoint::TurnError,
            PLUGIN_API_VERSION,
            vec![Capability::TurnErrorDecision],
        )]
    }

    fn instantiate(
        &self,
        id: &ProviderId,
        _options: ProviderOptions<'_>,
    ) -> Result<ProviderInstance, PluginError> {
        if id.name() == "failure" {
            Ok(ProviderInstance::TurnError(Arc::new(RedirectTurnError)))
        } else {
            Err(PluginError::UnknownProvider(id.to_string()))
        }
    }
}

/// 丢掉正文含「广告」的入站记录。
struct DropAds;

impl ReceiveProvider for DropAds {
    fn decide(&self, _options: &acb_kernel::ReceivePolicy, request: &ReceiveRequest<'_>) -> ReceiveDecision {
        if request.text.contains("广告") {
            ReceiveDecision::Drop
        } else {
            ReceiveDecision::Record
        }
    }
}

struct DropAdsManager;

impl ManagerPlugin for DropAdsManager {
    fn manifest(&self) -> ManagerManifest {
        ManagerManifest::new(
            "test",
            PLUGIN_API_VERSION,
            vec![Capability::ReceiveDecision],
        )
    }

    fn descriptors(&self) -> Vec<ProviderDescriptor> {
        vec![ProviderDescriptor::new(
            ProviderId::parse("test.drop").expect("合法"),
            ExtensionPoint::Receive,
            PLUGIN_API_VERSION,
            vec![Capability::ReceiveDecision],
        )]
    }

    fn instantiate(
        &self,
        id: &ProviderId,
        _options: ProviderOptions<'_>,
    ) -> Result<ProviderInstance, PluginError> {
        if id.name() == "drop" {
            Ok(ProviderInstance::Receive(Arc::new(DropAds)))
        } else {
            Err(PluginError::UnknownProvider(id.to_string()))
        }
    }
}

/// 在最后一个订阅上挂 60 秒固定间隔时钟。
fn clock_config() -> String {
    config().replacen(
        "        accumulate = 50",
        "        accumulate = 50\n\n        [sessions.subscriptions.clock]\n        interval_secs = 60",
        1,
    )
}

/// 单轮投递条数上限改小的桥。
fn bridge_with_limit(limit: usize) -> (Bridge<MemoryStore>, Sends) {
    let text = format!("delivery_limit = {limit}\n{}", config());
    let sends: Sends = Arc::new(Mutex::new(Vec::new()));
    let bridge = Bridge::new(
        acb_kernel::Config::parse(&text).expect("配置合法"),
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
    assert_eq!(delivery.wake_reason.as_deref(), Some("@ 了我"));
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

/// 乱序确认过的记录不能重投：本轮只确认 #2、留下 #1，下一轮只该看到 #1 和新消息。
#[test]
fn out_of_order_acks_are_not_delivered_again() {
    let (mut bridge, _sends) = bridge();
    bridge
        .ingest(message("qq:group/123", "10001", "第一句", true))
        .expect("能记下来");
    bridge
        .ingest(message("qq:group/123", "10001", "第二句", true))
        .expect("能记下来");
    let first = bridge.poll_turn().expect("能开轮").expect("有活");
    assert_eq!(first.records.len(), 2);

    // 只确认后面那条：水位被 #1 挡着，但 #2 已经处理完了。
    bridge
        .tool_call(&first.ticket, &sessions(), "ack_messages", json!({"ids": [2]}))
        .expect("能确认");
    bridge.end_turn(&first.ticket, None).expect("能收工");

    bridge
        .ingest(message("qq:group/123", "10001", "第三句", true))
        .expect("能记下来");
    let second = bridge.poll_turn().expect("能开轮").expect("有活");
    let ids: Vec<u64> = second.records.iter().map(|record| record.id).collect();
    assert_eq!(ids, vec![1, 3], "#2 已经确认过，不能再投一遍");
    assert_eq!(second.records[0].redelivery, Some(2), "#1 仍然要标重投");
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

/// `all_rooms`：整个会话豁免订阅边界，可以发到任意房间。
#[test]
fn all_rooms_access_policy_reaches_unsubscribed_rooms() {
    let (mut bridge, sends) = bridge_with_access("acb.access.all_rooms", &[]);
    bridge
        .ingest(message("qq:group/123", "10001", "在吗", true))
        .expect("能记下来");
    let ticket = bridge.poll_turn().expect("能开轮").expect("有活").ticket;

    let result = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "send_message_to",
            json!({
                "to": "qq:group/999",
                "elements": [{"type": "text", "text": "发得出去"}]
            }),
        )
        .expect("全豁免策略下，没订阅的房间也能发");
    assert_eq!(result["room"], "qq:group/999");
    assert_eq!(sends.lock().expect("没被毒死")[0].0, room("qq:group/999"));
}

/// `elevated_in_rooms`：只开列表里的那个聊天（读和写都开），其他照旧。
#[test]
fn elevated_access_opens_only_the_listed_rooms() {
    let (mut bridge, sends) = bridge_with_access("acb.access.elevated_in_rooms", &["qq:group/999"]);
    bridge
        .ingest(message("qq:group/123", "10001", "在吗", true))
        .expect("能记下来");
    let ticket = bridge.poll_turn().expect("能开轮").expect("有活").ticket;

    bridge
        .tool_call(
            &ticket,
            &sessions(),
            "send_message_to",
            json!({
                "to": "qq:group/999",
                "elements": [{"type": "text", "text": "去豁免的群说一句"}]
            }),
        )
        .expect("列表里的房间能发");
    assert_eq!(sends.lock().expect("没被毒死")[0].0, room("qq:group/999"));

    // 读也开：不用订阅就能看那个房间的流水。
    bridge
        .tool_call(
            &ticket,
            &sessions(),
            "get_history",
            json!({"room": "qq:group/999"}),
        )
        .expect("列表里的房间能读");

    // 列表外的房间仍然拒。
    let error = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "send_message_to",
            json!({
                "to": "qq:group/888",
                "elements": [{"type": "text", "text": "发不出去"}]
            }),
        )
        .expect_err("没列出来的房间照旧拒");
    assert_eq!(error.code, ErrorCode::RoomNotSubscribed);
}

/// 热重载只影响下一轮：这一轮开轮后把 qq 订阅删掉，本轮已开的票仍按快照放行。
#[test]
fn active_turn_keeps_config_snapshot_after_reload() {
    let (mut bridge, sends) = bridge();
    bridge
        .ingest(message("qq:group/123", "10001", "在吗", true))
        .expect("能记下来");
    let ticket = bridge.poll_turn().expect("能开轮").expect("有活").ticket;

    // 新配置里没有 qq:group/123 这条订阅了。
    let reloaded = config().replace(
        r#"        [[sessions.subscriptions]]
        connection = "qq"
        room = "group/123"

"#,
        "",
    );
    bridge.stage_config(acb_kernel::Config::parse(&reloaded).expect("新配置合法"));
    // ingest 是配置消费点：喂一条 dc 的已订阅消息，让 staged 配置真正换上。
    bridge
        .ingest(message("dc:channel/456", "10001", "刷新配置", true))
        .expect("能记下来");
    assert_eq!(bridge.config_version(), 1, "staged 配置已生效");

    // 当前配置已经没订阅 qq 群，但本轮的快照还有；所以这个发送仍应放行。
    bridge
        .tool_call(
            &ticket,
            &sessions(),
            "send_message_to",
            json!({
                "to": "qq:group/123",
                "elements": [{"type": "text", "text": "按本轮快照发"}]
            }),
        )
        .expect("本轮快照里仍订阅 qq 群");
    assert_eq!(
        sends.lock().expect("没被毒死")[0].0,
        room("qq:group/123"),
        "发送目标用的是本轮快照的订阅"
    );
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

/// 本轮读到但没投递的内容也能确认：补读要能把水位推着走。
#[test]
fn ack_accepts_records_read_this_turn_but_not_delivered() {
    let (mut bridge, _sends) = bridge_with_limit(1);
    for text in ["第一句", "第二句", "第三句"] {
        bridge
            .ingest(message("qq:group/123", "10001", text, true))
            .expect("能记下来");
    }
    let delivery = bridge.poll_turn().expect("能开轮").expect("有活");
    let ticket = delivery.ticket;
    let ids: Vec<u64> = delivery.records.iter().map(|record| record.id).collect();
    assert_eq!(ids, vec![3], "上限 1 条只给最新的");
    assert_eq!(
        delivery.omitted,
        Some(Omitted {
            from: 1,
            to: 2,
            count: 2
        })
    );

    // 补读跳过的两条；读到的编号进本轮 read set。
    let fetched = bridge
        .tool_call(&ticket, &sessions(), "fetch_records", json!({"ids": [1, 2]}))
        .expect("能补读");
    assert_eq!(fetched["returned"], 2);

    let acked = bridge
        .tool_call(&ticket, &sessions(), "ack_messages", json!({"ids": [1, 2]}))
        .expect("读过的编号能确认");
    assert_eq!(acked["watermark"], 2, "水位推到补读的连续前沿");

    let acked = bridge
        .tool_call(&ticket, &sessions(), "ack_messages", json!({"ids": [3]}))
        .expect("投递过的也能确认");
    assert_eq!(acked["watermark"], 3);
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

/// 票号带进程前缀：不同桥实例、以及同一实例的下一轮，票面都不会撞。
#[test]
fn tickets_are_unique_across_bridge_instances_and_turns() {
    let (mut first, _sends) = bridge();
    let (mut second, _sends) = bridge();
    first
        .ingest(message("qq:group/123", "10001", "在吗", true))
        .expect("能记下来");
    second
        .ingest(message("qq:group/123", "10001", "在吗", true))
        .expect("能记下来");
    let first_ticket = first.poll_turn().expect("能开轮").expect("有活").ticket;
    let other_ticket = second.poll_turn().expect("能开轮").expect("有活").ticket;
    assert_ne!(first_ticket, other_ticket, "不同桥实例的票不能相同");

    first.end_turn(&first_ticket, None).expect("能收工");
    first
        .ingest(message("qq:group/123", "10001", "第二句", true))
        .expect("能记下来");
    let next_ticket = first.poll_turn().expect("能开轮").expect("有活").ticket;
    assert_ne!(next_ticket, first_ticket, "同一实例的下一轮票不能相同");
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

/// 工具面按会话圈，不按连接圈。
///
/// 一条连接可以代表多个会话（同 harness 同令牌）。A 会话开轮时调 list_rooms，
/// 只该看见 A 自己订阅的房间——兄弟会话的房间列进来，agent 就会自称管着
/// 别人的房间（2026-11-01 真机实测：opencode 一条连接带两会话，agent 报 3 个房间）。
#[test]
fn tool_scope_is_the_turn_session_not_the_connection() {
    let sends: Sends = Arc::new(Mutex::new(Vec::new()));
    let config = r#"
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
        token = "t"

        [[sessions.subscriptions]]
        connection = "qq"
        room = "group/123"

        [sessions.subscriptions.wake]
        at_mention = true
        name_mention = false
        poke = false
        accumulate = 50

        [[sessions]]
        id = "sib"
        harness = "dsh"
        persona = "p"
        token = "t"

        [[sessions.subscriptions]]
        connection = "qq"
        room = "group/789"

        [sessions.subscriptions.wake]
        at_mention = true
        name_mention = false
        poke = false
        accumulate = 50
    "#;
    let mut bridge = Bridge::new(
        acb_kernel::Config::parse(config).expect("配置合法"),
        MemoryStore::new(),
        Box::new(Recorder { sends }),
    );

    // 兄弟会话 sib 的房间里有动静，开的是 sib 的轮。
    let _ = bridge
        .ingest(message("qq:group/789", "10001", "在吗", true))
        .expect("能记下来");
    let ticket = bridge.poll_turn().expect("能开轮").expect("有活").ticket;

    // 连接代表两个会话，但工具的可见域只有当前轮次会话的房间。
    let rooms = bridge
        .tool_call(
            &ticket,
            &["main".to_owned(), "sib".to_owned()],
            "list_rooms",
            json!({}),
        )
        .expect("能查");
    let listed: Vec<_> = rooms["rooms"]
        .as_array()
        .expect("是数组")
        .iter()
        .map(|r| {
            r["room"]["path"]
                .as_array()
                .expect("房间带路径")
                .iter()
                .map(|p| p.as_str().unwrap_or_default())
                .collect::<Vec<_>>()
                .join("/")
        })
        .collect();
    assert_eq!(listed.len(), 1, "只列当前会话的房间：{listed:?}");
    assert!(
        listed[0].contains("789"),
        "是 sib 的房间，不是 main 的：{listed:?}"
    );
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
/// 工具面按会话圈，不按会话组圈（2026-11-01 用户裁定，推翻旧的「组内能读」设计：
/// 一条连接挂的多个会话是各自独立的大脑——opencode 一个实例带多条独立会话，
/// 不是同一个大脑的分身，兄弟会话的房间不该出现在自己的可见域里）。
/// 组外房间照旧拒绝，这条边界原来就有，一并守着。
#[test]
fn reading_another_sessions_room_is_refused() {
    let (mut bridge, _sends) = group_bridge(group_config());

    // 兄弟会话 helper 的房间先进了流水；主会话被叫醒，领到一轮。
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

    let sibling = bridge
        .tool_call(
            &ticket,
            &group(),
            "get_history",
            json!({"room": "qq:group/2"}),
        )
        .expect_err("兄弟会话的房间也读不到");
    assert_eq!(sibling.code, ErrorCode::RoomNotSubscribed);

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
    // 通知在 end_turn 里只排队，真正发送由 endpoint 在锁外做；测试里手动发完。
    bridge.flush_notices();

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
    bridge.flush_notices();

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
    assert!(
        error.message.contains("[no_active_round]"),
        "{}",
        error.message
    );
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
    assert_eq!(
        stored[0].title,
        "群成员列表只回来 16 个，没有 total，我数不出 15 个管理员是不是全部"
    );
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
        .tool_call(
            &ticket,
            &sessions(),
            "report_feedback",
            json!({"text": "   "}),
        )
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

/// 实测过的脏数据：有的 harness 用文本流解析工具调用，标签漏进了 text 参数，
/// 结果带着 `</mcp__acb__send_message>` 和特殊 token 的话真发到群里了。
/// 桥在另一个进程拦不住解析器，但「落进参数之后」这一段必须切掉。
#[test]
fn leaked_call_markup_never_reaches_the_room() {
    let (mut bridge, sends) = bridge();
    bridge
        .ingest(message("qq:group/123", "10001", "在吗", true))
        .expect("能记下来");
    let ticket = bridge.poll_turn().expect("能开轮").expect("有活").ticket;

    let receipt = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "send_message",
            json!({"elements": [{"type": "text", "text":
                "在的，你说。</text>\n</mcp__acb__send_message>\n<｜｜DSML｜｜ calls>\n<｜｜DSML｜｜ invoke name=\"mcp__acb__platform_methods\">"
            }]}),
        )
        .expect("能发");

    assert_eq!(receipt["ok"], json!(true));
    assert_eq!(receipt["trimmed"], json!(true), "截断了就得说");
    assert!(
        receipt["note"].as_str().expect("有说明").contains("截断"),
        "{}",
        receipt["note"]
    );

    let sent = sends.lock().expect("没被毒死");
    let text = sent[0].1[0].as_text().expect("是文本段");
    assert_eq!(text, "在的，你说。", "群里只该看到正文");
}

/// 整段都是标记：切完没有正文，不该往平台上发一个空段。
#[test]
fn all_markup_text_is_refused_instead_of_sent_empty() {
    let (mut bridge, sends) = bridge();
    bridge
        .ingest(message("qq:group/123", "10001", "在吗", true))
        .expect("能记下来");
    let ticket = bridge.poll_turn().expect("能开轮").expect("有活").ticket;

    let rejected = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "send_message",
            json!({"elements": [{"type": "text", "text": "</text></mcp__acb__send_message>"}]}),
        )
        .expect_err("切完没正文就该拒");
    assert_eq!(rejected.code, ErrorCode::BadRequest);
    assert!(
        rejected.message.contains("bad_params"),
        "{}",
        rejected.message
    );
    assert!(
        sends.lock().expect("没被毒死").is_empty(),
        "一个字都不该发出去"
    );
}

/// 反馈正文同样要切：不然人翻开列表看到的是「正文 + 一串标签」。
#[test]
fn feedback_body_gets_cut_and_the_receipt_says_so() {
    let (mut bridge, _sends) = bridge();
    bridge
        .ingest(message("qq:group/123", "10001", "在吗", true))
        .expect("能记下来");
    let ticket = bridge.poll_turn().expect("能开轮").expect("有活").ticket;

    let recorded = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "report_feedback",
            json!({"text": "转发点开只有一句「对」，读不出正文。</text>\n<refs>[\"#700\"]</refs>\n<kind>gap</kind>"}),
        )
        .expect("能记反馈");

    assert_eq!(
        recorded["truncated"]["text_markup"],
        json!(true),
        "标记要被认出来"
    );
    assert!(
        recorded["note"].as_str().expect("有说明").contains("标记"),
        "{}",
        recorded["note"]
    );

    let stored = bridge.store().feedback(None, 10).expect("能读");
    assert_eq!(stored[0].body, "转发点开只有一句「对」，读不出正文。");
    assert_eq!(stored[0].title, "转发点开只有一句「对」，读不出正文。");
}

/// 合并时只补「空着的」字段：分类与严重度按 ADR-0021 保留第一次，
/// 但第一次没填（或那次参数被解析器吞了）的出处/建议/工具，后来的报告带全了要补上。
#[test]
fn merging_fills_empty_fields_but_keeps_the_first_classification() {
    let (mut bridge, _sends) = bridge();
    bridge
        .ingest(message("qq:group/123", "10001", "在吗", true))
        .expect("能记下来");
    let ticket = bridge.poll_turn().expect("能开轮").expect("有活").ticket;

    // 第一次：参数被吞，只剩正文（kind 掉回默认 friction）。
    bridge
        .tool_call(
            &ticket,
            &sessions(),
            "report_feedback",
            json!({"text": "平台方法清单里没有「撤回」这个动作名"}),
        )
        .expect("能记反馈");

    // 第二次：同一句话，参数这次带全了。
    let again = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "report_feedback",
            json!({
                "text": "平台方法清单里没有「撤回」这个动作名",
                "refs": ["#700", "platform_methods"],
                "tool": "platform_methods",
                "suggestion": "动作名跟 QQ 的 delete_msg 对齐",
                "kind": "gap",
            }),
        )
        .expect("能再记一次");
    assert_eq!(again["duplicate"], json!(true));

    let stored = bridge.store().feedback(None, 10).expect("能读");
    assert_eq!(stored[0].kind, "friction", "分类保留第一次的（ADR-0021）");
    assert_eq!(
        stored[0].refs,
        vec!["#700", "platform_methods"],
        "空着的出处补上了"
    );
    assert_eq!(
        stored[0].suggestion.as_deref(),
        Some("动作名跟 QQ 的 delete_msg 对齐"),
        "空着的建议补上了"
    );
    assert_eq!(stored[0].tool.as_deref(), Some("platform_methods"));
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

/// 给了 after 就从旧往新翻：接着 next_after 往后走，不会重复看同一条。
#[test]
fn history_after_pages_forward_without_repeating() {
    let (mut bridge, _sends) = bridge();
    for index in 1..=5 {
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

    let first = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "get_history",
            json!({"after": 0, "limit": 2}),
        )
        .expect("能往后翻");
    assert_eq!(first["total"], 5);
    assert_eq!(first["returned"], 2);
    assert_eq!(first["has_more"], true);
    assert_eq!(first["records"][0]["id"], 1);
    assert_eq!(first["records"][1]["id"], 2);
    let next_after = first["next_after"].as_u64().expect("给往后翻的锚点");
    assert_eq!(next_after, 2);

    let second = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "get_history",
            json!({"after": next_after, "limit": 2}),
        )
        .expect("能接着往后翻");
    assert_eq!(second["records"][0]["id"], 3, "不能把 #2 再给一遍");
    assert_eq!(second["records"][1]["id"], 4);
    assert_eq!(second["next_after"], 4);

    let last = bridge
        .tool_call(&ticket, &sessions(), "get_history", json!({"after": 4}))
        .expect("能翻到末尾");
    assert_eq!(last["records"][0]["id"], 5);
    assert_eq!(last["has_more"], false);
    assert_eq!(last["next_after"], Value::Null);
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
    assert_eq!(
        records[0]["quote_ref"],
        serde_json::Value::Null,
        "没引用就是空"
    );
    assert_eq!(records[1]["quote_ref"]["platform_id"], "p-100");
    assert_eq!(records[1]["quote_ref"]["id"], 1);
    // 状态表照样如实标：别的列这次没取。
    assert_eq!(records[1]["columns"]["quote_ref"], true);
    assert_eq!(records[1]["columns"]["elements"], false);
}

// ── 文件中继（ADR-0025）────────────────────────────────────────────────

/// 测试用临时目录，Drop 时整个删掉。
struct TempRelayDir(std::path::PathBuf);

impl TempRelayDir {
    fn new(name: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("时间正常")
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("acb-relay-{}-{name}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("能建临时目录");
        Self(dir)
    }
}

impl Drop for TempRelayDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn 发送时本机文件换成中继URL_别的路径不动() {
    let temp = TempRelayDir::new("send");
    let source = temp.0.join("截图 01.png");
    std::fs::write(&source, b"png-bytes").expect("能写源文件");

    // [file_relay] 是根级表，追加在配置末尾是合法 TOML。
    let text = format!(
        "{}\n[file_relay]\nlisten = \"127.0.0.1:0\"\npublic_base = \"http://10.89.0.1:3131\"\ndir = \"{}\"",
        config(),
        temp.0.join("relay").display()
    );
    let sends: Sends = Arc::new(Mutex::new(Vec::new()));
    let mut bridge = Bridge::new(
        acb_kernel::Config::parse(&text).expect("配置合法"),
        MemoryStore::new(),
        Box::new(Recorder {
            sends: sends.clone(),
        }),
    );

    bridge
        .ingest(message("qq:group/123", "10001", "发个图", true))
        .expect("能记下来");
    let ticket = bridge.poll_turn().expect("能开轮").expect("有活").ticket;

    let receipt = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "send_message",
            json!({
                "elements": [
                    {"type": "image", "url": source.display().to_string()},
                    {"type": "image", "url": "http://elsewhere.example/a.png"},
                    {"type": "text", "text": "看图"},
                ],
            }),
        )
        .expect("能发");

    assert_eq!(receipt["ok"], json!(true));
    let relayed = receipt["relayed"].as_array().expect("回执里有 relayed");
    assert_eq!(relayed.len(), 1, "只换本机存在的那一个");
    assert_eq!(relayed[0]["from"], json!(source.display().to_string()));
    let to = relayed[0]["to"].as_str().expect("有 to").to_owned();
    assert!(to.starts_with("http://10.89.0.1:3131/f/"), "{to}");

    let all = sends.lock().expect("没被毒死");
    let (_, elements) = &all[0];
    assert!(
        matches!(&elements[0], Element::Image { url, .. } if url.starts_with("http://10.89.0.1:3131/f/")),
        "发出去的应该是中继 URL"
    );
    assert!(
        matches!(&elements[1], Element::Image { url, .. } if url == "http://elsewhere.example/a.png"),
        "http 路径原样透传"
    );
    assert!(matches!(&elements[2], Element::Text { .. }));

    // 收下的文件真的落盘了，URL 里的两段就是目录名/文件名。
    let rest = to.strip_prefix("http://10.89.0.1:3131/f/").expect("前缀对");
    assert!(
        temp.0.join("relay").join(rest).is_file(),
        "文件应落在 {rest}"
    );
}

#[test]
fn 没配文件中继时本机路径原样透传() {
    let (mut bridge, sends) = bridge();
    bridge
        .ingest(message("qq:group/123", "10001", "发个图", true))
        .expect("能记下来");
    let ticket = bridge.poll_turn().expect("能开轮").expect("有活").ticket;

    bridge
        .tool_call(
            &ticket,
            &sessions(),
            "send_message",
            json!({
                "elements": [
                    {"type": "image", "url": "/home/nixos/ somewhere/a.png"},
                ],
            }),
        )
        .expect("能发");

    let all = sends.lock().expect("没被毒死");
    let (_, elements) = &all[0];
    assert!(
        matches!(&elements[0], Element::Image { url, .. } if url == "/home/nixos/ somewhere/a.png"),
        "没配 [file_relay] 就不碰路径——行为跟没有这个功能时完全一样"
    );
}

// ---------------------------------------------------------------------------
// 多订阅者下的水位（2026-10-31 真机事故回归）
// ---------------------------------------------------------------------------

const TWO_SESSION_CONFIG: &str = r#"
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
    poke = true

    [[sessions]]
    id = "side"
    harness = "dsh"
    persona = "p"
    token = "t-side"

    [[sessions.subscriptions]]
    connection = "qq"
    room = "group/123"

    [sessions.subscriptions.wake]
    poke = true
"#;

#[test]
fn a_reply_is_acked_for_every_subscriber_not_just_the_speaker() {
    // 两个会话订同一个群。修复前：main 发的回复只在 main 的水位上确认，
    // side 的水位上永远留一个洞——水位只推连续前沿，洞后面的消息在 side
    // 那边就永远算未读，表现为「同一批积压戳一次投一遍」（2026-10-31 真机）。
    let sends: Sends = Arc::new(Mutex::new(Vec::new()));
    let mut bridge = Bridge::new(
        acb_kernel::Config::parse(TWO_SESSION_CONFIG).expect("配置合法"),
        MemoryStore::new(),
        Box::new(Recorder {
            sends: sends.clone(),
        }),
    );

    // 1. 戳一下，两个会话都醒；各开一轮把 #1 确认掉，两边的水位都到 1。
    bridge.ingest(poke("qq:group/123", true)).expect("能记下来");
    let first = bridge.poll_turn().expect("能开轮").expect("有活");
    assert_eq!(first.session, "main");
    bridge
        .tool_call(
            &first.ticket,
            &["main".to_owned()],
            "ack_messages",
            json!({"ids": [1]}),
        )
        .expect("能确认");
    bridge.end_turn(&first.ticket, None).expect("能收工");
    let second = bridge.poll_turn().expect("能开轮").expect("有活");
    assert_eq!(second.session, "side");
    bridge
        .tool_call(
            &second.ticket,
            &["side".to_owned()],
            "ack_messages",
            json!({"ids": [1]}),
        )
        .expect("能确认");
    bridge.end_turn(&second.ticket, None).expect("能收工");

    // 2. main 再被戳醒，回了一句话（#3，自己发的）。
    bridge.ingest(poke("qq:group/123", true)).expect("能记下来");
    let turn = bridge.poll_turn().expect("能开轮").expect("有活");
    assert_eq!(turn.session, "main");
    bridge
        .tool_call(
            &turn.ticket,
            &["main".to_owned()],
            "send_message",
            json!({"elements": [{"type": "text", "text": "在"}]}),
        )
        .expect("能发");
    // #3 是自己发的，deliver_out 已替所有订阅者确认；模型只需确认收到的 #2。
    let acked = bridge
        .tool_call(
            &turn.ticket,
            &["main".to_owned()],
            "ack_messages",
            json!({"ids": [2]}),
        )
        .expect("能确认");
    assert_eq!(acked["watermark"], 3);
    bridge.end_turn(&turn.ticket, None).expect("能收工");

    // 3. side 开轮：#3 已经在 side 的账上确认过了（只是 #2 还没轮到它开轮）。
    //    所以投递里只有 #2，而且确认 #2 之后水位能直接迈过 #3。
    let side = bridge
        .poll_turn()
        .expect("能开轮")
        .expect("side 还挂着一次唤醒");
    assert_eq!(side.session, "side");
    assert_eq!(
        side.records.iter().map(|r| r.id).collect::<Vec<_>>(),
        vec![2],
        "side 只欠 #2；自己人的 #3 不该出现"
    );
    let acked = bridge
        .tool_call(
            &side.ticket,
            &["side".to_owned()],
            "ack_messages",
            json!({"ids": [2]}),
        )
        .expect("能确认");
    assert_eq!(
        acked["watermark"], 3,
        "确认 #2 后前沿要能跳过已确认的 #3——否则 #3 就是永久洞"
    );
    bridge.end_turn(&side.ticket, None).expect("能收工");

    // 4. 自愈：旧版本留下的洞（自己发的记录停在水位上），下次开轮自动补掉，
    //    不用手工修库。
    bridge
        .store_mut()
        .append(RecordDraft::message(
            room_of("qq:group/123"),
            Sender {
                id: "bot".to_owned(),
                name: "bot".to_owned(),
                role: None,
            },
            AT,
            vec![Element::text("历史版本留下的洞")],
            false,
        ))
        .expect("能记下来");
    bridge.ingest(poke("qq:group/123", true)).expect("能记下来");
    let main = bridge.poll_turn().expect("能开轮").expect("有活");
    bridge.end_turn(&main.ticket, None).expect("能收工");
    let side = bridge.poll_turn().expect("能开轮").expect("side 有活");
    assert_eq!(
        side.records.iter().map(|r| r.id).collect::<Vec<_>>(),
        vec![5],
        "只有新戳的 #5；#4 是自己发的，自愈时已确认"
    );
    let acked = bridge
        .tool_call(
            &side.ticket,
            &["side".to_owned()],
            "ack_messages",
            json!({"ids": [5]}),
        )
        .expect("能确认");
    assert_eq!(acked["watermark"], 5, "自愈把 #4 补确认了，前沿才能到 5");
    bridge.end_turn(&side.ticket, None).expect("能收工");
}

// ---------------------------------------------------------------------------
// 人格下发
// ---------------------------------------------------------------------------

/// 桥只给语义：人格卡正文随 Delivery 下去，怎么写进模型上下文由 harness 决定。
#[test]
fn delivery_carries_the_session_persona() {
    let (mut bridge, _sends) = bridge();
    bridge
        .ingest(message("qq:group/123", "10001", "在吗", true))
        .expect("能记下来");
    let delivery = bridge.poll_turn().expect("能开轮").expect("有活");
    let persona = delivery.persona.expect("人格要随轮次下发");
    assert_eq!(persona.id, "p");
    assert_eq!(persona.name, "人格");
    assert_eq!(persona.text, "你是桥。");
}

// ---------------------------------------------------------------------------
// 投递截断：最旧一小段 + 最新一批，中间明确告知（2026-11-01 用户裁定）
// ---------------------------------------------------------------------------

#[test]
fn delivery_truncation_keeps_newest_and_a_small_oldest_slice() {
    // 积压 5 条、上限 2 条：最旧留 1 条（水位要能前进）+ 最新 1 条（模型要看眼前）。
    // 中间 #2–#4 跳过，编号区间明确告诉模型。
    let text = format!("delivery_limit = 2\n{}", config());
    let sends: Sends = Arc::new(Mutex::new(Vec::new()));
    let mut bridge = Bridge::new(
        acb_kernel::Config::parse(&text).expect("配置合法"),
        MemoryStore::new(),
        Box::new(Recorder {
            sends: sends.clone(),
        }),
    );

    for i in 1..=5 {
        bridge
            .ingest(message("qq:group/123", "10001", &format!("第{i}条"), true))
            .expect("能记下来");
    }

    let delivery = bridge.poll_turn().expect("能开轮").expect("有活");
    let ids: Vec<u64> = delivery.records.iter().map(|r| r.id).collect();
    assert_eq!(ids, vec![1, 5], "最旧一条 + 最新一条");
    assert_eq!(delivery.more, Some(3), "中间跳过 3 条");
    assert_eq!(
        delivery.omitted,
        Some(Omitted {
            from: 2,
            to: 4,
            count: 3
        }),
        "跳过的编号区间要明确给出"
    );
}

// ---------------------------------------------------------------------------
// Phase 4：投递 composer + 发送前后钩子
// ---------------------------------------------------------------------------

#[test]
fn custom_delivery_composer_decides_the_batch() {
    let text = config_with("        [sessions.delivery]\n        provider = \"test.latest\"");
    let mut plugins = PluginManager::with_native_builtins();
    plugins
        .install_manager(Arc::new(LatestOnlyManager))
        .expect("能安装 composer");

    let (mut bridge, _sends) = bridge_with_plugins(&text, plugins);
    for i in 1..=3 {
        bridge
            .ingest(message("qq:group/123", "10001", &format!("第{i}句"), true))
            .expect("能记下来");
    }

    let delivery = bridge.poll_turn().expect("能开轮").expect("有活");
    let ids: Vec<u64> = delivery.records.iter().map(|record| record.id).collect();
    assert_eq!(ids, vec![3], "composer 只留最新一条，未知编号被忽略");
    assert_eq!(delivery.omitted, None, "composer 说没跳过中段");
}

#[test]
fn send_hooks_can_rewrite_and_observe() {
    let text = config_with(
        "        [sessions.outbound]\n        before = [\"hook.sign\"]\n        after = [\"hook.sign\"]",
    );
    let hook = Arc::new(SignHook::default());
    let mut plugins = PluginManager::with_native_builtins();
    plugins
        .install_manager(Arc::new(SignHookManager { hook: hook.clone() }))
        .expect("能安装发送钩子");

    let (mut bridge, sends) = bridge_with_plugins(&text, plugins);
    bridge
        .ingest(message("qq:group/123", "10001", "在吗", true))
        .expect("能记下来");
    let ticket = bridge.poll_turn().expect("能开轮").expect("有活").ticket;

    bridge
        .tool_call(
            &ticket,
            &sessions(),
            "send_message",
            json!({"elements": [{"type": "text", "text": "你好"}]}),
        )
        .expect("能发送");

    let sends = sends.lock().expect("没被毒死");
    let (_, elements) = &sends[0];
    assert_eq!(elements.len(), 2, "钩子应当追加了一段");
    match &elements[1] {
        Element::Text { text } => assert_eq!(text, " [签名]"),
        other => panic!("第二段应当是文本，收到 {other:?}"),
    }
    drop(sends);

    let after = hook.after.lock().expect("没被毒死");
    assert_eq!(
        after.as_slice(),
        ["main:p-1"],
        "发送后钩子应当拿到回执"
    );
}

#[test]
fn custom_turn_error_provider_decides_notifications() {
    let text = config_with(
        "        [sessions.turn_error]\n        provider = \"test.failure\"",
    );
    let mut plugins = PluginManager::with_native_builtins();
    plugins
        .install_manager(Arc::new(RedirectTurnErrorManager))
        .expect("能安装失败通知 provider");

    let (mut bridge, sends) = bridge_with_plugins(&text, plugins);
    bridge
        .ingest(message("qq:group/123", "10001", "在吗", true))
        .expect("能记下来");
    let delivery = bridge.poll_turn().expect("能开轮").expect("有活");
    bridge
        .end_turn(&delivery.ticket, Some("boom"))
        .expect("能收工");
    bridge.flush_notices();

    let sends = sends.lock().expect("没被毒死");
    assert_eq!(sends.len(), 1);
    assert_eq!(sends[0].0, room("qq:group/ops"), "provider 指定的房间");
    let text = match &sends[0].1[0] {
        Element::Text { text } => text,
        other => panic!("失败通知应是文本，收到 {other:?}"),
    };
    assert!(text.contains("[一轮失败]"));
    assert!(text.contains("boom"));
}

#[test]
fn interval_clock_opens_a_turn_with_no_records() {
    let text = clock_config();
    let sends: Sends = Arc::new(Mutex::new(Vec::new()));
    let mut bridge = Bridge::new(
        acb_kernel::Config::parse(&text).expect("配置合法"),
        MemoryStore::new(),
        Box::new(Recorder {
            sends: sends.clone(),
        }),
    );

    assert_eq!(bridge.tick_clock(AT), 0, "第一 tick 只开始计时");
    assert_eq!(bridge.tick_clock(AT + 30_000), 0, "还没到 60 秒");
    assert_eq!(bridge.tick_clock(AT + 61_000), 1, "到点排一轮");

    let delivery = bridge.poll_turn().expect("能开轮").expect("有活");
    assert_eq!(delivery.reason, TurnReason::Clock);
    assert_eq!(delivery.room, room("dc:channel/456"));
    assert!(delivery.records.is_empty(), "例行检查可以没有新记录");
}

#[test]
fn clock_is_off_by_default() {
    let (mut bridge, _sends) = bridge();
    assert_eq!(bridge.tick_clock(AT), 0);
    assert_eq!(bridge.tick_clock(AT + 3_600_000), 0, "默认不产生定时轮次");
}

#[test]
fn custom_receive_provider_can_drop_inbound_records() {
    let text = config().replacen(
        "        account = \"bot\"",
        "        account = \"bot\"\n\n        [connections.receive]\n        provider = \"test.drop\"",
        1,
    );
    let mut plugins = PluginManager::with_native_builtins();
    plugins
        .install_manager(Arc::new(DropAdsManager))
        .expect("能安装入站策略");

    let (mut bridge, _sends) = bridge_with_plugins(&text, plugins);
    let dropped = bridge
        .ingest(message("qq:group/123", "10001", "广告", true))
        .expect("不该崩");
    assert_eq!(
        dropped,
        Ingest::Dropped {
            room: room("qq:group/123")
        },
        "provider 说不收就不写流水"
    );

    let kept = bridge
        .ingest(message("qq:group/123", "10001", "在吗", true))
        .expect("能记下来");
    assert!(matches!(kept, Ingest::Recorded { .. }), "正常消息照常入库");
}
