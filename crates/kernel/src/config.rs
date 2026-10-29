//! 配置：设备、订阅、三条策略。
//!
//! 订阅 = 一个房间 + 发送账号 + 唤醒策略（ADR-0002）。接收策略与时钟策略还没轮到，
//! 先做目前唯一有判定逻辑的唤醒策略，避免一次铺开太多。
//!
//! 配置从 TOML 读进来后**完全校验**：非法配置在这里就死，不会带病启动。

use qqab_proto::RoomId;
use serde::Deserialize;
use thiserror::Error;

/// 没配「攒多少条」时的默认值。来源：实现默认，可推翻。
pub const DEFAULT_ACCUMULATE: u32 = 10;

/// 没配存储路径时的默认值：工作目录下的 `state/qqab.db`。
/// 来源：实现默认。放 `state/` 是因为那里已经在 .gitignore 里，不会被误提交。
pub const DEFAULT_STORAGE_PATH: &str = "state/qqab.db";

/// 没配「每个房间留多少条」时的默认值。来源：实现默认。
/// 5000 条对一个群来说大概是几个月的体量，够 `get_history` 往前翻。
pub const DEFAULT_MAX_RECORDS_PER_ROOM: u64 = 5000;

/// 没配「留多少天」时的默认值。来源：实现默认。
pub const DEFAULT_MAX_AGE_DAYS: u64 = 90;

/// 整份配置。
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub devices: Vec<Device>,
    /// 接入点配置。没配就不监听——只跑内存里的桥（测试、嵌入式）。
    pub endpoint: Option<EndpointConfig>,
    /// 平台侧适配器。一个平台一个，桥按房间的平台名把出站交给对应的适配器。
    pub adapters: Vec<AdapterConfig>,
    /// 状态存哪。流水、水位、笔记、投递计数都在这里，重启不改。
    pub storage: StorageConfig,
    /// 流水保留：旧记录什么时候可以删。
    pub retention: RetentionConfig,
}

/// 流水保留策略。
///
/// 只管**已经读过**的那一段：任何在册订阅者还没读到的记录，一条都不删（见
/// `Bridge::prune`）。两项都是 0 表示不作任何清理。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionConfig {
    /// 每个房间最多留多少条。0 = 不按条数清。
    pub max_records_per_room: u64,
    /// 记录最多留多少天。0 = 不按天数清。
    pub max_age_days: u64,
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            max_records_per_room: DEFAULT_MAX_RECORDS_PER_ROOM,
            max_age_days: DEFAULT_MAX_AGE_DAYS,
        }
    }
}

impl RetentionConfig {
    /// 两项都为 0 就是完全不清。
    pub fn enabled(self) -> bool {
        self.max_records_per_room > 0 || self.max_age_days > 0
    }
}

/// 状态存储：一个 SQLite 文件。
#[derive(Debug, Clone, PartialEq)]
pub struct StorageConfig {
    /// 库文件路径。相对路径按启动时的工作目录算。
    pub path: String,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            path: DEFAULT_STORAGE_PATH.to_owned(),
        }
    }
}

/// 一个平台侧适配器：往哪个平台、用哪种协议、连到哪个地址。
#[derive(Debug, Clone, PartialEq)]
pub struct AdapterConfig {
    /// 平台名，要跟订阅里的房间前缀一致（`qq` 的房间写成 `qq:...`）。
    pub platform: String,
    /// 协议种类，目前只有 `onebot11`。
    pub kind: String,
    /// 连接地址，形如 `ws://127.0.0.1:3001`。
    pub url: String,
    /// 访问令牌。OneBot 实现开了 token 就要填，握手时放在 `Authorization` 头里。
    pub token: Option<String>,
}

/// 目前认识的适配器种类。加新适配器时在这里登记。
pub const KNOWN_ADAPTERS: &[&str] = &["onebot11"];

/// 接入点：harness 连进来的地址。
#[derive(Debug, Clone, PartialEq)]
pub struct EndpointConfig {
    /// 监听地址，形如 `127.0.0.1:7700`。
    pub listen: String,
}

/// 一个设备：虚拟订阅者。
#[derive(Debug, Clone, PartialEq)]
pub struct Device {
    pub id: String,
    pub persona: String,
    pub memory: String,
    /// 这个设备的接入令牌。harness 拿它握手，握手成功后这条连接就绑到这个设备。
    pub token: String,
    pub subscriptions: Vec<Subscription>,
}

