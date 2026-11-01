//! wake provider 走动态库 ABI + 共用 wire 的契约测试。

use acb_adapter_dynamic::DynamicLoader;
use acb_kernel::{
    PluginConfig, PluginManager, PluginSource, ProviderId, RoomId, WakeContext, WakeDecision,
    WakePolicy,
};

fn fixture_path() -> std::path::PathBuf {
    if let Ok(path) = std::env::var("CARGO_CDYLIB_FILE_acb_adapter_wake_fixture") {
        return path.into();
    }
    let executable = std::env::current_exe().expect("测试二进制路径");
    let profile_dir = executable
        .parent()
        .and_then(|deps| deps.parent())
        .expect("测试二进制在 target/<profile>/deps 下");
    let name = if cfg!(target_os = "macos") {
        "libacb_adapter_wake_fixture.dylib"
    } else if cfg!(windows) {
        "acb_adapter_wake_fixture.dll"
    } else {
        "libacb_adapter_wake_fixture.so"
    };
    profile_dir.join(name)
}

#[test]
fn wake_provider_uses_the_shared_wire() {
    // `cargo test` 不保证重建同包的 cdylib；每次先补一次，保证测的是当前源码。
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../Cargo.toml");
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let status = std::process::Command::new(cargo)
        .arg("build")
        .arg("--manifest-path")
        .arg(manifest)
        .arg("-p")
        .arg("acb-adapter-wake-fixture")
        .status()
        .expect("能跑 cargo build");
    assert!(status.success(), "wake fixture 构建失败");

    let path = fixture_path();
    assert!(path.exists(), "缺 wake fixture：{}", path.display());

    let mut plugins = PluginManager::with_native_builtins();
    plugins
        .install_loader(&DynamicLoader::new(&[PluginConfig {
            path: path.to_string_lossy().into_owned(),
            kind: None,
        }]))
        .expect("能装载 wake 插件");

    let id = ProviderId::parse("fixture.wake").expect("provider id 合法");
    let info = plugins
        .catalog()
        .into_iter()
        .find(|info| info.id == id)
        .expect("目录里有 wake provider");
    assert_eq!(info.metadata.title.as_deref(), Some("wake 夹具"));
    assert_eq!(info.source, PluginSource::Dynamic);
    assert_eq!(
        info.metadata.effects.iter().map(|effect| effect.as_str()).collect::<Vec<_>>(),
        vec!["may_wake", "may_stay_silent"]
    );

    let provider = plugins
        .resolve_wake(&id, &WakePolicy::default())
        .expect("能解析 wake provider");

    let room = RoomId::new("qq", "group/1").expect("合法");
    let context = WakeContext {
        session: "main",
        room: &room,
        connection: "qq",
        platform: "qq",
        sender_id: "10001",
        text: "在吗",
        at_self: false,
        reply_to_self: false,
        event: None,
        tags: &[],
        unread: 1,
        last_wake_at: 0,
        now: 0,
    };
    let policy = WakePolicy::default();

    let aimed = WakeContext {
        at_self: true,
        ..context
    };
    assert_eq!(provider.decide(&policy, &aimed), WakeDecision::Wake);
    let verdict = provider.decide_with_reason(&policy, &aimed);
    assert_eq!(verdict.decision, WakeDecision::Wake);
    assert_eq!(
        verdict.reason.as_deref(),
        Some("fixture：@ 或关键词 fixture 命中")
    );

    let plain = WakeContext {
        text: "今天天气不错",
        ..context
    };
    assert_eq!(provider.decide(&policy, &plain), WakeDecision::Stay);

    let named = WakeContext {
        text: "fixture 在吗",
        ..context
    };
    assert_eq!(provider.decide(&policy, &named), WakeDecision::Wake);

    // 插件专属 options 走 wire 的 options.extra，原样到动态库里。
    let custom = WakePolicy {
        options: serde_json::json!({ "keyword": "needle" }),
        ..WakePolicy::default()
    };
    let needle = WakeContext {
        text: "带 needle 的话",
        ..context
    };
    assert_eq!(provider.decide(&custom, &needle), WakeDecision::Wake);
    let old_keyword = WakeContext {
        text: "只说 fixture 不算",
        ..context
    };
    assert_eq!(provider.decide(&custom, &old_keyword), WakeDecision::Stay);
}
