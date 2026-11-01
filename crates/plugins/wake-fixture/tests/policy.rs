//! access / receive / clock 策略走动态库 ABI + 共用 wire 的契约测试。
//!
//! 同一个 cdylib 通过环境变量切换 manifest 的扩展点；这个测试文件只有一个测试函数，
//! 所以进程内的环境变量不会和别的测试打架。

use std::path::PathBuf;

use acb_adapter_dynamic::DynamicLoader;
use acb_kernel::{
    AccessAction, AccessDecision, AccessPolicy, AccessRequest, ClockContext, ClockPolicy,
    DeliveryPolicy, DeliveryRequest, Element, Omitted, PluginConfig, PluginManager, PluginSource,
    ProviderId, ReceiveDecision, ReceivePolicy, ReceiveRequest, Record, RecordKind, RoomId,
    SendContext, SendOrigin, Sender, Sent, TurnErrorAction, TurnErrorPolicy, TurnErrorRequest,
};

fn fixture_path() -> PathBuf {
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

fn load(path: &PathBuf) -> PluginManager {
    let mut plugins = PluginManager::with_native_builtins();
    plugins
        .install_loader(&DynamicLoader::new(&[PluginConfig {
            path: path.to_string_lossy().into_owned(),
            kind: None,
        }]))
        .expect("能装载策略插件");
    plugins
}

fn record(id: u64, text: &str, room: &RoomId, sender: &Sender) -> Record {
    Record {
        id,
        room: room.clone(),
        kind: RecordKind::Message,
        sender: sender.clone(),
        at: id as i64,
        elements: vec![Element::text(text)],
        at_self: false,
        reply_to_self: false,
        platform_id: None,
        quote_ref: None,
        labels: Vec::new(),
        redelivery: None,
        note: None,
    }
}

#[test]
fn decision_providers_use_the_shared_wire() {
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
    assert!(status.success(), "策略 fixture 构建失败");

    let path = fixture_path();
    assert!(path.exists(), "缺策略 fixture：{}", path.display());

    let room = RoomId::new("qq", "group/1").expect("房间合法");
    let sender = Sender {
        id: "10001".to_owned(),
        name: "老王".to_owned(),
        role: None,
    };

    // access：读放行、写拒绝。
    unsafe { std::env::set_var("ACB_FIXTURE_EXTENSION", "access") };
    {
        let plugins = load(&path);
        let id = ProviderId::parse("fixture.access").expect("id 合法");
        let info = plugins
            .catalog()
            .into_iter()
            .find(|info| info.id == id)
            .expect("目录里有 access provider");
        assert_eq!(info.metadata.title.as_deref(), Some("access 夹具"));
        assert_eq!(info.source, PluginSource::Dynamic);

        let policy = AccessPolicy {
            provider: id.clone(),
            rooms: Vec::new(),
            ..Default::default()
        };
        let provider = plugins
            .resolve_access(&id, &policy)
            .expect("能解析 access provider");
        let request = AccessRequest {
            session: "main",
            connection: "qq",
            room: Some(&room),
            action: AccessAction::Read,
            subscriptions: std::slice::from_ref(&room),
        };
        assert_eq!(provider.check(&policy, &request), AccessDecision::Allow);
        let write = AccessRequest {
            action: AccessAction::Send,
            ..request
        };
        assert_eq!(provider.check(&policy, &write), AccessDecision::Deny);
    }

    // receive：含 drop 的正文丢掉，其余照收。
    unsafe { std::env::set_var("ACB_FIXTURE_EXTENSION", "receive") };
    {
        let plugins = load(&path);
        let id = ProviderId::parse("fixture.receive").expect("id 合法");
        let policy = ReceivePolicy {
            provider: id.clone(),
            ..Default::default()
        };
        let provider = plugins
            .resolve_receive(&id, &policy)
            .expect("能解析 receive provider");
        let kind = RecordKind::Message;
        let elements = vec![Element::text("你好")];
        let request = ReceiveRequest {
            connection: "qq",
            platform: "qq",
            room: &room,
            sender: &sender,
            kind: &kind,
            elements: &elements,
            text: "你好",
            at_self: false,
        };
        assert_eq!(provider.decide(&policy, &request), ReceiveDecision::Record);
        let dropped = ReceiveRequest {
            text: "drop this",
            ..request
        };
        assert_eq!(provider.decide(&policy, &dropped), ReceiveDecision::Drop);
    }

    // clock：距上次开轮满 60 秒才 due。
    unsafe { std::env::set_var("ACB_FIXTURE_EXTENSION", "clock") };
    {
        let plugins = load(&path);
        let id = ProviderId::parse("fixture.clock").expect("id 合法");
        let policy = ClockPolicy {
            provider: id.clone(),
            interval_secs: None,
            idle_secs: None,
            ..Default::default()
        };
        let provider = plugins
            .resolve_clock(&id, &policy)
            .expect("能解析 clock provider");
        let context = ClockContext {
            session: "main",
            room: &room,
            now: 60_000,
            last_clock_at: 0,
            last_wake_at: 0,
            last_record_at: None,
        };
        assert!(provider.is_due(&policy, &context));
        let early = ClockContext { now: 1_000, ..context };
        assert!(!provider.is_due(&policy, &early));
    }

    // delivery：只保留最后 N 条，其余明确标成跳过。
    unsafe { std::env::set_var("ACB_FIXTURE_EXTENSION", "delivery") };
    {
        let plugins = load(&path);
        let id = ProviderId::parse("fixture.delivery").expect("id 合法");
        let policy = DeliveryPolicy {
            provider: id.clone(),
            options: serde_json::json!({ "keep_last": 1 }),
        };
        let records = vec![
            record(1, "第一条", &room, &sender),
            record(2, "第二条", &room, &sender),
            record(3, "第三条", &room, &sender),
        ];
        let request = DeliveryRequest {
            session: "main",
            room: &room,
            records: &records,
            limit: 10,
        };
        let plan = plugins
            .resolve_delivery(&id, &policy)
            .expect("能解析 delivery composer")
            .compose(&policy, &request);
        assert_eq!(plan.keep, vec![3]);
        assert_eq!(
            plan.omitted,
            Some(Omitted {
                from: 1,
                to: 2,
                count: 2
            })
        );
    }

    // turn_error：插件能按 options 选动作，配置变化能推进已缓存的实例。
    unsafe { std::env::set_var("ACB_FIXTURE_EXTENSION", "turn_error") };
    {
        let plugins = load(&path);
        let id = ProviderId::parse("fixture.turn_error").expect("id 合法");
        let actions = vec![
            TurnErrorAction::Log,
            TurnErrorAction::Send { room: room.clone() },
        ];
        let default_actions = vec![TurnErrorAction::Send { room: room.clone() }];
        let request = TurnErrorRequest {
            session: "main",
            room: &room,
            error: "boom",
            actions: &actions,
            default_actions: &default_actions,
        };

        let only_log = TurnErrorPolicy {
            provider: id.clone(),
            options: serde_json::json!({ "only": "log" }),
        };
        let provider = plugins
            .resolve_turn_error(&id, &only_log)
            .expect("能解析 turn_error provider");
        assert_eq!(
            provider.decide(&only_log, &request),
            vec![TurnErrorAction::Log]
        );

        let prefer_default = TurnErrorPolicy {
            provider: id.clone(),
            options: serde_json::json!({ "only": "default" }),
        };
        let provider = plugins
            .resolve_turn_error(&id, &prefer_default)
            .expect("同一个 provider 能复用");
        assert_eq!(provider.decide(&prefer_default, &request), default_actions);
    }

    // send：before 改写元素，after 只观察回执。
    unsafe { std::env::set_var("ACB_FIXTURE_EXTENSION", "send") };
    {
        let plugins = load(&path);
        let id = ProviderId::parse("fixture.send").expect("id 合法");
        let hook = plugins
            .resolve_send_hook(&id)
            .expect("能解析发送钩子");
        let context = SendContext {
            session: "main",
            room: &room,
            origin: SendOrigin::Message,
        };
        let mut elements = vec![Element::text("正文")];
        hook.before_send(&context, &mut elements)
            .expect("before 钩子能改写正文");
        assert_eq!(elements.len(), 2);
        match &elements[1] {
            Element::Text { text } => assert_eq!(text, " [fixture-hook]"),
            other => panic!("应当是追加的文本段，收到 {other:?}"),
        }
        hook.after_send(
            &context,
            &Sent {
                platform_id: "p1".into(),
                at: 123,
            },
        );
    }
}
