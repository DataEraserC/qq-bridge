//! 端到端：平台能力目录 + 通用调用口（ADR-0020）。
//!
//! 这里的 `Platformy` 假装有一条带平台能力的连接。桥不认识任何方法名——它只认
//! `MethodSpec` 这个形状，所以这些测试同时也在证明「core 不认识 QQ」。

use std::sync::{Arc, Mutex};

use acb_kernel::{
    Bridge, CallError, Element, Ingest, MemoryStore, MethodSpec, Outbound, OutboundError,
    ParamSpec, ParamType, RecordDraft, Risk, RoomId, Sender, Sent, Ticket,
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

static METHODS: &[MethodSpec] = &[
    MethodSpec {
        name: "forward.get_content",
        summary: "读一条合并转发的正文",
        risk: Risk::Read,
        params: FORWARD_PARAMS,
    },
    MethodSpec {
        name: "group.get_member_info",
        summary: "查群成员信息",
        risk: Risk::Read,
        params: MEMBER_PARAMS,
    },
    MethodSpec {
        name: "group.kick",
        summary: "踢人",
        risk: Risk::Destructive,
        params: KICK_PARAMS,
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

    fn call(&mut self, _room: &RoomId, method: &str, params: &Value) -> Result<Value, CallError> {
        self.calls.lock().expect("没被毒死").push((
            method.to_owned(),
            params.clone(),
        ));
        match method {
            "forward.get_content" => Ok(json!({
                "messages": [{"sender": "老王", "text": "转发的正文"}]
            })),
            "group.get_member_info" => Ok(json!({"nickname": "老王", "role": "member"})),
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
    let calls: Calls = Arc::new(Mutex::new(Vec::new()));
    let bridge = Bridge::new(
        acb_kernel::Config::parse(config()).expect("配置合法"),
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

/// 什么都不给：只给分组摘要，省上下文。
#[test]
fn catalog_summary_lists_groups_without_dumping_everything() {
    let (mut bridge, _calls) = setup();
    let ticket = open_turn(&mut bridge);

    let result = bridge
        .tool_call(&ticket, &sessions(), "platform_methods", json!({}))
        .expect("能查目录");

    assert_eq!(result["platform"], "qq");
    assert_eq!(result["total"], 3);
    let groups = result["groups"].as_array().expect("有分组");
    assert_eq!(groups.len(), 2);
    assert_eq!(groups[0]["group"], "forward");
    assert_eq!(groups[0]["count"], 1);
    assert_eq!(groups[1]["group"], "group");
    assert_eq!(groups[1]["count"], 2);
    // 摘要里不该出现方法详情，否则等于全倒出来了。
    assert!(result.get("methods").is_none());
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

#[test]
fn catalog_group_that_does_not_exist_lists_the_real_ones() {
    let (mut bridge, _calls) = setup();
    let ticket = open_turn(&mut bridge);

    let error = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "platform_methods",
            json!({"group": "message"}),
        )
        .expect_err("没有这一组");
    assert!(error.message.contains("forward"));
    assert!(error.message.contains("group"));
}

/// 知道要干什么但不知道方法名：按说明里的词也能搜到。
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

    let hits = result["hits"].as_array().expect("有命中");
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
