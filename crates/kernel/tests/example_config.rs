//! 示例配置必须始终能被解析——它是用户复制粘贴的起点，过期了就是 bug。

use std::path::PathBuf;

use qqab_kernel::Config;

fn example_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config.example.toml")
}

#[test]
fn example_config_parses_and_covers_two_platforms() {
    let text = std::fs::read_to_string(example_path()).expect("示例配置应当存在");
    let config = Config::parse(&text).expect("示例配置应当能解析");

    assert_eq!(config.devices.len(), 1);
    let device = &config.devices[0];
    assert_eq!(device.id, "main");
    assert_eq!(device.subscriptions.len(), 2);

    let platforms: Vec<&str> = device
        .subscriptions
        .iter()
        .map(|subscription| subscription.room.platform.as_str())
        .collect();
    assert!(platforms.contains(&"qq"));
    assert!(platforms.contains(&"discord"));
}

#[test]
fn example_config_demonstrates_per_subscription_wake_rules() {
    let text = std::fs::read_to_string(example_path()).expect("示例配置应当存在");
    let config = Config::parse(&text).expect("示例配置应当能解析");
    let subscriptions = &config.devices[0].subscriptions;

    let qq = subscriptions
        .iter()
        .find(|subscription| subscription.room.platform == "qq")
        .expect("应当有 QQ 订阅");
    let discord = subscriptions
        .iter()
        .find(|subscription| subscription.room.platform == "discord")
        .expect("应当有 Discord 订阅");

    assert!(qq.wake.poke, "QQ 那边被拍要醒");
    assert!(!discord.wake.poke, "Discord 那边被拍不醒");
    assert_ne!(
        qq.wake.accumulate, discord.wake.accumulate,
        "两个房间的攒条阈值可以不同"
    );
    assert_eq!(discord.wake.ignore_speakers, vec!["10001".to_string()]);
}
