//! 端到端：平台能力目录 + 通用调用口（ADR-0020）。
//!
//! 这里的 `Platformy` 假装有一条带平台能力的连接。桥不认识任何方法名——它只认
//! `MethodSpec` 这个形状，所以这些测试同时也在证明「core 不认识 QQ」。

use std::sync::{Arc, Mutex};

use acb_kernel::{
    Bridge, CallError, CallOutcome, Cursor, Effect, Element, ErrorCode, Ingest, MemoryStore,
    MethodSpec, Outbound, OutboundError, OutgoingKind, OutgoingRecord, ParamSpec, ParamType,
    RecordDraft, RecordMode, Risk, RoomId, Scope, Sender, Sent, TargetSpec, Ticket,
    Transcript, Watermarks, outbound::tags, unread,
};
use serde_json::{Value, json};

const AT: i64 = 1_793_000_000_000;

// ── 假平台 ──────────────────────────────────────────────────────────────

static FORWARD_PARAMS: &[ParamSpec] = &[ParamSpec::required("id", "转发 id")];

static MEMBER_PARAMS: &[ParamSpec] = &[
    ParamSpec::required("user_id", "成员号"),
    ParamSpec::optional("group_id", ParamType::String, "群号，群里调用可以不填"),
];

static KICK_PARAMS: &[ParamSpec] = &[ParamSpec::required("user_id", "要踢的人")];

static IMAGE_PARAMS: &[ParamSpec] = &[ParamSpec::required("file", "图片")];

static FILE_PARAMS: &[ParamSpec] = &[ParamSpec::required("file_id", "群文件 id")];

/// 测试平台声明：`group_id` 是房间目标，不能由模型在 params 里换掉。
static GROUP_TARGETS: &[TargetSpec] = &[TargetSpec::room("group_id", "group")];

/// 故意让 `image` 这个标签跨在两个归属上：`message.*` 和 `group.*` 各有一个。
/// 这正是 tag 维度存在的理由——模型问「图片相关」，一次就得把两边都捞出来。
static METHODS: &[MethodSpec] = &[
    MethodSpec {
        name: "forward.get_content",
        summary: "读一条合并转发的正文",
        tags: &[tags::FORWARD, tags::MESSAGE],
        risk: Risk::Read,
        scope: Scope::Room,
        params: FORWARD_PARAMS,
        targets: &[],
        effect: Effect::Read,
        record: RecordMode::None,
    },
    MethodSpec {
        name: "group.get_member_info",
        summary: "查群成员信息",
        tags: &[tags::MEMBER, tags::GROUP],
        risk: Risk::Read,
        scope: Scope::Room,
        params: MEMBER_PARAMS,
        targets: GROUP_TARGETS,
        effect: Effect::Read,
        record: RecordMode::None,
    },
    MethodSpec {
        name: "message.send_image",
        summary: "往会话里发一张图",
        tags: &[tags::SEND, tags::IMAGE, tags::MEDIA],
        risk: Risk::Write,
        scope: Scope::Room,
        params: IMAGE_PARAMS,
        targets: &[],
        effect: Effect::Send,
        record: RecordMode::Message,
    },
    MethodSpec {
        name: "group.get_file_url",
        summary: "取群文件的下载地址",
        tags: &[tags::FILE, tags::IMAGE, tags::DOWNLOAD],
        risk: Risk::Read,
        scope: Scope::Room,
        params: FILE_PARAMS,
        targets: GROUP_TARGETS,
        effect: Effect::Read,
        record: RecordMode::None,
    },
    MethodSpec {
        name: "group.kick",
        summary: "踢人",
        tags: &[tags::ADMIN, tags::MEMBER],
        risk: Risk::Destructive,
        scope: Scope::Room,
        params: KICK_PARAMS,
        targets: GROUP_TARGETS,
        effect: Effect::Write,
        record: RecordMode::None,
    },
    MethodSpec {
        name: "group.ban",
        summary: "禁言",
        tags: &[tags::ADMIN, tags::MUTE],
        risk: Risk::Destructive,
        scope: Scope::Room,
        params: KICK_PARAMS,
        targets: GROUP_TARGETS,
        effect: Effect::Write,
        record: RecordMode::None,
    },
    MethodSpec {
        name: "group.get_member_list",
        summary: "群成员列表（不带分页，一次给全）",
        tags: &[tags::MEMBER, tags::GROUP],
        risk: Risk::Read,
        scope: Scope::Room,
        params: &[],
        targets: GROUP_TARGETS,
        effect: Effect::Read,
        record: RecordMode::None,
    },
    // 跟房间无关的那一类：查的是这个号本身有什么，不是房间里发生了什么。
    MethodSpec {
        name: "friend.list",
        summary: "好友列表",
        tags: &[tags::FRIEND],
        risk: Risk::Read,
        scope: Scope::Connection,
        params: &[],
        targets: &[],
        effect: Effect::Read,
        record: RecordMode::None,
    },
];