/// 一条订阅。
#[derive(Debug, Clone, PartialEq)]
pub struct Subscription {
    pub room: RoomId,
    pub send_account: String,
    pub wake: WakePolicy,
}

/// 唤醒策略的判定输入。只带判定需要的东西，不把整条记录塞进来。
#[derive(Debug, Clone, Copy)]
pub struct WakeInput<'a> {
    /// 发送者 id。
    pub sender_id: &'a str,
    /// 消息纯文本（事件没有文本，传空串）。
    pub text: &'a str,
    /// 是不是冲本账号来的（@ 到我、拍我）。
    pub at_self: bool,
    /// 这条是不是引用了我发的某条消息。
    pub reply_to_self: bool,
    /// 平台事件类型；普通消息为 `None`。
    pub event: Option<&'a str>,
    /// 这条进来之后，该房间待处理的未读条数。
    pub unread: u32,
}

/// 判定结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WakeDecision {
    Wake,
    Stay,
}

/// 唤醒策略。
///
/// 默认五件套：@ 我就醒、引用我的话也醒、叫昵称就醒、攒够 N 条就醒、被拍就醒
/// （ADR-0002 / 0007）。关键词、事件类型、免打扰名单是可选开关，默认全关。
#[derive(Debug, Clone, PartialEq)]
pub struct WakePolicy {
    /// @ 到本账号就醒。
    pub at_mention: bool,
    /// 引用我发的消息就醒（QQ 的「回复」就是这种）。
    ///
    /// 和 `at_mention` 分开：有人只想被 @ 叫醒，不想被人引用一条旧消息就醒。
    pub reply: bool,
    /// 文本里出现 `names` 里任意一个就醒。
    pub name_mention: bool,
    pub names: Vec<String>,
    /// 未读攒到这么多条就醒；`None` 表示不看条数。
    pub accumulate: Option<u32>,
    /// 被拍就醒（事件 `poke` 且冲着自己）。
    pub poke: bool,
    /// 额外的事件类型，命中就醒。
    pub events: Vec<String>,
    /// 文本命中关键词就醒。默认空。
    pub keywords: Vec<String>,
    /// 这些人说话不叫醒（只挡唤醒，不挡记录）。
    pub ignore_speakers: Vec<String>,
}

impl Default for WakePolicy {
    fn default() -> Self {
        Self {
            at_mention: true,
            reply: true,
            name_mention: true,
            names: Vec::new(),
            accumulate: Some(DEFAULT_ACCUMULATE),
            poke: true,
            events: Vec::new(),
            keywords: Vec::new(),
            ignore_speakers: Vec::new(),
        }
    }
}

impl WakePolicy {
    /// 判定这条要不要叫醒设备。
    ///
    /// 顺序有讲究：免打扰名单最先——被忽略的人怎么叫都不醒，但记录照旧。
    pub fn decide(&self, input: &WakeInput<'_>) -> WakeDecision {
        if self
            .ignore_speakers
            .iter()
            .any(|speaker| speaker == input.sender_id)
        {
            return WakeDecision::Stay;
        }

        if let Some(event) = input.event {
            if event == "poke" && self.poke && input.at_self {
                return WakeDecision::Wake;
            }
            if self.events.iter().any(|wanted| wanted == event) {
                return WakeDecision::Wake;
            }
            return WakeDecision::Stay;
        }

        if self.at_mention && input.at_self {
            return WakeDecision::Wake;
        }
        // 引用我的话等同于对我说：QQ 里「回复」是最自然的追问方式，很多人不会再加 @。
        if self.reply && input.reply_to_self {
            return WakeDecision::Wake;
        }
        if self.name_mention && matches_any(&self.names, input.text) {
            return WakeDecision::Wake;
        }
        if matches_any(&self.keywords, input.text) {
            return WakeDecision::Wake;
        }
        if let Some(threshold) = self.accumulate {
            if threshold > 0 && input.unread >= threshold {
                return WakeDecision::Wake;
            }
        }
        WakeDecision::Stay
    }
}

fn matches_any(names: &[String], text: &str) -> bool {
    names
        .iter()
        .any(|name| !name.trim().is_empty() && text.contains(name.as_str()))
}

