//! LuaManager / LuaLoader 的端到端测试：真写脚本、真装 manager、真调 provider。

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use acb_kernel::{
    AccessAction, AccessRequest, ClockContext, ClockPolicy, Config, DeliveryPolicy,
    DeliveryRequest, Element, Omitted, PluginManager, ProviderId, ReceiveDecision, ReceivePolicy,
    ReceiveRequest, Record, RecordKind, ReloadScope, RoomId, SendContext, SendOrigin, Sender, Sent,
    TurnErrorAction, TurnErrorPolicy, TurnErrorRequest, WakeContext, WakeDecision, WakePolicy,
};

use super::*;

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "acb-lua-{tag}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("建临时目录");
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn write(&self, name: &str, source: &str) {
        fs::write(self.0.join(name), source).expect("写脚本");
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn loaded(dir: &TempDir) -> PluginManager {
    let mut plugins = PluginManager::with_native_builtins();
    plugins
        .install_loader(&LuaLoader::new(dir.path()))
        .expect("Lua loader 应当能安装");
    plugins
}

fn wake_context<'a>(room: &'a RoomId, at_self: bool, text: &'a str) -> WakeContext<'a> {
    WakeContext {
        session: "main",
        room,
        connection: "qq",
        platform: "qq",
        sender_id: "10001",
        text,
        at_self,
        reply_to_self: false,
        event: None,
        tags: &[],
        unread: 1,
        now: 0,
    }
}

fn access_request<'a>(room: &'a RoomId, action: AccessAction) -> AccessRequest<'a> {
    AccessRequest {
        session: "main",
        connection: "qq",
        room: Some(room),
        action,
        subscriptions: &[],
    }
}

fn record(id: u64, text: &str) -> Record {
    Record {
        id,
        room: RoomId::new("qq", "group/1").expect("合法"),
        kind: RecordKind::Message,
        sender: Sender {
            id: "10001".to_owned(),
            name: "老王".to_owned(),
            role: None,
        },
        at: 0,
        elements: vec![Element::text(text)],
        at_self: false,
        reply_to_self: false,
        platform_id: None,
        quote_ref: None,
        redelivery: None,
        note: None,
    }
}

#[test]
fn wake_provider_runs_from_a_lua_script() {
    let dir = TempDir::new("wake");
    dir.write(
        "mention.lua",
        r#"
return {
  extension = "wake",
  decide = function(options, context)
    if context.at_self then return "wake" end
    if context.unread >= (options.accumulate or 1000) then return "wake" end
    return "stay"
  end,
}
"#,
    );
    let plugins = loaded(&dir);
    let id = ProviderId::parse("lua.mention").expect("合法");
    let provider = plugins
        .resolve_wake(&id, &WakePolicy::default())
        .expect("脚本 provider 应当可解析");

    let room = RoomId::new("qq", "group/1").expect("合法");
    assert_eq!(
        provider.decide(&WakePolicy::default(), &wake_context(&room, true, "在吗")),
        WakeDecision::Wake,
        "@ 到我就醒"
    );
    assert_eq!(
        provider.decide(&WakePolicy::default(), &wake_context(&room, false, "随便说说")),
        WakeDecision::Stay,
        "没 @、未读也不够就不醒"
    );
}

#[test]
fn lua_wake_can_return_a_reason() {
    let dir = TempDir::new("wake-reason");
    dir.write(
        "reason.lua",
        r#"
return {
  extension = "wake",
  decide = function(options, context)
    if context.at_self then
      return { decision = "wake", reason = "脚本：被 @ 了" }
    end
    return { decision = "stay" }
  end,
}
"#,
    );
    let plugins = loaded(&dir);
    let id = ProviderId::parse("lua.reason").expect("合法");
    let provider = plugins
        .resolve_wake(&id, &WakePolicy::default())
        .expect("脚本 provider 应当可解析");

    let room = RoomId::new("qq", "group/1").expect("合法");
    let verdict = provider.decide_with_reason(
        &WakePolicy::default(),
        &wake_context(&room, true, "在吗"),
    );
    assert_eq!(verdict.decision, WakeDecision::Wake);
    assert_eq!(verdict.reason.as_deref(), Some("脚本：被 @ 了"));
}