type Calls = Arc<Mutex<Vec<(String, Value)>>>;

struct Platformy {
    calls: Calls,
}

impl Outbound for Platformy {
    fn send(&mut self, _room: &RoomId, _elements: &[Element]) -> Result<Sent, OutboundError> {
        Ok(Sent {
            platform_id: "p".into(),
            at: AT,
        })
    }

    /// 只有 `qq` 这条连接有平台能力，`dc` 没有——用来验证「这个连接没声明」那条路。
    fn catalog(&self, connection: &str) -> &'static [MethodSpec] {
        if connection == "qq" { METHODS } else { &[] }
    }

    fn call(
        &mut self,
        _room: &RoomId,
        method: &str,
        params: &Value,
    ) -> Result<CallOutcome, CallError> {
        self.calls.lock().expect("没被毒死").push((
            method.to_owned(),
            params.clone(),
        ));
        match method {
            "forward.get_content" => Ok(CallOutcome::value(json!({
                "messages": [{"sender": "老王", "text": "转发的正文"}]
            }))),
            // 发送类方法要把“实际发出去的内容”翻译成流水记录带回来。
            "message.send_image" => Ok(CallOutcome {
                value: json!({"message_id": 9002}),
                record: Some(OutgoingRecord {
                    kind: OutgoingKind::Message,
                    elements: vec![Element::Image {
                        url: params["file"].as_str().unwrap_or_default().to_owned(),
                        alt: None,
                    }],
                    platform_id: Some("9002".into()),
                    at: Some(AT),
                }),
            }),
            "group.get_member_info" => Ok(CallOutcome::value(json!({
                "nickname": "老王",
                "role": "member"
            }))),
            "group.kick" | "group.ban" => Ok(CallOutcome::value(json!({"done": true}))),
            "friend.list" => Ok(CallOutcome::value(json!({
                "friends": [{"user_id": "10001", "nickname": "老王"}]
            }))),
            // 213 人的群：7 个管理员、206 个普通成员。用来验分页和分组计数。
            "group.get_member_list" => {
                let members: Vec<Value> = (1..=213)
                    .map(|index| {
                        json!({
                            "user_id": index,
                            "nickname": format!("成员{index}"),
                            "role": if index <= 7 { "admin" } else { "member" },
                        })
                    })
                    .collect();
                Ok(CallOutcome::value(json!({"retcode": 0, "data": members})))
            }
            other => Err(CallError::NotImplemented(other.to_owned())),
        }
    }
}

// ── 脚手架 ──────────────────────────────────────────────────────────────

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

fn setup() -> (Bridge<MemoryStore>, Calls) {
    setup_with("")
}

