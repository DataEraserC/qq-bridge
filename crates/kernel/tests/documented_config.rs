//! 文档里的配置示例必须真的能被解析。
//!
//! 这些测试的失败意味着文档与实现漂移，而不是实现有 bug。

use qsh_kernel::config::{Config, PersonaSource};

#[test]
fn documented_config_parses() {
    let text = include_str!("fixtures/documented.toml");
    let config = Config::parse(text).expect("文档里的配置示例必须能解析");

    assert_eq!(config.accounts.len(), 2);
    assert_eq!(config.personas.len(), 1);
    assert_eq!(config.devices.len(), 2);

    let phone = config
        .devices
        .iter()
        .find(|device| device.id == "alice-phone")
        .expect("应有 alice-phone");
    assert_eq!(phone.memory, "alice");
    assert_eq!(phone.accounts, vec!["main", "backup"]);
    assert_eq!(phone.scopes.len(), 1);
    assert_eq!(phone.scopes[0].receive, vec!["main"]);
    assert_eq!(phone.scopes[0].send, vec!["main", "backup"]);

    // 共享记忆：两个设备同一个记忆名。
    let pc = config
        .devices
        .iter()
        .find(|device| device.id == "alice-pc")
        .expect("应有 alice-pc");
    assert_eq!(pc.memory, phone.memory);

    assert!(!phone.wake.any_message);
    assert_eq!(phone.wake.keywords, vec!["小鲸鱼", "上线"]);
    assert_eq!(phone.wake.probability, 0.05);
    assert_eq!(phone.wake.idle_prompt_after_min, Some(45));
    assert!(phone.wake.sleep_until.is_some());
    assert!(phone.wake.ignore_speakers.contains("30002"));

    // 省略 [devices.wake] 的设备拿到文档写的缺省：只响应 @ 与名字。
    assert!(pc.wake.at_mention);
    assert!(pc.wake.name_mention);
    assert!(!pc.wake.question);
    assert_eq!(pc.wake.probability, 0.0);
}

#[test]
fn documented_persona_source_is_inline() {
    let text = include_str!("fixtures/documented.toml");
    let config = Config::parse(text).expect("应能解析");
    match &config.personas[0].source {
        PersonaSource::Inline(value) => assert_eq!(value, "你是一个……"),
        PersonaSource::File(path) => panic!("期望内联人格，得到文件 {path}"),
    }
}

#[test]
fn env_prefixed_token_is_not_inlined() {
    let text = include_str!("fixtures/documented.toml");
    let config = Config::parse(text).expect("应能解析");
    let main = config
        .accounts
        .iter()
        .find(|account| account.id == "main")
        .expect("应有 main");

    // token 写成 env: 前缀时不进内存明文，只记变量名。
    let token = main.token.as_ref().expect("main 应有 token");
    assert!(format!("{token:?}").contains("QQ_TOKEN"));
}
