//! 示例配置必须始终能被解析——它是用户复制粘贴的起点，过期了就是 bug。

use std::path::PathBuf;

use acb_kernel::Config;

fn example_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config.example.toml")
}

fn example() -> Config {
    let text = std::fs::read_to_string(example_path()).expect("示例配置应当存在");
    Config::parse(&text).expect("示例配置应当能解析")
}

#[test]
fn example_config_shows_two_connections_on_one_platform() {
    let config = example();

    assert_eq!(config.connections.len(), 2, "示例要示范同平台两条连接");
    assert_eq!(config.connections[0].platform, "qq");
    assert_eq!(config.connections[1].platform, "qq");
    assert_ne!(
        config.connections[0].account, config.connections[1].account,
        "一个账号一条连接"
    );

    assert_eq!(config.personas.len(), 1);
    assert_eq!(config.memories.len(), 1);
    assert_eq!(config.sessions.len(), 1);
    assert_eq!(config.sessions[0].subscriptions.len(), 2);
}

#[test]
fn example_config_demonstrates_per_subscription_wake_rules() {
    let config = example();
    let subscriptions = &config.sessions[0].subscriptions;

    let group = subscriptions
        .iter()
        .find(|subscription| subscription.room.key().starts_with("group/"))
        .expect("应当有群订阅");
    let private = subscriptions
        .iter()
        .find(|subscription| subscription.room.key().starts_with("private/"))
        .expect("应当有私聊订阅");

    assert!(group.wake.ignore_event_tags.is_empty(), "群里被拍要醒");
    assert_eq!(
        private.wake.ignore_event_tags,
        vec!["poke".to_string()],
        "私聊那边 poke = false，不因被拍而醒"
    );
    assert_ne!(
        group.wake.accumulate, private.wake.accumulate,
        "两个房间的攒条阈值可以不同"
    );
    assert_eq!(private.wake.ignore_speakers, vec!["987654321".to_string()]);
}

#[test]
fn example_config_connects_qq_through_onebot_websocket() {
    let config = example();

    for connection in &config.connections {
        assert_eq!(connection.kind, "onebot11");
        assert_eq!(connection.transport.as_str(), "ws-client", "正向 WebSocket");
        let url = connection.url.as_deref().expect("OneBot 连接必须有地址");
        assert!(url.starts_with("ws://"), "OneBot 11 走 WebSocket：{url}");
    }

    // QQ 房间要按适配器的写法来，否则适配器认不出是群还是私聊。
    for subscription in &config.sessions[0].subscriptions {
        assert!(
            config.connection(&subscription.room.connection).is_some(),
            "订阅指向的连接必须在配置里存在：{}",
            subscription.room
        );
        let key = subscription.room.key();
        assert!(
            key.starts_with("group/") || key.starts_with("private/"),
            "QQ 房间要写成 group/<群号> 或 private/<号码>，示例里是 {}",
            key
        );
    }
}