/// 给 qq 那条连接额外加配置（比如 `allow_destructive = true`）。
fn setup_with(extra: &str) -> (Bridge<MemoryStore>, Calls) {
    let calls: Calls = Arc::new(Mutex::new(Vec::new()));
    // 插在 qq 连接的第一行配置后面；`account = "bot"` 全篇只出现一次。
    let text = config().replace(
        "account = \"bot\"",
        &format!("account = \"bot\"\n        {extra}"),
    );
    let bridge = Bridge::new(
        acb_kernel::Config::parse(&text).expect("配置合法"),
        MemoryStore::new(),
        Box::new(Platformy {
            calls: calls.clone(),
        }),
    );
    (bridge, calls)
}

fn sessions() -> Vec<String> {
    vec!["main".to_owned()]
}

/// 开一轮，拿到票。工具调用都发生在这一轮里。
fn open_turn(bridge: &mut Bridge<MemoryStore>) -> Ticket {
    let ingested = bridge
        .ingest(RecordDraft::message(
            RoomId::parse("qq:group/123").expect("房间合法"),
            Sender {
                id: "10001".to_owned(),
                name: "老王".to_owned(),
                role: None,
            },
            AT,
            vec![Element::text("在吗")],
            true,
        ))
        .expect("能记下来");
    assert!(matches!(ingested, Ingest::Recorded { .. }));
    bridge
        .poll_turn()
        .expect("能开轮")
        .expect("有活")
        .ticket
}

// ── 目录 ────────────────────────────────────────────────────────────────

/// 什么都不给：逐条清单。够模型决定调哪个，但不带参数表——带表就等于全倒出来了。
#[test]
fn catalog_lists_every_method_without_dumping_params() {
    let (mut bridge, _calls) = setup();
    let ticket = open_turn(&mut bridge);

    let result = bridge
        .tool_call(&ticket, &sessions(), "platform_methods", json!({}))
        .expect("能查目录");

    assert_eq!(result["platform"], "qq");
    assert_eq!(result["total"], 8);

    let methods = result["methods"].as_array().expect("有清单");
    assert_eq!(methods.len(), 8);
    assert_eq!(methods[0]["name"], "forward.get_content");
    assert_eq!(methods[0]["tags"][0], "forward");
    assert_eq!(methods[2]["risk"], "write");
    // 清单档不带参数表。
    assert!(methods[0].get("params").is_none());

    // 标签概览：模型能先看出「图片相关的有两个」。
    let tags = result["tags"].as_array().expect("有标签概览");
    let image = tags
        .iter()
        .find(|item| item["tag"] == "image")
        .expect("有 image 标签");
    assert_eq!(image["count"], 2);
}

/// 给 group：这一组的方法 + 参数表，参数表是 JSON Schema（模型最熟的形式）。
#[test]
fn catalog_group_detail_carries_json_schema_params() {
    let (mut bridge, _calls) = setup();
    let ticket = open_turn(&mut bridge);

    let result = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "platform_methods",
            json!({"group": "forward"}),
        )
        .expect("能查这一组");

    let methods = result["methods"].as_array().expect("有方法");
    assert_eq!(methods.len(), 1);
    assert_eq!(methods[0]["name"], "forward.get_content");
    assert_eq!(methods[0]["risk"], "read");
    assert_eq!(methods[0]["params"]["type"], "object");
    assert_eq!(methods[0]["params"]["properties"]["id"]["type"], "string");
    assert_eq!(methods[0]["params"]["required"][0], "id");
}

/// 标签是跨归属的：`image` 在 message 和 group 两处各有一个方法，一次全给。
#[test]
fn catalog_tag_pulls_methods_from_several_groups() {
    let (mut bridge, _calls) = setup();
    let ticket = open_turn(&mut bridge);

    let result = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "platform_methods",
            json!({"tag": "image"}),
        )
        .expect("能按标签查");

    let methods = result["methods"].as_array().expect("有方法");
    assert_eq!(result["matched"], 2);
    let names: Vec<&str> = methods
        .iter()
        .map(|method| method["name"].as_str().unwrap_or_default())
        .collect();
    assert!(names.contains(&"message.send_image"));
    assert!(names.contains(&"group.get_file_url"));
    // 缩了范围就带参数表，模型下一步就能直接调。
    assert!(methods[0].get("params").is_some());
}

