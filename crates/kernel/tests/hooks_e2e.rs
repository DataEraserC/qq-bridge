//! 钩子的端到端接线：真配置文件、真脚本文件、真管线。
//!
//! 与 `hooks.rs` 里的单元测试的区别是这里走的是**文档描述的启用路径**——配置里的
//! `hooks = "hooks/alice.lua"`、相对配置文件所在目录解析、启动时一次性加载。单元测试
//! 直接喂源码字符串，抓不到「路径解析错了」这类问题。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use time::OffsetDateTime;

use qqab_kernel::archive::{Limits, Message};
use qqab_kernel::config::{Config, Device, ScopeConfig};
use qqab_kernel::hooks::{HookError, HookRuntime, Hooks};
use qqab_kernel::pipeline::{Action, Pipeline};

const CONFIG: &str = r#"
[[accounts]]
id = "main"
self_id = "10001"
ws_url = "ws://127.0.0.1:6700"

[[personas]]
id = "whale"
text = "你是……"

[[devices]]
id = "alice-phone"
persona = "whale"
memory = "alice"
accounts = ["main"]
hooks = "hooks/alice.lua"

  [[devices.scopes]]
  conversation = "20001"
  receive = ["main"]
  send = ["main"]

  [devices.wake]
  at_mention = true
"#;

fn now() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_792_561_540).expect("合法时间戳")
}

/// 每个用例一个独立目录。
///
/// 用进程 id 拼目录会让并行的用例互相覆盖——那几个跑得快的用例已经在共享同一个
/// `hooks/alice.lua`，一个写出半截语法、另一个读到的就是坏脚本。
fn unique_dir(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "qqab-hooks-{}-{}-{}",
        std::process::id(),
        tag,
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("应能建目录");
    dir
}

/// 一个带脚本文件的临时配置目录。返回目录，调用方负责删。
fn fixture(tag: &str, script: &str) -> PathBuf {
    let dir = unique_dir(tag);
    std::fs::create_dir_all(dir.join("hooks")).expect("应能建目录");
    std::fs::write(dir.join("hooks/alice.lua"), script).expect("应能写脚本");
    dir
}

fn pipeline_with(dir: &Path) -> (Pipeline, Config) {
    let config = Config::parse(CONFIG).expect("配置应能解析");
    let hooks: BTreeMap<String, Box<dyn HookRuntime>> = Hooks::load_all(&config, dir)
        .expect("应能加载脚本")
        .into_iter()
        .map(|(device, hooks)| (device, Box::new(hooks) as Box<dyn HookRuntime>))
        .collect();
    let pipeline =
        Pipeline::new(Limits::default(), 100, time::Duration::minutes(10)).with_hooks(hooks);
    (pipeline, config)
}

fn device(config: &Config) -> Device {
    config.devices[0].clone()
}

fn scope(config: &Config) -> ScopeConfig {
    config.devices[0].scopes[0].clone()
}

fn message(id: &str, text: &str) -> Message {
    Message {
        account: "main".to_owned(),
        platform: "qq".to_owned(),
        group: "20001".to_owned(),
        message_id: id.to_owned(),
        sender_id: "30002".to_owned(),
        text: text.to_owned(),
        at_self: true,
        at: 1_792_561_540_000,
        ..Message::default()
    }
}