/// 配置错误。
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ConfigError {
    #[error("配置读不懂：{detail}")]
    ParseFailed { detail: String },
    #[error("设备 {device} 的 {field} 不能为空")]
    EmptyField { device: String, field: &'static str },
    #[error("设备 id 重复：{device}")]
    DuplicateDevice { device: String },
    #[error("设备 {device} 重复订阅了房间 {room}")]
    DuplicateRoom { device: String, room: String },
    #[error("设备 {device} 的房间标识不合法：{room}（{detail}）")]
    BadRoom {
        device: String,
        room: String,
        detail: String,
    },
    #[error("设备 {device} 的房间 {room} 没有配置发送账号")]
    EmptyAccount { device: String, room: String },
    #[error("设备 {device} 的房间 {room} 的 accumulate 必须是正整数")]
    BadAccumulate { device: String, room: String },
    #[error("设备 {device} 的接入令牌不能为空")]
    EmptyToken { device: String },
    #[error("接入令牌重复：设备 {first} 和 {second} 用了同一个令牌")]
    DuplicateToken { first: String, second: String },
    #[error("接入点监听地址不合法：{address}（{detail}）")]
    BadListen { address: String, detail: String },
    #[error("适配器的平台名不能为空")]
    EmptyAdapterPlatform,
    #[error("平台 {platform} 配了不止一个适配器")]
    DuplicateAdapter { platform: String },
    #[error("适配器 {platform} 的种类不认识：{kind}（目前只有 onebot11）")]
    UnknownAdapterKind { platform: String, kind: String },
    #[error("适配器 {platform} 的连接地址不合法：{url}（目前只支持 ws://主机:端口/路径）")]
    BadAdapterUrl { platform: String, url: String },
    #[error("存储路径不能为空（不配就用默认的 {DEFAULT_STORAGE_PATH}）")]
    EmptyStoragePath,
}

impl Config {
    /// 从 TOML 文本解析并校验。
    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        let raw: RawConfig = toml::from_str(text).map_err(|error| ConfigError::ParseFailed {
            detail: error.to_string(),
        })?;
        Self::from_raw(raw)
    }

    fn from_raw(raw: RawConfig) -> Result<Self, ConfigError> {
        let storage = match raw.storage {
            Some(raw) => {
                let path = raw.path.trim().to_owned();
                if path.is_empty() {
                    return Err(ConfigError::EmptyStoragePath);
                }
                StorageConfig { path }
            }
            None => StorageConfig::default(),
        };

        let retention = match raw.retention {
            Some(raw) => RetentionConfig {
                max_records_per_room: raw
                    .max_records_per_room
                    .unwrap_or(DEFAULT_MAX_RECORDS_PER_ROOM),
                max_age_days: raw.max_age_days.unwrap_or(DEFAULT_MAX_AGE_DAYS),
            },
            None => RetentionConfig::default(),
        };

        let endpoint = match raw.endpoint {
            Some(raw) => {
                let address = raw.listen.trim().to_owned();
                // 解析不了就别启动：监听地址写错了，等到运行时才发现只会更晚。
                if address.parse::<std::net::SocketAddr>().is_err() {
                    return Err(ConfigError::BadListen {
                        address,
                        detail: "要写成 主机:端口，如 127.0.0.1:7700".into(),
                    });
                }
                Some(EndpointConfig { listen: address })
            }
            None => None,
        };

        let mut adapters = Vec::with_capacity(raw.adapters.len());
        for raw_adapter in raw.adapters {
            let platform = raw_adapter.platform.trim().to_owned();
            if platform.is_empty() {
                return Err(ConfigError::EmptyAdapterPlatform);
            }
            if adapters
                .iter()
                .any(|seen: &AdapterConfig| seen.platform == platform)
            {
                return Err(ConfigError::DuplicateAdapter { platform });
            }

            let kind = raw_adapter.kind.trim().to_owned();
            if !KNOWN_ADAPTERS.contains(&kind.as_str()) {
                return Err(ConfigError::UnknownAdapterKind { platform, kind });
            }

            let url = raw_adapter.url.trim().to_owned();
            // 只支持不加密的 ws://：wss 要拉 TLS 依赖，等真有需求再说。
            let looks_like_url = url.starts_with("ws://") && url.len() > "ws://".len();
            if !looks_like_url {
                return Err(ConfigError::BadAdapterUrl { platform, url });
            }

            adapters.push(AdapterConfig {
                platform,
                kind,
                url,
                token: raw_adapter
                    .token
                    .map(|token| token.trim().to_owned())
                    .filter(|token| !token.is_empty()),
            });
        }

        let mut devices = Vec::with_capacity(raw.devices.len());
        let mut seen_devices = Vec::new();
        let mut seen_tokens: Vec<(String, String)> = Vec::new();

        for raw_device in raw.devices {
            let id = raw_device.id;
            if seen_devices.iter().any(|seen: &String| seen == &id) {
                return Err(ConfigError::DuplicateDevice { device: id });
            }
            seen_devices.push(id.clone());

            check_not_blank(&id, "id").map_err(|field| ConfigError::EmptyField {
                device: id.clone(),
                field,
            })?;
            check_not_blank(&raw_device.persona, "persona").map_err(|field| {
                ConfigError::EmptyField {
                    device: id.clone(),
                    field,
                }
            })?;
            check_not_blank(&raw_device.memory, "memory").map_err(|field| {
                ConfigError::EmptyField {
                    device: id.clone(),
                    field,
                }
            })?;
            check_not_blank(&raw_device.token, "token")
                .map_err(|_| ConfigError::EmptyToken { device: id.clone() })?;

            let token = raw_device.token.trim().to_owned();
            if let Some((first, _)) = seen_tokens.iter().find(|(_, seen)| seen == &token) {
                return Err(ConfigError::DuplicateToken {
                    first: first.clone(),
                    second: id,
                });
            }
            seen_tokens.push((id.clone(), token.clone()));

            let mut subscriptions = Vec::with_capacity(raw_device.subscriptions.len());
            let mut seen_rooms: Vec<RoomId> = Vec::new();
            for raw_subscription in raw_device.subscriptions {
                let subscription = build_subscription(&id, raw_subscription)?;
                if seen_rooms.iter().any(|room| room == &subscription.room) {
                    return Err(ConfigError::DuplicateRoom {
                        device: id.clone(),
                        room: subscription.room.to_string(),
                    });
                }
                seen_rooms.push(subscription.room.clone());
                subscriptions.push(subscription);
            }

            devices.push(Device {
                id,
                persona: raw_device.persona,
                memory: raw_device.memory,
                token,
                subscriptions,
            });
        }

        Ok(Self {
            devices,
            endpoint,
            adapters,
            storage,
            retention,
        })
    }

    /// 找某个平台的适配器配置。
    pub fn adapter(&self, platform: &str) -> Option<&AdapterConfig> {
        self.adapters
            .iter()
            .find(|adapter| adapter.platform == platform)
    }

    /// 找设备的某条订阅。发送与工具鉴权都从这里查。
    pub fn subscription(&self, device: &str, room: &RoomId) -> Option<&Subscription> {
        self.devices
            .iter()
            .find(|candidate| candidate.id == device)?
            .subscriptions
            .iter()
            .find(|subscription| &subscription.room == room)
    }

    /// 按接入令牌找设备。握手时用：令牌对了，这条连接就绑到那个设备。
    pub fn device_by_token(&self, token: &str) -> Option<&Device> {
        self.devices
            .iter()
            .find(|candidate| candidate.token == token)
    }
}