/// 模型说中文（「图片」），适配器声明的是 `image`：别名表把两头接上。
#[test]
fn catalog_query_in_chinese_still_reaches_tags() {
    let (mut bridge, _calls) = setup();
    let ticket = open_turn(&mut bridge);

    let result = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "platform_methods",
            json!({"query": "图片"}),
        )
        .expect("能搜");

    assert_eq!(result["matched"], 2);
}

/// 三个条件可以叠加，取交集。
#[test]
fn catalog_filters_stack() {
    let (mut bridge, _calls) = setup();
    let ticket = open_turn(&mut bridge);

    let result = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "platform_methods",
            json!({"group": "group", "tag": "image"}),
        )
        .expect("能叠加");

    assert_eq!(result["matched"], 1);
    assert_eq!(result["methods"][0]["name"], "group.get_file_url");
}

#[test]
fn catalog_group_that_does_not_exist_lists_the_real_ones() {
    let (mut bridge, _calls) = setup();
    let ticket = open_turn(&mut bridge);

    let error = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "platform_methods",
            json!({"group": "user"}),
        )
        .expect_err("没有这一组");
    assert!(error.message.contains("no_match"));
    assert!(error.message.contains("forward"));
    assert!(error.message.contains("message"));
}

/// 标签不存在时把手上的标签列出来，别让模型挨个瞎试。
#[test]
fn catalog_unknown_tag_lists_the_known_ones() {
    let (mut bridge, _calls) = setup();
    let ticket = open_turn(&mut bridge);

    let error = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "platform_methods",
            json!({"tag": "qwq"}),
        )
        .expect_err("没这个标签");
    assert!(error.message.contains("no_match"));
    assert!(error.message.contains("image"));
    assert!(error.message.contains("member"));
}

/// 关键词搜说明文字也还算数（不只是标签）。
#[test]
fn catalog_query_matches_the_summary_text() {
    let (mut bridge, _calls) = setup();
    let ticket = open_turn(&mut bridge);

    let result = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "platform_methods",
            json!({"query": "转发"}),
        )
        .expect("能搜");

    let hits = result["methods"].as_array().expect("有命中");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0]["name"], "forward.get_content");
}

/// 这个连接没声明任何方法：明确说「平台不支持」，别报个内部错让模型瞎猜。
#[test]
fn connection_without_a_catalog_says_unsupported() {
    let (mut bridge, _calls) = setup();
    let ticket = open_turn(&mut bridge);

    let error = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "platform_methods",
            json!({"room": "dc:channel/456"}),
        )
        .expect_err("这条连接没有平台能力");
    assert!(error.message.contains("unsupported_platform"));
}

// ── 调用 ────────────────────────────────────────────────────────────────

#[test]
fn call_returns_the_platform_result() {
    let (mut bridge, calls) = setup();
    let ticket = open_turn(&mut bridge);

    let result = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "platform_call",
            json!({"method": "forward.get_content", "params": {"id": "x7/O5"}}),
        )
        .expect("能调");

    assert_eq!(result["messages"][0]["text"], "转发的正文");
    let calls = calls.lock().expect("没被毒死");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "forward.get_content");
    assert_eq!(calls[0].1["id"], "x7/O5");
}

