//! LuaManager / LuaLoader 的端到端测试：真写脚本、真装 manager、真调 provider。

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use acb_kernel::{
    AccessAction, AccessRequest, Config, PluginManager, ProviderId, ReloadScope, RoomId,
    WakeContext, WakeDecision, WakePolicy,
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