#[test]
fn script_path_resolves_relative_to_the_config_dir() {
    let dir = fixture("path", "local x = 1\n");
    let (_, config) = pipeline_with(&dir);
    // 设备 id 对上，说明 `load_all` 找到了设备声明的那一个。
    assert!(
        Hooks::load_all(&config, &dir)
            .unwrap()
            .iter()
            .any(|(id, _)| id == "alice-phone")
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn missing_script_fails_the_whole_startup() {
    let dir = unique_dir("missing");
    let config = Config::parse(CONFIG).expect("配置应能解析");

    let error = Hooks::load_all(&config, &dir).unwrap_err();
    assert!(matches!(error, HookError::Read { .. }), "得到 {error:?}");
    // 提示里要有设备与路径，否则排不出是哪一个。
    assert!(error.to_string().contains("alice-phone"));
    assert!(error.to_string().contains("hooks/alice.lua"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn syntax_error_fails_before_the_process_starts_serving() {
    let dir = fixture("syntax", "function preReceive(ev\n");
    let config = Config::parse(CONFIG).expect("配置应能解析");
    let error = Hooks::load_all(&config, &dir).unwrap_err();
    assert!(matches!(error, HookError::Syntax { .. }), "得到 {error:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn devices_without_hooks_get_no_environment() {
    let dir = fixture("nohooks", "local x = 1\n");
    let config =
        Config::parse(&CONFIG.replace("hooks = \"hooks/alice.lua\"\n", "")).expect("配置应能解析");
    assert!(
        Hooks::load_all(&config, &dir).expect("应能加载").is_empty(),
        "没声明 hooks 的设备不该建环境"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn documented_drop_example_takes_effect_end_to_end() {
    // 与 docs/manual/04-hooks.md 的「丢弃空消息」示例同义。
    let dir = fixture(
        "drop",
        r#"function preReceive(ev)
             if ev.text == "" then return nil end
             return ev
           end
"#,
    );
    let (mut pipeline, config) = pipeline_with(&dir);

    assert_eq!(
        pipeline.ingest(
            &device(&config),
            &scope(&config),
            "你是……",
            message("1", ""),
            now(),
            0.0
        ),
        Action::Stay,
        "空正文应被脚本丢掉"
    );
    assert_eq!(pipeline.archive().retained("alice"), 0);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn documented_veto_example_takes_effect_end_to_end() {
    // 「深夜只响应 @」同义：这里把任何非 @ 的唤醒都否决。
    let dir = fixture(
        "veto",
        r#"function shouldWake(ev, reason)
             if reason ~= "at_mention" then return nil end
             return reason
           end
"#,
    );
    let (mut pipeline, config) = pipeline_with(&dir);

    // 先来一条普通消息：规则不命中，脚本也没放行。
    assert_eq!(
        pipeline.ingest(
            &device(&config),
            &scope(&config),
            "你是……",
            Message {
                at_self: false,
                ..message("1", "随便聊聊")
            },
            now(),
            0.0
        ),
        Action::Stay
    );

    // @ 进来：规则命中，脚本放行，原因保持 at_mention。
    let Action::Deliver(delivery) = pipeline.ingest(
        &device(&config),
        &scope(&config),
        "你是……",
        message("2", "在吗"),
        now(),
        0.0,
    ) else {
        panic!("@ 应能唤醒");
    };
    assert_eq!(delivery.wake_reason, "at_mention");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn hook_state_is_per_device_not_global() {
    // 两个设备各有一份环境：脚本里的上值互不可见。
    let dir = fixture(
        "state",
        r#"local count = 0
           function postReceive(ev, batch) count = count + 1 end
           function shouldWake(ev, reason) return "count:" .. count end
"#,
    );
    let config = Config::parse(CONFIG).expect("配置应能解析");
    let hooks = Hooks::load_all(&config, &dir).expect("应能加载");
    assert_eq!(hooks.len(), 1);

    // 同一份源码、两个不同的设备 id：各自从 0 开始。
    let mut a = Hooks::from_source(
        "alice-phone",
        "local count = 0\nfunction shouldWake(ev, reason) return 'count:' .. count end",
        "a.lua",
        Default::default(),
    )
    .unwrap();
    let mut b = Hooks::from_source(
        "bob-phone",
        "local count = 0\nfunction shouldWake(ev, reason) return 'count:' .. count end",
        "b.lua",
        Default::default(),
    )
    .unwrap();
    let event = qqab_kernel::hooks::Snapshot::of_poke("qq", "20001", "30002", 0);
    assert_eq!(
        a.should_wake(&event.view(), &[], None),
        Ok(Some("count:0".into()))
    );
    assert_eq!(
        b.should_wake(&event.view(), &[], None),
        Ok(Some("count:0".into()))
    );
    let _ = std::fs::remove_dir_all(&dir);
}