/// 发送类 `platform_call` 也要进流水：这是模型自己说的话，不记就会在
/// 自己的水位上留洞，洞后面的新消息永远推不过去。
#[test]
fn platform_call_send_is_recorded_and_acked() {
    let (mut bridge, _calls) = setup();
    let ticket = open_turn(&mut bridge);

    bridge
        .tool_call(
            &ticket,
            &sessions(),
            "platform_call",
            json!({
                "method": "message.send_image",
                "params": {"file": "https://example.com/a.png"},
            }),
        )
        .expect("能发");

    let room = RoomId::parse("qq:group/123").expect("房间合法");
    let stored = bridge.store().since(&room, 1, 10).expect("能读");
    assert_eq!(stored.len(), 1, "平台调用发的消息要写进流水");
    assert_eq!(stored[0].sender.id, "bot", "发送者是自己这个号");
    assert_eq!(stored[0].platform_id.as_deref(), Some("9002"));

    // 先确认轮次里收到的那条，水位才推得动；自己发的那条已经确认过，不能留下洞。
    let acked = bridge
        .tool_call(&ticket, &sessions(), "ack_messages", json!({"ids": [1]}))
        .expect("能确认");
    assert_eq!(acked["watermark"], 2);
    assert_eq!(
        bridge
            .store()
            .watermark(&Cursor::new("main", room.clone()))
            .expect("可读"),
        2
    );
    assert!(
        unread(bridge.store(), &Cursor::new("main", room), 100)
            .expect("可读")
            .is_empty()
    );
}

/// 方法名写错：给出同组的近似名，让模型一轮就改对，而不是反复猜。
#[test]
fn unknown_method_suggests_a_direct_neighbour() {
    let (mut bridge, _calls) = setup();
    let ticket = open_turn(&mut bridge);

    let error = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "platform_call",
            json!({"method": "group.get_member", "params": {}}),
        )
        .expect_err("没有这个方法");

    assert!(error.message.contains("unknown_method"));
    assert!(error.message.contains("group.get_member_info"));
}

/// 必填参数没给：说清楚缺哪个、它是干什么的。
#[test]
fn missing_required_param_names_the_field() {
    let (mut bridge, _calls) = setup();
    let ticket = open_turn(&mut bridge);

    let error = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "platform_call",
            json!({"method": "forward.get_content", "params": {}}),
        )
        .expect_err("少了 id");
    assert!(error.message.contains("bad_params"));
    assert!(error.message.contains("id"));
}

/// 没声明的参数也拒：白名单精神，也帮模型发现字段名拼错了。
#[test]
fn undeclared_param_is_refused_with_the_allowed_list() {
    let (mut bridge, _calls) = setup();
    let ticket = open_turn(&mut bridge);

    let error = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "platform_call",
            json!({"method": "forward.get_content", "params": {"id": "x", "verbose": true}}),
        )
        .expect_err("多了参数");
    assert!(error.message.contains("verbose"));
    assert!(error.message.contains("id"));
}

/// 破坏性操作默认关——白名单挡不住「模型被人骗着去调它有权调的破坏性方法」。
#[test]
fn destructive_method_is_refused_by_default() {
    let (mut bridge, calls) = setup();
    let ticket = open_turn(&mut bridge);

    let error = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "platform_call",
            json!({"method": "group.kick", "params": {"user_id": "1"}}),
        )
        .expect_err("破坏性的默认不给");

    assert!(error.message.contains("forbidden"));
    // 关键：根本没交到适配器手上。
    assert!(calls.lock().expect("没被毒死").is_empty());
}

/// 但配了 `allow_destructive = true` 就要真的放行——「人能做的模型也能做」靠这一条成立。
#[test]
fn destructive_method_runs_when_the_connection_allows_it() {
    let (mut bridge, calls) = setup_with("allow_destructive = true");
    let ticket = open_turn(&mut bridge);

    let result = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "platform_call",
            json!({"method": "group.kick", "params": {"user_id": "1"}}),
        )
        .expect("开了就放行");

    assert_eq!(result["done"], true);
    assert_eq!(calls.lock().expect("没被毒死").len(), 1);
}

/// 只开名单里的那几个：列表外的仍然拒。
#[test]
fn allow_destructive_list_only_opens_the_named_methods() {
    let (mut bridge, calls) = setup_with("allow_destructive = [\"group.ban\"]");
    let ticket = open_turn(&mut bridge);

    let refused = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "platform_call",
            json!({"method": "group.kick", "params": {"user_id": "1"}}),
        )
        .expect_err("不在名单里");
    assert!(refused.message.contains("forbidden"));

    let allowed = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "platform_call",
            json!({"method": "group.ban", "params": {"user_id": "1"}}),
        )
        .expect("在名单里");
    assert_eq!(allowed["done"], true);
    assert_eq!(calls.lock().expect("没被毒死").len(), 1);
}

