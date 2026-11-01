//! 动态库适配器的契约测试。
//!
//! 真的用 libloading 打开本 crate 编出来的 cdylib，走 C ABI + 共用 wire：
//! manifest、describe 目录、入站事件、发送/调用回执、错误分类、shutdown。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use acb_adapter_dynamic::DynamicLoader;
use acb_kernel::{
    AdapterHost, AdapterInstance, CallError, Connection, Element, EventQueue, PluginConfig,
    PluginManager, PluginSource, RoomId, Transport,
};
use serde_json::json;

/// 找到本 crate 编出来的 cdylib。
///
/// Cargo 不会在编译期给测试设 `CARGO_CDYLIB_FILE_*`，所以运行时先看环境变量，
/// 再按测试二进制的位置推 `target/<profile>/libacb_adapter_dynamic_fixture.so`。
fn fixture_path() -> std::path::PathBuf {
    if let Ok(path) = std::env::var("CARGO_CDYLIB_FILE_acb_adapter_dynamic_fixture") {
        return path.into();
    }
    let executable = std::env::current_exe().expect("测试二进制路径");
    let profile_dir = executable
        .parent()
        .and_then(|deps| deps.parent())
        .expect("测试二进制在 target/<profile>/deps 下");
    let name = if cfg!(target_os = "macos") {
        "libacb_adapter_dynamic_fixture.dylib"
    } else if cfg!(windows) {
        "acb_adapter_dynamic_fixture.dll"
    } else {
        "libacb_adapter_dynamic_fixture.so"
    };
    profile_dir.join(name)
}

fn connection() -> Connection {
    Connection {
        id: "demo".into(),
        platform: "demo".into(),
        kind: "demo".into(),
        transport: Transport::Other("dynamic".into()),
        url: None,
        options: json!({}),
        token: None,
        account: "bot".into(),
        admin_qqs: Vec::new(),
        allow_destructive: Default::default(),
        receive: Default::default(),
    }
}

#[test]
fn dynamic_adapter_loads_and_speaks_the_wire_protocol() {
    // `cargo test` 不保证重建同包的 cdylib；每次先补一次，保证测的是当前源码。
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../Cargo.toml");
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let status = std::process::Command::new(cargo)
        .arg("build")
        .arg("--manifest-path")
        .arg(manifest)
        .arg("-p")
        .arg("acb-adapter-dynamic-fixture")
        .status()
        .expect("能跑 cargo build");
    assert!(status.success(), "fixture 动态库构建失败");

    let path = fixture_path();
    assert!(path.exists(), "缺 fixture 动态库：{}", path.display());
    let path = path.to_string_lossy().into_owned();

    let mut plugins = PluginManager::with_native_builtins();
    plugins
        .install_loader(&DynamicLoader::new(&[PluginConfig {
            path,
            kind: Some("demo".into()),
        }]))
        .expect("能装载动态库");

    let info = plugins
        .catalog()
        .into_iter()
        .find(|info| info.kind.as_deref() == Some("demo"))
        .expect("目录里有动态适配器");
    assert_eq!(info.metadata.title.as_deref(), Some("动态库夹具"));
    assert_eq!(info.source, PluginSource::Dynamic);

    let (events, receiver) = EventQueue::channel();
    let instance = plugins
        .create_adapter(&connection(), events.clone())
        .expect("能创建动态适配器");
    let AdapterInstance {
        mut outbound,
        launch,
        manifest: _,
    } = instance;

    let catalog = outbound.catalog("demo");
    assert!(
        catalog
            .iter()
            .any(|method| method.name.as_ref() == "demo.echo"),
        "describe 要带回目录"
    );

    let shutdown = Arc::new(AtomicBool::new(false));
    let host = AdapterHost {
        events,
        shutdown: Arc::clone(&shutdown),
    };
    thread::spawn(move || launch(host));

    let draft = receiver
        .recv_timeout(Duration::from_secs(2))
        .expect("应当收到插件发来的入站事件");
    assert_eq!(draft.room.to_string(), "demo:group/1");
    assert_eq!(draft.platform_id.as_deref(), Some("dyn-event"));

    let room = RoomId::new("demo", "group/1").expect("合法");

    let sent = outbound
        .send(&room, &[Element::text("hi")])
        .expect("发送应当成功");
    assert_eq!(sent.platform_id, "dyn-1");

    let echoed = outbound
        .call(&room, "demo.echo", &json!({ "text": "回显正文" }))
        .expect("调用应当成功");
    assert_eq!(echoed.value["text"], "回显正文");

    let sent_call = outbound
        .call(&room, "demo.send", &json!({ "text": "记一笔" }))
        .expect("发送类调用应当成功");
    assert_eq!(
        sent_call.record.expect("要带流水").elements.len(),
        1
    );

    let failure = outbound
        .call(&room, "demo.fail", &json!({}))
        .expect_err("fixture 故意失败");
    assert!(matches!(failure, CallError::BadParams(_)), "{failure:?}");

    let unknown = outbound
        .call(&room, "demo.nope", &json!({}))
        .expect_err("没声明的方法应当失败");
    assert!(
        matches!(unknown, CallError::UnknownMethod { .. }),
        "{unknown:?}"
    );

    // 插件睡过头：调用方必须在 CALL_TIMEOUT 内拿回错误，而不是陪它一起卡住。
    let started = std::time::Instant::now();
    let timeout = outbound
        .call(&room, "demo.slow", &json!({}))
        .expect_err("应当超时");
    assert!(
        started.elapsed() < Duration::from_millis(2400),
        "超时控制在 2 秒内，实际 {:?}",
        started.elapsed()
    );
    assert!(
        matches!(&timeout, CallError::Unavailable(detail) if detail.contains("超时")),
        "{timeout:?}"
    );
    // 等插件线程自己睡醒，避免卸载库时它还在执行代码。
    thread::sleep(Duration::from_millis(800));

    shutdown.store(true, Ordering::Relaxed);
    thread::sleep(Duration::from_millis(300));
}