fn build_subscription(device: &str, raw: RawSubscription) -> Result<Subscription, ConfigError> {
    let room = RoomId::parse(&raw.room).map_err(|error| ConfigError::BadRoom {
        device: device.to_owned(),
        room: raw.room.clone(),
        detail: error.to_string(),
    })?;

    check_not_blank(&raw.send_account, "send_account").map_err(|_| ConfigError::EmptyAccount {
        device: device.to_owned(),
        room: room.to_string(),
    })?;

    let wake = build_wake(device, &room, raw.wake.unwrap_or_default())?;

    Ok(Subscription {
        room,
        send_account: raw.send_account,
        wake,
    })
}

fn build_wake(device: &str, room: &RoomId, raw: RawWake) -> Result<WakePolicy, ConfigError> {
    let default = WakePolicy::default();

    if let Some(accumulate) = raw.accumulate {
        if accumulate == 0 {
            return Err(ConfigError::BadAccumulate {
                device: device.to_owned(),
                room: room.to_string(),
            });
        }
    }

    Ok(WakePolicy {
        at_mention: raw.at_mention.unwrap_or(default.at_mention),
        reply: raw.reply.unwrap_or(default.reply),
        name_mention: raw.name_mention.unwrap_or(default.name_mention),
        names: raw.names.unwrap_or(default.names),
        accumulate: raw.accumulate.map(Some).unwrap_or(default.accumulate),
        poke: raw.poke.unwrap_or(default.poke),
        events: raw.events.unwrap_or(default.events),
        keywords: raw.keywords.unwrap_or(default.keywords),
        ignore_speakers: raw.ignore_speakers.unwrap_or(default.ignore_speakers),
    })
}