/// 订阅之外房间一律拒绝，和 send_message_to 同一条边界。
#[test]
fn room_outside_the_subscription_is_refused() {
    let (mut bridge, calls) = setup();
    let ticket = open_turn(&mut bridge);

    let error = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "platform_call",
            json!({"room": "qq:group/999", "method": "forward.get_content", "params": {"id": "x"}}),
        )
        .expect_err("没订阅这个房间");
    assert!(calls.lock().expect("没被毒死").is_empty());
    assert!(!error.message.is_empty());
}

/// core 用 MethodSpec.targets 兜底：房间目标参数不能由模型在 params 里换掉。
#[test]
fn platform_call_cannot_change_the_room_target_via_params() {
    let (mut bridge, calls) = setup();
    let ticket = open_turn(&mut bridge);

    let error = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "platform_call",
            json!({"method": "group.get_member_info", "params": {"user_id": "10001", "group_id": "999"}}),
        )
        .expect_err("不能在 params 里换群");
    assert_eq!(error.code, ErrorCode::BadRequest);
    assert!(error.message.contains("group_id"), "{}", error.message);
    assert!(
        error.message.contains("room"),
        "错误要给出怎么改：{}",
        error.message
    );
    assert!(calls.lock().expect("没被毒死").is_empty(), "拒绝前不该调平台");
}

/// 好友列表、账号资料这类跟房间无关：不该逼模型先编一个群号出来。
#[test]
fn connection_scoped_method_needs_no_room() {
    let (mut bridge, calls) = setup();
    let ticket = open_turn(&mut bridge);

    let result = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "platform_call",
            json!({"method": "friend.list"}),
        )
        .expect("不给房间也能查好友列表");

    assert_eq!(result["friends"][0]["nickname"], "老王");
    assert_eq!(calls.lock().expect("没被毒死").len(), 1);
}

/// 连接级只按「这条连接有没有订阅」量，不按房间量。
/// 反过来说：房间级那条边界一点没松——上面那个测试还红着就不行。
#[test]
fn connection_scoped_method_does_not_check_the_room() {
    let (mut bridge, _calls) = setup();
    let ticket = open_turn(&mut bridge);

    let result = bridge.tool_call(
        &ticket,
        &sessions(),
        "platform_call",
        json!({"room": "qq:group/999", "method": "friend.list"}),
    );
    assert!(result.is_ok(), "连接级方法不该因为房间没订阅就拒");
}

/// 清单里只有连接级方法带 scope 标记，房间级默认不写——省上下文。
#[test]
fn catalog_marks_only_connection_scoped_methods() {
    let (mut bridge, _calls) = setup();
    let ticket = open_turn(&mut bridge);

    let result = bridge
        .tool_call(&ticket, &sessions(), "platform_methods", json!({}))
        .expect("能查目录");

    let methods = result["methods"].as_array().expect("有清单");
    let friend = methods
        .iter()
        .find(|item| item["name"] == "friend.list")
        .expect("有好友列表");
    assert_eq!(friend["scope"], "connection");

    let forward = methods
        .iter()
        .find(|item| item["name"] == "forward.get_content")
        .expect("有转发");
    assert!(forward.get("scope").is_none(), "房间级是默认，不用写出来");
}

// ── 分页与查询（ADR-0022 / ADR-0023）───────────────────────────────────

