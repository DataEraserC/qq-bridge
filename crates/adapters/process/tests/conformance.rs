//! 外置进程适配器的契约测试。
//!
//! 真启动一个子进程，只用 wire 协议通信：hello、入站事件、发送回执、调用回执、
//! 错误分类、按 shutdown 退出。core 这边只经过正常的 `AdapterFactory` 入口。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use acb_adapter_process::ProcessAdapterFactory;
use acb_kernel::{
    AdapterError, AdapterHost, AdapterInstance, CallError, Connection, Element, EventQueue,
    PluginManager, RoomId, Transport,
};
use serde_json::json;

fn connection(command: Option<&str>) -> Connection {
    Connection {
        id: "fixture".into(),
        platform: "fixture".into(),
        kind: "process".into(),
        transport: Transport::Process,
        url: None,
        options: match command {
            Some(command) => json!({ "command": command }),
            None => json!({}),
        },
        token: None,
        account: "bot".into(),
        admin_qqs: Vec::new(),
        allow_destructive: Default::default(),
        receive: Default::default(),
    }
}

#[test]
fn process_adapter_speaks_the_wire_protocol() {
    let (events, receiver) = EventQueue::channel();
    let mut plugins = PluginManager::with_native_builtins();
    plugins
        .register_adapter(Arc::new(ProcessAdapterFactory))
        .expect("能注册 process 适配器");

    let instance = plugins
        .create_adapter(
            &connection(Some(env!(
                "CARGO_BIN_EXE_acb-adapter-process-fixture"
            ))),
            events.clone(),
        )
        .expect("能启动子进程并读到 hello");

    let AdapterInstance {
        mut outbound,
        launch,
        manifest,
    } = instance;

    // hello 里声明的入站能力要进 manifest，describe_plugins 才看得见。
    assert!(
        manifest.events.iter().any(|event| event.as_ref() == "knock"),
        "{:?}",
        manifest.events
    );
    assert!(
        manifest
            .event_tags
            .iter()
            .any(|tag| tag.as_ref() == "notice"),
        "{:?}",
        manifest.event_tags
    );
    assert!(manifest.supports_at);
    assert!(manifest.supports_reply);
    assert_eq!(manifest.platform.as_ref(), "fixture");

    let catalog = outbound.catalog("fixture");
    assert!(
        catalog
            .iter()
            .any(|method| method.name.as_ref() == "fixture.echo"),
        "目录要带过来"
    );

    let shutdown = Arc::new(AtomicBool::new(false));
    let host = AdapterHost {
        events,
        shutdown: Arc::clone(&shutdown),
    };
    thread::spawn(move || launch(host));

    // 子进程启动后主动发一条入站事件。
    let draft = receiver
        .recv_timeout(Duration::from_secs(2))
        .expect("应当收到子进程的入站事件");
    assert_eq!(draft.room.to_string(), "fixture:group/1");
    assert_eq!(draft.platform_id.as_deref(), Some("fx-event"));

    let room = RoomId::new("fixture", "group/1").expect("合法");

    let sent = outbound
        .send(&room, &[Element::text("hi")])
        .expect("发送应当成功");
    assert_eq!(sent.platform_id, "fx-1");

    let echoed = outbound
        .call(&room, "fixture.echo", &json!({ "text": "回显正文" }))
        .expect("调用应当成功");
    assert_eq!(echoed.value["text"], "回显正文");
    assert!(echoed.record.is_none());

    let sent_call = outbound
        .call(&room, "fixture.send", &json!({ "text": "记一笔" }))
        .expect("发送类调用应当成功");
    let record = sent_call.record.expect("发送类调用要带流水");
    assert_eq!(record.elements.len(), 1);

    let failure = outbound
        .call(&room, "fixture.fail", &json!({}))
        .expect_err("fixture 故意失败");
    assert!(matches!(failure, CallError::BadParams(_)), "{failure:?}");

    let unknown = outbound
        .call(&room, "fixture.nope", &json!({}))
        .expect_err("没声明的方法应当失败");
    assert!(
        matches!(unknown, CallError::UnknownMethod { .. }),
        "{unknown:?}"
    );

    shutdown.store(true, Ordering::Relaxed);
    thread::sleep(Duration::from_millis(300));
}

#[test]
fn process_adapter_restarts_after_child_exits() {
    let (events, receiver) = EventQueue::channel();
    let mut plugins = PluginManager::with_native_builtins();
    plugins
        .register_adapter(Arc::new(ProcessAdapterFactory))
        .expect("能注册 process 适配器");

    let instance = plugins
        .create_adapter(
            &connection(Some(env!(
                "CARGO_BIN_EXE_acb-adapter-process-fixture"
            ))),
            events.clone(),
        )
        .expect("能启动子进程");

    let AdapterInstance {
        mut outbound,
        launch,
        manifest: _,
    } = instance;
    let shutdown = Arc::new(AtomicBool::new(false));
    let host = AdapterHost {
        events,
        shutdown: Arc::clone(&shutdown),
    };
    thread::spawn(move || launch(host));

    // 先等第一条事件，确认进程已经在跑。
    let _ = receiver
        .recv_timeout(Duration::from_secs(2))
        .expect("应当收到子进程的入站事件");
    let room = RoomId::new("fixture", "group/1").expect("合法");

    // 让子进程自己退出；这次调用拿不到回执是预期行为。
    let _ = outbound.call(&room, "fixture.exit", &json!({}));

    // 退避重启后，原来的 outbound 句柄要能继续用（catalog 和 stdin 都换新的）。
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        match outbound.call(&room, "fixture.echo", &json!({ "text": "after restart" })) {
            Ok(result) => {
                assert_eq!(result.value["text"], "after restart");
                break;
            }
            Err(error) => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "子进程没有自动重启，最后一次错误：{error:?}"
                );
                thread::sleep(Duration::from_millis(100));
            }
        }
    }

    shutdown.store(true, Ordering::Relaxed);
    thread::sleep(Duration::from_millis(300));
}

#[test]
fn missing_command_is_a_startup_error() {
    let (events, _receiver) = EventQueue::channel();
    let mut plugins = PluginManager::with_native_builtins();
    plugins
        .register_adapter(Arc::new(ProcessAdapterFactory))
        .expect("能注册");
    let error = plugins
        .create_adapter(&connection(None), events)
        .expect_err("没有 command 不该启动");
    match error {
        AdapterError::Create(connection, detail) => {
            assert_eq!(connection, "fixture");
            assert!(detail.contains("command"), "{detail}");
        }
        other => panic!("应当是启动失败，收到 {other:?}"),
    }
}