fn check_not_blank(value: &str, field: &'static str) -> Result<(), &'static str> {
    if value.trim().is_empty() {
        return Err(field);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 原始 TOML 形态
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct RawConfig {
    #[serde(default)]
    devices: Vec<RawDevice>,
    endpoint: Option<RawEndpoint>,
    #[serde(default)]
    adapters: Vec<RawAdapter>,
    storage: Option<RawStorage>,
    retention: Option<RawRetention>,
}

#[derive(Debug, Deserialize)]
struct RawStorage {
    path: String,
}

#[derive(Debug, Default, Deserialize)]
struct RawRetention {
    max_records_per_room: Option<u64>,
    max_age_days: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct RawAdapter {
    platform: String,
    kind: String,
    url: String,
    token: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawEndpoint {
    listen: String,
}

#[derive(Debug, Deserialize)]
struct RawDevice {
    id: String,
    persona: String,
    memory: String,
    /// 接入令牌。必填——没有令牌就没有身份，桥不知道该把这条连接绑到哪个设备。
    token: String,
    #[serde(default)]
    subscriptions: Vec<RawSubscription>,
}

#[derive(Debug, Deserialize)]
struct RawSubscription {
    room: String,
    send_account: String,
    wake: Option<RawWake>,
}

#[derive(Debug, Default, Deserialize)]
struct RawWake {
    at_mention: Option<bool>,
    reply: Option<bool>,
    name_mention: Option<bool>,
    names: Option<Vec<String>>,
    accumulate: Option<u32>,
    poke: Option<bool>,
    events: Option<Vec<String>>,
    keywords: Option<Vec<String>>,
    ignore_speakers: Option<Vec<String>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"
        [[devices]]
        id = "main"
        persona = "default"
        memory = "main-memory"
        token = "t-main"

        [[devices.subscriptions]]
        room = "qq:123"
        send_account = "qq-bot"
    "#;

    #[test]
    fn minimal_config_uses_documented_defaults() {
        let config = Config::parse(MINIMAL).expect("应当能解析");
        let subscription = &config.devices[0].subscriptions[0];

        assert_eq!(subscription.room.to_string(), "qq:123");
        assert!(subscription.wake.at_mention);
        assert!(subscription.wake.name_mention);
        assert!(subscription.wake.poke);
        assert_eq!(subscription.wake.accumulate, Some(DEFAULT_ACCUMULATE));
        assert!(subscription.wake.keywords.is_empty());
        assert!(subscription.wake.events.is_empty());
    }

    #[test]
    fn explicit_wake_policy_overrides_defaults() {
        let text = r#"
            [[devices]]
            id = "main"
            persona = "p"
            memory = "m"
            token = "t-main"

            [[devices.subscriptions]]
            room = "discord:456"
            send_account = "dc-bot"

            [devices.subscriptions.wake]
            at_mention = false
            poke = false
            names = ["小助手"]
            accumulate = 3
            keywords = ["上线"]
            ignore_speakers = ["10001"]
        "#;
        let config = Config::parse(text).expect("应当能解析");
        let wake = &config.devices[0].subscriptions[0].wake;

        assert!(!wake.at_mention);
        assert!(!wake.poke);
        assert_eq!(wake.names, vec!["小助手"]);
        assert_eq!(wake.accumulate, Some(3));
        assert_eq!(wake.keywords, vec!["上线"]);
        assert_eq!(wake.ignore_speakers, vec!["10001"]);
    }

    #[test]
    fn same_room_number_on_two_platforms_is_fine() {
        let text = r#"
            [[devices]]
            id = "main"
            persona = "p"
            memory = "m"
            token = "t-main"

            [[devices.subscriptions]]
            room = "qq:123"
            send_account = "a"

            [[devices.subscriptions]]
            room = "discord:123"
            send_account = "b"
        "#;
        let config = Config::parse(text).expect("应当能解析");
        assert_eq!(config.devices[0].subscriptions.len(), 2);
    }

    #[test]
    fn duplicate_room_in_one_device_is_rejected() {
        let text = r#"
            [[devices]]
            id = "main"
            persona = "p"
            memory = "m"
            token = "t-main"

            [[devices.subscriptions]]
            room = "qq:123"
            send_account = "a"

            [[devices.subscriptions]]
            room = "qq:123"
            send_account = "b"
        "#;
        assert!(matches!(
            Config::parse(text),
            Err(ConfigError::DuplicateRoom { .. })
        ));
    }

    #[test]
    fn duplicate_device_is_rejected() {
        let text = r#"
            [[devices]]
            id = "main"
            persona = "p"
            memory = "m"
            token = "t-main"

            [[devices]]
            id = "main"
            persona = "p2"
            memory = "m2"
            token = "t-other"
        "#;
        assert!(matches!(
            Config::parse(text),
            Err(ConfigError::DuplicateDevice { .. })
        ));
    }

    #[test]
    fn bad_room_string_is_rejected() {
        let text = r#"
            [[devices]]
            id = "main"
            persona = "p"
            memory = "m"
            token = "t-main"

            [[devices.subscriptions]]
            room = "没有平台前缀"
            send_account = "a"
        "#;
        assert!(matches!(
            Config::parse(text),
            Err(ConfigError::BadRoom { .. })
        ));
    }

    #[test]
    fn blank_values_are_rejected() {
        let text = r#"
            [[devices]]
            id = "main"
            persona = "  "
            memory = "m"
            token = "t-main"
        "#;
        assert!(matches!(
            Config::parse(text),
            Err(ConfigError::EmptyField {
                field: "persona",
                ..
            })
        ));

        let text = r#"
            [[devices]]
            id = "main"
            persona = "p"
            memory = "m"
            token = "t-main"

            [[devices.subscriptions]]
            room = "qq:1"
            send_account = " "
        "#;
        assert!(matches!(
            Config::parse(text),
            Err(ConfigError::EmptyAccount { .. })
        ));
    }

    #[test]
    fn zero_accumulate_is_rejected() {
        let text = r#"
            [[devices]]
            id = "main"
            persona = "p"
            memory = "m"
            token = "t-main"

            [[devices.subscriptions]]
            room = "qq:1"
            send_account = "a"

            [devices.subscriptions.wake]
            accumulate = 0
        "#;
        assert!(matches!(
            Config::parse(text),
            Err(ConfigError::BadAccumulate { .. })
        ));
    }

    #[test]
    fn broken_toml_reports_parse_failure() {
        assert!(matches!(
            Config::parse("这不是 toml"),
            Err(ConfigError::ParseFailed { .. })
        ));
    }

    // ---- 接入令牌与接入点 ----

    #[test]
    fn a_device_without_token_is_rejected() {
        let text = r#"
            [[devices]]
            id = "main"
            persona = "p"
            memory = "m"
        "#;
        assert!(matches!(
            Config::parse(text),
            Err(ConfigError::ParseFailed { .. })
        ));
    }

    #[test]
    fn blank_token_is_rejected() {
        let text = r#"
            [[devices]]
            id = "main"
            persona = "p"
            memory = "m"
            token = "   "
        "#;
        assert!(matches!(
            Config::parse(text),
            Err(ConfigError::EmptyToken { .. })
        ));
    }

    #[test]
    fn two_devices_may_not_share_a_token() {
        let text = r#"
            [[devices]]
            id = "main"
            persona = "p"
            memory = "m"
            token = "same"

            [[devices]]
            id = "side"
            persona = "p2"
            memory = "m2"
            token = "same"
        "#;
        assert!(matches!(
            Config::parse(text),
            Err(ConfigError::DuplicateToken { .. })
        ));
    }

    #[test]
    fn token_maps_to_its_device() {
        let config = Config::parse(MINIMAL).expect("应当能解析");
        assert_eq!(
            config
                .device_by_token("t-main")
                .map(|device| device.id.as_str()),
            Some("main")
        );
        assert!(config.device_by_token("nope").is_none());
    }

    #[test]
    fn endpoint_section_is_optional() {
        let config = Config::parse(MINIMAL).expect("应当能解析");
        assert!(config.endpoint.is_none(), "不配就不监听");
    }

    #[test]
    fn endpoint_listen_must_be_a_socket_address() {
        let text = format!("{MINIMAL}\n[endpoint]\nlisten = \"127.0.0.1:7700\"\n");
        let config = Config::parse(&text).expect("应当能解析");
        assert_eq!(
            config
                .endpoint
                .as_ref()
                .map(|endpoint| endpoint.listen.as_str()),
            Some("127.0.0.1:7700")
        );

        let text = format!("{MINIMAL}\n[endpoint]\nlisten = \"随便写的\"\n");
        assert!(matches!(
            Config::parse(&text),
            Err(ConfigError::BadListen { .. })
        ));
    }

    // ---- 适配器 ----

    #[test]
    fn adapters_section_is_optional() {
        let config = Config::parse(MINIMAL).expect("应当能解析");
        assert!(config.adapters.is_empty(), "不配适配器就没有平台连接");
    }

    #[test]
    fn adapter_is_found_by_platform() {
        let text = format!(
            "{MINIMAL}\n[[adapters]]\nplatform = \"qq\"\nkind = \"onebot11\"\nurl = \"ws://127.0.0.1:3001\"\n"
        );
        let config = Config::parse(&text).expect("应当能解析");
        assert_eq!(
            config.adapter("qq").map(|adapter| adapter.url.as_str()),
            Some("ws://127.0.0.1:3001")
        );
        assert!(config.adapter("discord").is_none());
    }

    #[test]
    fn adapter_token_is_optional_and_trimmed() {
        let text = format!(
            "{MINIMAL}\n[[adapters]]\nplatform = \"qq\"\nkind = \"onebot11\"\nurl = \"ws://127.0.0.1:3001\"\ntoken = \"  abc  \"\n"
        );
        let config = Config::parse(&text).expect("应当能解析");
        assert_eq!(
            config
                .adapter("qq")
                .and_then(|adapter| adapter.token.as_deref()),
            Some("abc")
        );

        let text = format!(
            "{MINIMAL}\n[[adapters]]\nplatform = \"qq\"\nkind = \"onebot11\"\nurl = \"ws://127.0.0.1:3001\"\n"
        );
        let config = Config::parse(&text).expect("应当能解析");
        assert!(config.adapter("qq").expect("有适配器").token.is_none());
    }

    #[test]
    fn unknown_adapter_kind_is_rejected() {
        let text = format!(
            "{MINIMAL}\n[[adapters]]\nplatform = \"qq\"\nkind = \"telepathy\"\nurl = \"ws://127.0.0.1:3001\"\n"
        );
        assert!(matches!(
            Config::parse(&text),
            Err(ConfigError::UnknownAdapterKind { .. })
        ));
    }

    #[test]
    fn adapter_url_must_be_ws() {
        let text = format!(
            "{MINIMAL}\n[[adapters]]\nplatform = \"qq\"\nkind = \"onebot11\"\nurl = \"http://127.0.0.1:3001\"\n"
        );
        assert!(matches!(
            Config::parse(&text),
            Err(ConfigError::BadAdapterUrl { .. })
        ));
    }

    #[test]
    fn one_platform_may_not_have_two_adapters() {
        let text = format!(
            "{MINIMAL}\n\
             [[adapters]]\nplatform = \"qq\"\nkind = \"onebot11\"\nurl = \"ws://127.0.0.1:3001\"\n\
             [[adapters]]\nplatform = \"qq\"\nkind = \"onebot11\"\nurl = \"ws://127.0.0.1:3002\"\n"
        );
        assert!(matches!(
            Config::parse(&text),
            Err(ConfigError::DuplicateAdapter { .. })
        ));
    }

    // ---- 存储 ----

    #[test]
    fn storage_defaults_to_state_dir_when_absent() {
        let config = Config::parse(MINIMAL).expect("应当能解析");
        assert_eq!(config.storage.path, DEFAULT_STORAGE_PATH);
        assert_eq!(DEFAULT_STORAGE_PATH, "state/qqab.db");
    }

    #[test]
    fn storage_path_can_be_set() {
        let text = format!("{MINIMAL}\n[storage]\npath = \"/var/lib/qqab/bridge.db\"\n");
        let config = Config::parse(&text).expect("应当能解析");
        assert_eq!(config.storage.path, "/var/lib/qqab/bridge.db");
    }

    #[test]
    fn a_blank_storage_path_is_rejected() {
        let text = format!("{MINIMAL}\n[storage]\npath = \"  \"\n");
        assert!(matches!(
            Config::parse(&text),
            Err(ConfigError::EmptyStoragePath)
        ));
    }

    #[test]
    fn retention_falls_back_to_the_documented_defaults() {
        let config = Config::parse(MINIMAL).expect("应当能解析");
        assert_eq!(config.retention.max_records_per_room, 5000);
        assert_eq!(config.retention.max_age_days, 90);
        assert!(config.retention.enabled());
    }

    #[test]
    fn retention_can_turn_each_limit_off() {
        let text = format!("{MINIMAL}\n[retention]\nmax_records_per_room = 0\nmax_age_days = 0\n");
        let config = Config::parse(&text).expect("应当能解析");
        assert!(!config.retention.enabled(), "两项都是 0 就是不清理");

        let text = format!("{MINIMAL}\n[retention]\nmax_records_per_room = 20\n");
        let config = Config::parse(&text).expect("应当能解析");
        assert_eq!(config.retention.max_records_per_room, 20);
        assert_eq!(
            config.retention.max_age_days, 90,
            "只写一项时，另一项走默认"
        );
    }

    // ---- 唤醒判定 ----

    fn input<'a>(text: &'a str, at_self: bool, unread: u32) -> WakeInput<'a> {
        WakeInput {
            sender_id: "10001",
            text,
            at_self,
            reply_to_self: false,
            event: None,
            unread,
        }
    }

    /// 引用了我某条消息的输入。
    fn reply<'a>(text: &'a str, unread: u32) -> WakeInput<'a> {
        WakeInput {
            reply_to_self: true,
            ..input(text, false, unread)
        }
    }

    #[test]
    fn quoting_my_message_wakes_by_default() {
        let policy = WakePolicy::default();
        assert_eq!(
            policy.decide(&reply("只发子网", 1)),
            WakeDecision::Wake,
            "引用我的话就是对我说话，没 @ 也该醒"
        );
    }

    #[test]
    fn reply_switch_can_be_turned_off() {
        let policy = WakePolicy {
            reply: false,
            ..WakePolicy::default()
        };
        assert_eq!(
            policy.decide(&reply("只发子网", 1)),
            WakeDecision::Stay,
            "关掉之后只认 @，引用不叫醒"
        );
    }

    #[test]
    fn default_policy_wakes_on_mention_and_stays_on_chatter() {
        let policy = WakePolicy::default();
        assert_eq!(policy.decide(&input("在吗", true, 1)), WakeDecision::Wake);
        assert_eq!(
            policy.decide(&input("今天天气不错", false, 1)),
            WakeDecision::Stay
        );
    }

    #[test]
    fn name_mention_wakes_only_when_names_configured() {
        let mut policy = WakePolicy::default();
        assert_eq!(
            policy.decide(&input("小助手在吗", false, 1)),
            WakeDecision::Stay
        );

        policy.names = vec!["小助手".into()];
        assert_eq!(
            policy.decide(&input("小助手在吗", false, 1)),
            WakeDecision::Wake
        );
    }

    #[test]
    fn accumulating_enough_messages_wakes() {
        let policy = WakePolicy::default();
        let threshold = DEFAULT_ACCUMULATE;
        assert_eq!(
            policy.decide(&input("嗯", false, threshold - 1)),
            WakeDecision::Stay
        );
        assert_eq!(
            policy.decide(&input("嗯", false, threshold)),
            WakeDecision::Wake
        );
    }

    #[test]
    fn poke_wakes_by_default_only_when_it_targets_me() {
        let policy = WakePolicy::default();
        let poke_at_me = WakeInput {
            sender_id: "10001",
            text: "",
            at_self: true,
            reply_to_self: false,
            event: Some("poke"),
            unread: 0,
        };
        let poke_someone_else = WakeInput {
            at_self: false,
            ..poke_at_me
        };

        assert_eq!(policy.decide(&poke_at_me), WakeDecision::Wake);
        assert_eq!(policy.decide(&poke_someone_else), WakeDecision::Stay);
    }

    #[test]
    fn other_events_stay_unless_configured() {
        let event = WakeInput {
            sender_id: "10001",
            text: "",
            at_self: true,
            reply_to_self: false,
            event: Some("kook.channel.created"),
            unread: 5,
        };
        let mut policy = WakePolicy::default();
        assert_eq!(
            policy.decide(&event),
            WakeDecision::Stay,
            "默认不因其它事件醒"
        );

        policy.events = vec!["kook.channel.created".into()];
        assert_eq!(policy.decide(&event), WakeDecision::Wake);
    }

    #[test]
    fn ignored_speaker_never_wakes_even_when_mentioned() {
        let policy = WakePolicy {
            ignore_speakers: vec!["10001".into()],
            ..WakePolicy::default()
        };
        assert_eq!(policy.decide(&input("在吗", true, 99)), WakeDecision::Stay);
    }

    #[test]
    fn keywords_can_wake() {
        let policy = WakePolicy {
            keywords: vec!["上线".into()],
            ..WakePolicy::default()
        };
        assert_eq!(
            policy.decide(&input("今晚上线吗", false, 1)),
            WakeDecision::Wake
        );
    }
}