/// 大列表必须给总数，而且能接着翻。以前是「超过 8000 字符就砍成半截字符串」，
/// 模型既数不出总数，也没法继续取。
#[test]
fn big_lists_page_with_a_total_instead_of_truncating() {
    let (mut bridge, _calls) = setup();
    let ticket = open_turn(&mut bridge);

    let first = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "platform_call",
            json!({"method": "group.get_member_list", "limit": 50}),
        )
        .expect("能调");

    assert_eq!(first["_page"]["total"], 213, "总数是真的总数");
    assert_eq!(first["_page"]["returned"], 50);
    assert_eq!(first["_page"]["has_more"], true);
    assert_eq!(first["retcode"], 0, "外壳字段不能丢");
    assert_eq!(first["data"].as_array().expect("还是数组").len(), 50);
    assert!(
        serde_json::to_vec(&first).expect("可序列化").len() <= 24 * 1024,
        "整页结果要装得进预算"
    );

    let next = first["_page"]["next_offset"].as_u64().expect("有下一页");
    let second = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "platform_call",
            json!({"method": "group.get_member_list", "limit": 50, "offset": next}),
        )
        .expect("能翻页");
    assert_eq!(second["data"][0]["user_id"], 51);
    assert_eq!(second["_page"]["offset"], 50);
}

/// 「群里几个管理员」不该需要拉全表：group_by 一次给答案。
#[test]
fn group_by_answers_counting_questions_without_the_whole_list() {
    let (mut bridge, _calls) = setup();
    let ticket = open_turn(&mut bridge);

    let result = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "platform_call",
            json!({"method": "group.get_member_list", "group_by": "role"}),
        )
        .expect("能分组");

    assert_eq!(result["matched"], 213);
    let groups = result["groups"].as_array().expect("有分组");
    assert_eq!(groups[0]["key"], "member");
    assert_eq!(groups[0]["count"], 206);
    assert_eq!(groups[1]["key"], "admin");
    assert_eq!(groups[1]["count"], 7);
    // 分组结果里没有明细，也没有分页信封——这是另一个形状，不是被截断的列表。
    assert!(result.get("_page").is_none());
    assert!(result.get("data").is_none());
}

/// 过滤要在分页之前做：否则 `_page.total` 给的是过滤前的数。
#[test]
fn where_filter_projection_run_before_paging() {
    let (mut bridge, _calls) = setup();
    let ticket = open_turn(&mut bridge);

    let result = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "platform_call",
            json!({
                "method": "group.get_member_list",
                "where": [{"field": "role", "op": "eq", "value": "admin"}],
                "fields": ["user_id"],
                "order_by": "user_id desc",
                "limit": 10,
            }),
        )
        .expect("能查");

    assert_eq!(result["matched"], 7, "过滤后的总数");
    let data = result["data"].as_array().expect("还是数组");
    assert_eq!(data.len(), 7);
    assert_eq!(data[0], json!({"user_id": 7}), "降序且只留 user_id");
    assert_eq!(result["_page"]["total"], 7, "分页信封跟着过滤后的数");
    assert_eq!(result["_page"]["has_more"], false);
}

/// 字段写错要报错，并把真实字段列出来——不然只能猜着改。
#[test]
fn bad_query_field_lists_the_real_ones() {
    let (mut bridge, _calls) = setup();
    let ticket = open_turn(&mut bridge);

    let error = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "platform_call",
            json!({
                "method": "group.get_member_list",
                "where": [{"field": "rol", "op": "eq", "value": "admin"}],
            }),
        )
        .expect_err("字段写错就该报错");
    assert_eq!(error.code, acb_kernel::ErrorCode::BadRequest);
    assert!(error.message.contains("[bad_field]"), "{}", error.message);
    assert!(error.message.contains("role"), "{}", error.message);
}

/// 详情档要给能照抄的样例：参数表说「填什么」，样例说「填好长这样」。
#[test]
fn catalog_details_carry_a_copyable_example() {
    let (mut bridge, _calls) = setup();
    let ticket = open_turn(&mut bridge);

    let result = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "platform_methods",
            json!({"group": "forward"}),
        )
        .expect("能查这一组");

    let example = &result["methods"][0]["example"];
    assert_eq!(example["method"], "forward.get_content", "样例要能直接喂给 platform_call");
    assert_eq!(example["params"]["id"], "123456");
    assert!(result["hint"].as_str().expect("有说明").contains("example"));

    // 清单档不带样例：五十来条都塞一遍，清单就没法看了。
    let list = bridge
        .tool_call(&ticket, &sessions(), "platform_methods", json!({}))
        .expect("能查清单");
    assert!(list["methods"][0].get("example").is_none());
}