#[test]
fn access_provider_runs_from_a_lua_script() {
    let dir = TempDir::new("access");
    dir.write(
        "read_only.lua",
        r#"
return {
  extension = "access",
  decide = function(options, request)
    if request.action == "read" then return "allow" end
    return "deny"
  end,
}
"#,
    );
    let plugins = loaded(&dir);
    let id = ProviderId::parse("lua.read_only").expect("合法");
    let provider = plugins
        .resolve_access(&id, &acb_kernel::AccessPolicy::default())
        .expect("脚本 access provider 应当可解析");

    let room = RoomId::new("qq", "group/1").expect("合法");
    assert_eq!(
        provider.check(
            &acb_kernel::AccessPolicy::default(),
            &access_request(&room, AccessAction::Read)
        ),
        AccessDecision::Allow,
        "读放行"
    );
    assert_eq!(
        provider.check(
            &acb_kernel::AccessPolicy::default(),
            &access_request(&room, AccessAction::Send)
        ),
        AccessDecision::Deny,
        "发送拒绝"
    );
}

#[test]
fn infinite_loop_is_cut_off_and_falls_back_to_the_safe_answer() {
    let dir = TempDir::new("timeout");
    dir.write(
        "spin.lua",
        r#"
return {
  extension = "wake",
  decide = function() while true do end end,
}
"#,
    );
    let plugins = loaded(&dir);
    let id = ProviderId::parse("lua.spin").expect("合法");
    let provider = plugins
        .resolve_wake(&id, &WakePolicy::default())
        .expect("脚本 provider 应当可解析");

    let room = RoomId::new("qq", "group/1").expect("合法");
    assert_eq!(
        provider.decide(&WakePolicy::default(), &wake_context(&room, true, "在吗")),
        WakeDecision::Stay,
        "超时按不唤醒兜底"
    );
    let health = plugins
        .manager_health()
        .into_iter()
        .find(|entry| entry.namespace == LUA_NAMESPACE)
        .expect("lua manager 应当在");
    assert!(
        matches!(health.health, Health::Degraded(_)),
        "一次失败后 health 应当降级：{:?}",
        health.health
    );
}

#[test]
fn broken_script_fails_the_install_with_the_file_name() {
    let dir = TempDir::new("broken");
    dir.write(
        "broken.lua",
        r#"
return { extension = "wake" }
"#,
    );
    let mut plugins = PluginManager::with_native_builtins();
    let error = plugins
        .install_loader(&LuaLoader::new(dir.path()))
        .expect_err("缺 decide 的脚本不该装上");
    let text = error.to_string();
    assert!(text.contains("broken.lua"), "错误要带文件名：{text}");
}

#[test]
fn reload_swaps_the_implementation_without_changing_descriptors() {
    let dir = TempDir::new("reload");
    dir.write(
        "flag.lua",
        r#"
return {
  extension = "wake",
  decide = function() return "stay" end,
}
"#,
    );
    let plugins = loaded(&dir);
    let id = ProviderId::parse("lua.flag").expect("合法");
    let room = RoomId::new("qq", "group/1").expect("合法");
    let decision = |plugins: &PluginManager| {
        plugins
            .resolve_wake(&id, &WakePolicy::default())
            .expect("可解析")
            .decide(&WakePolicy::default(), &wake_context(&room, true, "在吗"))
    };
    assert_eq!(decision(&plugins), WakeDecision::Stay);

    dir.write(
        "flag.lua",
        r#"
return {
  extension = "wake",
  decide = function() return "wake" end,
}
"#,
    );
    assert!(
        plugins
            .reload_managers(&ReloadScope::Namespace(LUA_NAMESPACE.to_owned()))
            .is_empty()
    );
    assert_eq!(decision(&plugins), WakeDecision::Wake, "reload 后换成新实现");

    // 新增脚本会改变 descriptor 集合，reload 应拒绝并提示重启。
    dir.write(
        "extra.lua",
        r#"
return {
  extension = "wake",
  decide = function() return "stay" end,
}
"#,
    );
    let failures = plugins.reload_managers(&ReloadScope::All);
    assert_eq!(failures.len(), 1, "descriptor 集合变化应当报错");
    assert!(failures[0].1.to_string().contains("重启"));
}

#[test]
fn config_can_reference_a_lua_provider() {
    let dir = TempDir::new("config");
    dir.write(
        "mention.lua",
        r#"
return {
  extension = "wake",
  decide = function(options, context) return "stay" end,
}
"#,
    );
    let plugins = loaded(&dir);
    let text = r#"
        [[connections]]
        id = "qq"
        platform = "qq"
        kind = "onebot11"
        url = "ws://127.0.0.1:3001"
        account = "10001"

        [[personas]]
        id = "p"
        name = "P"
        text = "你是桥。"

        [[harnesses]]
        id = "dsh"
        name = "DSH"

        [[sessions]]
        id = "main"
        harness = "dsh"
        persona = "p"
        token = "t-main"

        [[sessions.subscriptions]]
        connection = "qq"
        room = "group/1"

        [sessions.subscriptions.wake]
        provider = "lua.mention"
    "#;
    let config = Config::parse(text).expect("配置合法");
    plugins.validate_config(&config).expect("lua provider 应当能解析");
}

#[test]
fn delivery_composer_runs_from_a_lua_script() {
    let dir = TempDir::new("delivery");
    dir.write(
        "newest.lua",
        r#"
return {
  extension = "delivery",
  compose = function(options, request)
    local keep = {}
    if #request.records > 0 then
      keep[1] = request.records[#request.records].id
    end
    return { keep = keep }
  end,
}
"#,
    );
    let plugins = loaded(&dir);
    let id = ProviderId::parse("lua.newest").expect("合法");
    let composer = plugins
        .resolve_delivery(&id, &DeliveryPolicy::default())
        .expect("Lua composer 应当可解析");

    let room = RoomId::new("qq", "group/1").expect("合法");
    let records = vec![record(1, "第一"), record(2, "第二")];
    let plan = composer.compose(
        &DeliveryPolicy::default(),
        &DeliveryRequest {
            session: "main",
            room: &room,
            records: &records,
            limit: 10,
        },
    );
    assert_eq!(plan.keep, vec![2], "脚本只留最新一条");
    assert_eq!(plan.omitted, None);
}

#[test]
fn delivery_error_falls_back_to_the_default_plan() {
    let dir = TempDir::new("delivery-bad");
    dir.write(
        "bad.lua",
        r#"
return {
  extension = "delivery",
  compose = function() return "nope" end,
}
"#,
    );
    let plugins = loaded(&dir);
    let id = ProviderId::parse("lua.bad").expect("合法");
    let composer = plugins
        .resolve_delivery(&id, &DeliveryPolicy::default())
        .expect("Lua composer 应当可解析");

    let room = RoomId::new("qq", "group/1").expect("合法");
    let records = vec![record(1, "第一"), record(2, "第二")];
    let plan = composer.compose(
        &DeliveryPolicy::default(),
        &DeliveryRequest {
            session: "main",
            room: &room,
            records: &records,
            limit: 10,
        },
    );
    assert_eq!(plan.keep, vec![1, 2], "返回错类型时回退默认选批");
    let health = plugins
        .manager_health()
        .into_iter()
        .find(|entry| entry.namespace == LUA_NAMESPACE)
        .expect("lua manager 应当在");
    assert!(matches!(health.health, Health::Degraded(_)));
}

#[test]
fn send_hook_rewrites_elements_from_a_lua_script() {
    let dir = TempDir::new("send");
    dir.write(
        "sign.lua",
        r#"
return {
  extension = "send",
  before = function(context, elements)
    elements[#elements + 1] = { type = "text", text = " [Lua " .. context.origin .. "]" }
    return elements
  end,
}
"#,
    );
    let plugins = loaded(&dir);
    let id = ProviderId::parse("lua.sign").expect("合法");
    let hook = plugins.resolve_send_hook(&id).expect("Lua send hook 应当可解析");

    let room = RoomId::new("qq", "group/1").expect("合法");
    let context = SendContext {
        session: "main",
        room: &room,
        origin: SendOrigin::Message,
    };
    let mut elements = vec![
        Element::text("你好"),
        Element::Image {
            url: "https://example.com/a.png".to_owned(),
            alt: None,
        },
    ];
    hook.before_send(&context, &mut elements).expect("能改写");
    assert_eq!(elements.len(), 3);
    match &elements[2] {
        Element::Text { text } => assert_eq!(text, " [Lua message]"),
        other => panic!("最后一段应当是文本，收到 {other:?}"),
    }
    assert!(
        matches!(&elements[1], Element::Image { url, .. } if url == "https://example.com/a.png"),
        "图片段必须原样保留"
    );
}

#[test]
fn send_after_error_is_recorded_in_health() {
    let dir = TempDir::new("send-after");
    dir.write(
        "after_fail.lua",
        r#"
return {
  extension = "send",
  after = function() error("boom") end,
}
"#,
    );
    let plugins = loaded(&dir);
    let id = ProviderId::parse("lua.after_fail").expect("合法");
    let hook = plugins.resolve_send_hook(&id).expect("Lua send hook 应当可解析");

    let room = RoomId::new("qq", "group/1").expect("合法");
    let context = SendContext {
        session: "main",
        room: &room,
        origin: SendOrigin::Message,
    };
    hook.after_send(
        &context,
        &Sent {
            platform_id: "p-1".to_owned(),
            at: 0,
        },
    );
    let health = plugins
        .manager_health()
        .into_iter()
        .find(|entry| entry.namespace == LUA_NAMESPACE)
        .expect("lua manager 应当在");
    assert!(matches!(health.health, Health::Degraded(_)));
}

#[test]
fn config_can_reference_lua_delivery_and_send_hooks() {
    let dir = TempDir::new("config-4b");
    dir.write(
        "newest.lua",
        r#"
return {
  extension = "delivery",
  compose = function(options, request)
    return { keep = {} }
  end,
}
"#,
    );
    dir.write(
        "sign.lua",
        r#"
return {
  extension = "send",
  before = function(context, elements) return elements end,
}
"#,
    );
    dir.write(
        "notify.lua",
        r#"
return {
  extension = "turn_error",
  decide = function() return { { kind = "log" } } end,
}
"#,
    );
    dir.write(
        "hourly.lua",
        r#"
return {
  extension = "clock",
  is_due = function() return false end,
}
"#,
    );
    dir.write(
        "no_bots.lua",
        r#"
return {
  extension = "receive",
  decide = function() return "record" end,
}
"#,
    );
    let plugins = loaded(&dir);
    let text = r#"
        [[connections]]
        id = "qq"
        platform = "qq"
        kind = "onebot11"
        url = "ws://127.0.0.1:3001"
        account = "10001"

        [[personas]]
        id = "p"
        name = "P"
        text = "你是桥。"

        [[harnesses]]
        id = "dsh"
        name = "DSH"

        [[sessions]]
        id = "main"
        harness = "dsh"
        persona = "p"
        token = "t-main"

        [sessions.delivery]
        provider = "lua.newest"

        [sessions.outbound]
        before = ["lua.sign"]

        [sessions.turn_error]
        provider = "lua.notify"

        [[sessions.subscriptions]]
        connection = "qq"
        room = "group/1"

        [sessions.subscriptions.clock]
        provider = "lua.hourly"
    "#;
    let config = Config::parse(text).expect("配置合法");
    plugins
        .validate_config(&config)
        .expect("lua 扩展点应当能解析");
}

#[test]
fn turn_error_provider_runs_from_a_lua_script() {
    let dir = TempDir::new("turn-error");
    dir.write(
        "notify.lua",
        r#"
return {
  extension = "turn_error",
  decide = function(request)
    local actions = { { kind = "log" } }
    if request.error ~= "" then
      actions[#actions + 1] = { kind = "send", room = "qq:group/ops" }
    end
    return actions
  end,
}
"#,
    );
    let plugins = loaded(&dir);
    let id = ProviderId::parse("lua.notify").expect("合法");
    let provider = plugins
        .resolve_turn_error(&id, &TurnErrorPolicy::default())
        .expect("Lua turn_error provider 应当可解析");

    let room = RoomId::new("qq", "group/1").expect("合法");
    let request = TurnErrorRequest {
        session: "main",
        room: &room,
        error: "boom",
        actions: &[],
        default_actions: &[],
    };
    let actions = provider.decide(&TurnErrorPolicy::default(), &request);
    assert_eq!(
        actions,
        vec![
            TurnErrorAction::Log,
            TurnErrorAction::Send {
                room: RoomId::new("qq", "group/ops").expect("合法")
            }
        ]
    );
}

#[test]
fn turn_error_error_falls_back_to_default_actions() {
    let dir = TempDir::new("turn-error-bad");
    dir.write(
        "bad.lua",
        r#"
return {
  extension = "turn_error",
  decide = function() return { { kind = "nope" } } end,
}
"#,
    );
    let plugins = loaded(&dir);
    let id = ProviderId::parse("lua.bad").expect("合法");
    let provider = plugins
        .resolve_turn_error(&id, &TurnErrorPolicy::default())
        .expect("Lua turn_error provider 应当可解析");

    let room = RoomId::new("qq", "group/1").expect("合法");
    let session_actions = vec![TurnErrorAction::Log];
    let request = TurnErrorRequest {
        session: "main",
        room: &room,
        error: "boom",
        actions: &session_actions,
        default_actions: &[],
    };
    assert_eq!(
        provider.decide(&TurnErrorPolicy::default(), &request),
        session_actions,
        "脚本返回坏动作时回退默认选择"
    );
    let health = plugins
        .manager_health()
        .into_iter()
        .find(|entry| entry.namespace == LUA_NAMESPACE)
        .expect("lua manager 应当在");
    assert!(matches!(health.health, Health::Degraded(_)));
}

#[test]
fn clock_provider_runs_from_a_lua_script() {
    let dir = TempDir::new("clock");
    dir.write(
        "hourly.lua",
        r#"
return {
  extension = "clock",
  is_due = function(options, context)
    local interval = (options.interval_secs or 3600) * 1000
    return context.now - context.last_clock_at >= interval
  end,
}
"#,
    );
    let plugins = loaded(&dir);
    let id = ProviderId::parse("lua.hourly").expect("合法");
    let policy = ClockPolicy {
        provider: id.clone(),
        interval_secs: Some(60),
        idle_secs: None,
        ..Default::default()
    };
    let provider = plugins
        .resolve_clock(&id, &policy)
        .expect("Lua clock provider 应当可解析");

    let room = RoomId::new("qq", "group/1").expect("合法");
    let context = ClockContext {
        session: "main",
        room: &room,
        now: 30_000,
        last_clock_at: 0,
        last_record_at: None,
    };
    assert!(!provider.is_due(&policy, &context));
    let due = ClockContext {
        now: 61_000,
        ..context
    };
    assert!(provider.is_due(&policy, &due));
}

#[test]
fn shipped_examples_load_and_behave() {
    // examples/ 是给用户照抄的，不能悄悄坏掉；这里真装载、真调用一遍。
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../examples/lua");
    let mut plugins = PluginManager::with_native_builtins();
    plugins
        .install_loader(&LuaLoader::new(&dir))
        .expect("示例脚本应当能装载");

    let ids: Vec<String> = plugins
        .catalog()
        .into_iter()
        .map(|info| info.id.to_string())
        .collect();
    for expected in [
        "lua.quiet_hours",
        "lua.allow_read_only",
        "lua.drop_noise",
        "lua.recent_only",
        "lua.signature",
        "lua.timeout_log_only",
    ] {
        assert!(
            ids.iter().any(|id| id == expected),
            "示例 provider {expected} 没注册；实际有 {ids:?}"
        );
    }

    let room = RoomId::new("qq", "group/1").expect("合法");

    // quiet_hours：本地 08:00 攒够 10 条会醒；23:00 不醒；@ 我任何时间都醒。
    let quiet = ProviderId::parse("lua.quiet_hours").expect("合法");
    let wake = plugins
        .resolve_wake(&quiet, &WakePolicy::default())
        .expect("能解析 quiet_hours");
    let day = WakeContext {
        session: "main",
        room: &room,
        connection: "qq",
        platform: "qq",
        sender_id: "10001",
        text: "闲聊",
        at_self: false,
        reply_to_self: false,
        event: None,
        tags: &[],
        unread: 10,
        now: 0,
    };
    assert_eq!(wake.decide(&WakePolicy::default(), &day), WakeDecision::Wake);

    let night = WakeContext {
        now: 15 * 3_600_000, // UTC 15:00 = UTC+8 的 23:00
        ..day
    };
    assert_eq!(wake.decide(&WakePolicy::default(), &night), WakeDecision::Stay);
    let aimed = WakeContext {
        at_self: true,
        ..night
    };
    assert_eq!(wake.decide(&WakePolicy::default(), &aimed), WakeDecision::Wake);

    // allow_read_only：读放行，发和连接级拒绝。
    let read_only = ProviderId::parse("lua.allow_read_only").expect("合法");
    let access = plugins
        .resolve_access(&read_only, &Default::default())
        .expect("能解析 allow_read_only");
    let policy = Default::default();
    assert_eq!(
        access.check(&policy, &access_request(&room, AccessAction::Read)),
        AccessDecision::Allow
    );
    assert_eq!(
        access.check(&policy, &access_request(&room, AccessAction::Send)),
        AccessDecision::Deny
    );
    assert_eq!(
        access.check(&policy, &access_request(&room, AccessAction::Connection)),
        AccessDecision::Deny
    );

    // drop_noise：以 "!!" 开头的正文被丢掉，其它照收。
    let drop_noise = ProviderId::parse("lua.drop_noise").expect("合法");
    let receive = plugins
        .resolve_receive(&drop_noise, &ReceivePolicy::default())
        .expect("能解析 drop_noise");
    let sender = Sender {
        id: "10001".to_owned(),
        name: "老王".to_owned(),
        role: None,
    };
    let elements = vec![Element::text("!! 刷屏")];
    let noisy = ReceiveRequest {
        connection: "qq",
        platform: "qq",
        room: &room,
        sender: &sender,
        kind: &RecordKind::Message,
        elements: &elements,
        text: "!! 刷屏",
        at_self: false,
    };
    assert_eq!(
        receive.decide(&ReceivePolicy::default(), &noisy),
        ReceiveDecision::Drop
    );
    let normal = ReceiveRequest {
        text: "你好",
        ..noisy
    };
    assert_eq!(
        receive.decide(&ReceivePolicy::default(), &normal),
        ReceiveDecision::Record
    );

    // recent_only：只留最新 keep 条，其余标成跳过。
    let recent_only = ProviderId::parse("lua.recent_only").expect("合法");
    let delivery_policy = DeliveryPolicy {
        provider: recent_only.clone(),
        options: serde_json::json!({ "keep": 2 }),
    };
    let composer = plugins
        .resolve_delivery(&recent_only, &delivery_policy)
        .expect("能解析 recent_only");
    let records = vec![record(1, "一"), record(2, "二"), record(3, "三")];
    let plan = composer.compose(
        &delivery_policy,
        &DeliveryRequest {
            session: "main",
            room: &room,
            records: &records,
            limit: 10,
        },
    );
    assert_eq!(plan.keep, vec![2, 3]);
    assert_eq!(
        plan.omitted,
        Some(Omitted {
            from: 1,
            to: 1,
            count: 1
        })
    );

    // signature：普通回复追加签名，失败通知不动。
    let signature = ProviderId::parse("lua.signature").expect("合法");
    let hook = plugins
        .resolve_send_hook(&signature)
        .expect("能解析 signature");
    let send_context = SendContext {
        session: "main",
        room: &room,
        origin: SendOrigin::Message,
    };
    let mut signed = vec![Element::text("你好")];
    hook.before_send(&send_context, &mut signed)
        .expect("签名钩子不该报错");
    assert_eq!(signed.len(), 2);
    match &signed[1] {
        Element::Text { text } => assert_eq!(text, "\n——来自小助手"),
        other => panic!("最后一段应当是签名文本，收到 {other:?}"),
    }
    let mut failure = vec![Element::text("这轮失败了")];
    hook.before_send(
        &SendContext {
            origin: SendOrigin::TurnFailure,
            ..send_context
        },
        &mut failure,
    )
    .expect("失败通知不该被改写");
    assert_eq!(failure.len(), 1);
    hook.after_send(
        &send_context,
        &Sent {
            platform_id: "p1".into(),
            at: 123,
        },
    );

    // timeout_log_only：超时只记日志，其它失败沿用会话动作。
    let timeout_log = ProviderId::parse("lua.timeout_log_only").expect("合法");
    let turn_error_policy = TurnErrorPolicy {
        provider: timeout_log.clone(),
        ..Default::default()
    };
    let provider = plugins
        .resolve_turn_error(&timeout_log, &turn_error_policy)
        .expect("能解析 timeout_log_only");
    let configured = vec![
        TurnErrorAction::Log,
        TurnErrorAction::Send { room: room.clone() },
    ];
    let defaults = vec![TurnErrorAction::Send { room: room.clone() }];
    let timed_out = TurnErrorRequest {
        session: "main",
        room: &room,
        error: "request timeout after 300s",
        actions: &configured,
        default_actions: &defaults,
    };
    assert_eq!(
        provider.decide(&turn_error_policy, &timed_out),
        vec![TurnErrorAction::Log]
    );
    let failed = TurnErrorRequest {
        error: "boom",
        ..timed_out
    };
    assert_eq!(
        provider.decide(&turn_error_policy, &failed),
        configured
    );

    // options 在第二个参数：会话没配动作时发到 ops_room。
    let with_ops = TurnErrorPolicy {
        provider: timeout_log.clone(),
        options: serde_json::json!({ "ops_room": "qq:group/ops" }),
    };
    let provider = plugins
        .resolve_turn_error(&timeout_log, &with_ops)
        .expect("配置变化能推进已装载的 provider");
    let no_actions = TurnErrorRequest {
        error: "boom",
        actions: &[],
        default_actions: &[],
        ..timed_out
    };
    assert_eq!(
        provider.decide(&with_ops, &no_actions),
        vec![TurnErrorAction::Send {
            room: RoomId::new("qq", "group/ops").expect("合法")
        }]
    );
}

#[test]
fn plugin_options_reach_lua_providers() {
    let dir = TempDir::new("options");
    dir.write(
        "keyword.lua",
        r#"
return {
  extension = "wake",
  decide = function(options, context)
    if context.text == options.extra.keyword then return "wake" end
    return "stay"
  end,
}
"#,
    );
    dir.write(
        "blocked.lua",
        r#"
return {
  extension = "receive",
  decide = function(request, options)
    if request.sender.id == options.extra.blocked then return "drop" end
    return "record"
  end,
}
"#,
    );
    let plugins = loaded(&dir);

    let wake_id = ProviderId::parse("lua.keyword").expect("合法");
    let wake_policy = WakePolicy {
        options: serde_json::json!({ "keyword": "ping" }),
        ..Default::default()
    };
    let wake = plugins
        .resolve_wake(&wake_id, &wake_policy)
        .expect("能解析 keyword");
    let room = RoomId::new("qq", "group/1").expect("合法");
    let hit = wake_context(&room, false, "ping");
    let miss = wake_context(&room, false, "pong");
    assert_eq!(wake.decide(&wake_policy, &hit), WakeDecision::Wake);
    assert_eq!(wake.decide(&wake_policy, &miss), WakeDecision::Stay);

    let receive_id = ProviderId::parse("lua.blocked").expect("合法");
    let receive_policy = ReceivePolicy {
        provider: receive_id.clone(),
        options: serde_json::json!({ "blocked": "bot" }),
    };
    let receive = plugins
        .resolve_receive(&receive_id, &receive_policy)
        .expect("能解析 blocked");
    let bot = Sender {
        id: "bot".to_owned(),
        name: "Bot".to_owned(),
        role: None,
    };
    let human = Sender {
        id: "10001".to_owned(),
        name: "老王".to_owned(),
        role: None,
    };
    let elements = vec![Element::text("你好")];
    let blocked = ReceiveRequest {
        connection: "qq",
        platform: "qq",
        room: &room,
        sender: &bot,
        kind: &RecordKind::Message,
        elements: &elements,
        text: "你好",
        at_self: false,
    };
    assert_eq!(
        receive.decide(&receive_policy, &blocked),
        ReceiveDecision::Drop
    );
    let allowed = ReceiveRequest {
        sender: &human,
        ..blocked
    };
    assert_eq!(
        receive.decide(&receive_policy, &allowed),
        ReceiveDecision::Record
    );
}

#[test]
fn receive_provider_runs_from_a_lua_script() {
    let dir = TempDir::new("receive");
    dir.write(
        "no_bots.lua",
        r#"
return {
  extension = "receive",
  decide = function(request)
    if request.sender.id == "bot" then return "drop" end
    return "record"
  end,
}
"#,
    );
    let plugins = loaded(&dir);
    let id = ProviderId::parse("lua.no_bots").expect("合法");
    let provider = plugins
        .resolve_receive(&id, &ReceivePolicy::default())
        .expect("Lua receive provider 应当可解析");

    let room = RoomId::new("qq", "group/1").expect("合法");
    let bot = Sender {
        id: "bot".to_owned(),
        name: "Bot".to_owned(),
        role: None,
    };
    let human = Sender {
        id: "10001".to_owned(),
        name: "老王".to_owned(),
        role: None,
    };
    let text = "你好";
    let elements = vec![Element::text(text)];
    let request = ReceiveRequest {
        connection: "qq",
        platform: "qq",
        room: &room,
        sender: &bot,
        kind: &RecordKind::Message,
        elements: &elements,
        text,
        at_self: false,
    };
    assert_eq!(provider.decide(&ReceivePolicy::default(), &request), ReceiveDecision::Drop);
    let request = ReceiveRequest {
        sender: &human,
        ..request
    };
    assert_eq!(provider.decide(&ReceivePolicy::default(), &request), ReceiveDecision::Record);
}