/// 样例里的值是「改一改就能跑」的，不是 `<string>` 这种还得翻译一遍的占位符。
#[test]
fn example_values_look_real() {
    let (mut bridge, _calls) = setup();
    let ticket = open_turn(&mut bridge);

    let result = bridge
        .tool_call(&ticket, &sessions(), "platform_methods", json!({"tag": "send"}))
        .expect("能查");

    let methods = result["methods"].as_array().expect("有方法");
    let send = methods
        .iter()
        .find(|method| method["name"] == "message.send_image")
        .expect("有发图的方法");
    assert_eq!(
        send["example"]["params"]["file"], "https://example.com/a.png",
        "文件参数给一个真的地址，照着改就行"
    );
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
        let dir = std::env::temp_dir().join(format!(
            "acb-relay-{}-{name}-{nanos}",
            std::process::id()
        ));
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
fn 平台调用的params里本机文件换成中继URL() {
    let temp = TempRelayDir::new("platform-call");
    let source = temp.0.join("upload.bin");
    std::fs::write(&source, b"file-bytes").expect("能写源文件");

    // [file_relay] 是根级表，追加在配置末尾是合法 TOML。
    let text = format!(
        "{}\n[file_relay]\nlisten = \"127.0.0.1:0\"\npublic_base = \"http://10.89.0.1:3131\"\ndir = \"{}\"",
        config(),
        temp.0.join("relay").display()
    );
    let calls: Calls = Arc::new(Mutex::new(Vec::new()));
    let mut bridge = Bridge::new(
        acb_kernel::Config::parse(&text).expect("配置合法"),
        MemoryStore::new(),
        Box::new(Platformy {
            calls: calls.clone(),
        }),
    );
    let ticket = open_turn(&mut bridge);

    bridge
        .tool_call(
            &ticket,
            &sessions(),
            "platform_call",
            json!({
                "method": "message.send_image",
                "params": {"file": source.display().to_string()},
            }),
        )
        .expect("能调");

    let calls = calls.lock().expect("没被毒死");
    let (method, params) = &calls[0];
    assert_eq!(method, "message.send_image");
    let file = params["file"].as_str().expect("file 还是字符串");
    assert!(
        file.starts_with("http://10.89.0.1:3131/f/"),
        "本机路径应该换成中继 URL，实际是 {file}"
    );
    let rest = file.strip_prefix("http://10.89.0.1:3131/f/").expect("前缀对");
    assert!(
        temp.0.join("relay").join(rest).is_file(),
        "文件应该真的落在中继目录里"
    );
}

#[test]
fn 平台调用的http路径原样透传() {
    let temp = TempRelayDir::new("platform-http");
    let text = format!(
        "{}\n[file_relay]\nlisten = \"127.0.0.1:0\"\npublic_base = \"http://10.89.0.1:3131\"\ndir = \"{}\"",
        config(),
        temp.0.join("relay").display()
    );
    let calls: Calls = Arc::new(Mutex::new(Vec::new()));
    let mut bridge = Bridge::new(
        acb_kernel::Config::parse(&text).expect("配置合法"),
        MemoryStore::new(),
        Box::new(Platformy {
            calls: calls.clone(),
        }),
    );
    let ticket = open_turn(&mut bridge);

    bridge
        .tool_call(
            &ticket,
            &sessions(),
            "platform_call",
            json!({
                "method": "message.send_image",
                "params": {"file": "https://elsewhere.example/a.png"},
            }),
        )
        .expect("能调");

    let calls = calls.lock().expect("没被毒死");
    assert_eq!(
        calls[0].1["file"],
        json!("https://elsewhere.example/a.png"),
        "http(s) 路径不依赖本地命名空间，不该动"
    );
}
