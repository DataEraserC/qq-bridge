//! 配置：解析 + 校验。
//!
//! 任何一条不通过即启动失败，并指出具体位置与原因。不做「警告后继续」——配置错误
//! 在运行期表现为「某些群没反应」，比启动失败难查得多。

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::Deserialize;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// 已校验的配置模型。
#[derive(Debug, Clone)]
pub struct Config {
    pub accounts: Vec<Account>,
    pub personas: Vec<Persona>,
    pub devices: Vec<Device>,
    pub adapter: AdapterConfig,
    pub notes: NotesConfig,
}

impl Config {
    /// 从 TOML 文本解析并校验。
    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        let raw: RawConfig = toml::from_str(text).map_err(|source| ConfigError::Syntax {
            source: Box::new(source),
        })?;
        Self::from_raw(raw)
    }

    fn from_raw(raw: RawConfig) -> Result<Self, ConfigError> {
        let accounts = validate_accounts(raw.accounts)?;
        let personas = validate_personas(raw.personas)?;
        let devices = validate_devices(raw.devices, &accounts, &personas)?;

        Ok(Self {
            accounts,
            personas,
            devices,
            adapter: raw.adapter.unwrap_or_default().validated()?,
            notes: raw.notes.unwrap_or_default().validated()?,
        })
    }
}

// ---------------------------------------------------------------------------
// 账号
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Account {
    /// 本文件内唯一的标识，不是 QQ 号。
    pub id: String,
    /// 这个账号的 QQ 号。
    pub self_id: String,
    pub ws_url: String,
    pub token: Option<Secret>,
}

fn validate_accounts(raw: Vec<RawAccount>) -> Result<Vec<Account>, ConfigError> {
    if raw.is_empty() {
        return Err(ConfigError::NoAccounts);
    }

    let mut seen_ids = BTreeSet::new();
    let mut seen_self = BTreeMap::new();
    let mut accounts = Vec::with_capacity(raw.len());

    for (index, account) in raw.into_iter().enumerate() {
        let id = require_non_empty(account.id, "accounts[].id", index)?;
        let self_id = require_non_empty(account.self_id, "accounts[].self_id", index)?;
        let ws_url = require_non_empty(account.ws_url, "accounts[].ws_url", index)?;

        if !seen_ids.insert(id.clone()) {
            return Err(ConfigError::DuplicateAccountId { id });
        }
        // 同一个 QQ 号挂两条连接没有意义：身份过滤会重复计一次，发送侧也会出现
        // 两个「可用账号」其实是同一个人。
        if let Some(previous) = seen_self.insert(self_id.clone(), id.clone()) {
            return Err(ConfigError::DuplicateSelfId {
                self_id,
                first: previous,
                second: id,
            });
        }

        accounts.push(Account {
            id,
            self_id,
            ws_url,
            token: account.token.map(Secret::parse),
        });
    }

    Ok(accounts)
}

/// 一段可能来自环境的凭据。`Debug` 不打印值。
#[derive(Clone, PartialEq, Eq)]
pub enum Secret {
    /// 明文写在配置里（不推荐）。
    Literal(String),
    /// `env:NAME`，启动时从环境变量读取。
    Env(String),
}

impl Secret {
    fn parse(text: String) -> Self {
        match text.strip_prefix("env:") {
            Some(name) => Self::Env(name.to_owned()),
            None => Self::Literal(text),
        }
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Literal(_) => f.write_str("Secret::Literal(<redacted>)"),
            Self::Env(name) => write!(f, "Secret::Env({name})"),
        }
    }
}

// ---------------------------------------------------------------------------
// 人格
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Persona {
    pub id: String,
    pub source: PersonaSource,
}

#[derive(Debug, Clone)]
pub enum PersonaSource {
    Inline(String),
    File(String),
}

fn validate_personas(raw: Vec<RawPersona>) -> Result<Vec<Persona>, ConfigError> {
    let mut seen = BTreeSet::new();
    let mut personas = Vec::with_capacity(raw.len());

    for (index, persona) in raw.into_iter().enumerate() {
        let id = require_non_empty(persona.id, "personas[].id", index)?;
        if !seen.insert(id.clone()) {
            return Err(ConfigError::DuplicatePersonaId { id });
        }

        let source = match (persona.text, persona.file) {
            (Some(text), None) => PersonaSource::Inline(text),
            (None, Some(file)) => {
                PersonaSource::File(require_non_empty(file, "personas[].file", index)?)
            }
            (Some(_), Some(_)) => return Err(ConfigError::PersonaSourceAmbiguous { id }),
            (None, None) => return Err(ConfigError::PersonaSourceMissing { id }),
        };

        personas.push(Persona { id, source });
    }

    Ok(personas)
}

// ---------------------------------------------------------------------------
// 设备
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Device {
    pub id: String,
    pub persona: String,
    /// 记忆名。相同即共享记忆。
    pub memory: String,
    /// 这个设备可用的账号集合。
    pub accounts: Vec<String>,
    pub scopes: Vec<ScopeConfig>,
    pub wake: WakeConfig,
    pub hooks: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ScopeConfig {
    pub group: String,
    /// 从哪些账号的连接上收这个群。决定订阅关系。
    pub receive: Vec<String>,
    /// 回复时允许用哪些账号发。决定发送侧的故障轮换。
    pub send: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct WakeConfig {
    pub at_mention: bool,
    pub name_mention: bool,
    /// 名字与昵称。`name_mention` 生效时用这份列表匹配。
    pub names: Vec<String>,
    pub question: bool,
    pub keywords: Vec<String>,
    pub poke: bool,
    /// 普通消息的随机唤醒概率。
    pub probability: f64,
    pub any_message: bool,
    /// 在 `any_message` 生效时排除这些发言者（QQ 号）。
    pub ignore_speakers: BTreeSet<String>,
    pub sleep_until: Option<OffsetDateTime>,
    /// 群安静这么久后允许主动冒泡；`None` 表示关闭。
    pub idle_prompt_after_min: Option<u32>,
}

impl Default for WakeConfig {
    fn default() -> Self {
        // 省略时只响应 @ 与名字。
        Self {
            at_mention: true,
            name_mention: true,
            names: Vec::new(),
            question: false,
            keywords: Vec::new(),
            poke: false,
            probability: 0.0,
            any_message: false,
            ignore_speakers: BTreeSet::new(),
            sleep_until: None,
            idle_prompt_after_min: None,
        }
    }
}

fn validate_devices(
    raw: Vec<RawDevice>,
    accounts: &[Account],
    personas: &[Persona],
) -> Result<Vec<Device>, ConfigError> {
    if raw.is_empty() {
        return Err(ConfigError::NoDevices);
    }

    let account_ids: BTreeSet<&str> = accounts.iter().map(|a| a.id.as_str()).collect();
    let persona_ids: BTreeSet<&str> = personas.iter().map(|p| p.id.as_str()).collect();

    let mut seen_device_ids = BTreeSet::new();
    // (群, 账号) → 声明它的设备。冲突即启动失败，不降级。
    let mut claimed: BTreeMap<(String, String), String> = BTreeMap::new();
    let mut devices = Vec::with_capacity(raw.len());

    for (index, device) in raw.into_iter().enumerate() {
        let id = require_non_empty(device.id, "devices[].id", index)?;
        if !seen_device_ids.insert(id.clone()) {
            return Err(ConfigError::DuplicateDeviceId { id });
        }

        let persona = require_non_empty(device.persona, "devices[].persona", index)?;
        if !persona_ids.contains(persona.as_str()) {
            return Err(ConfigError::UnknownPersona {
                device: id,
                persona,
            });
        }

        let memory = require_non_empty(device.memory, "devices[].memory", index)?;

        if device.accounts.is_empty() {
            return Err(ConfigError::DeviceWithoutAccounts { device: id });
        }
        let mut device_accounts = Vec::with_capacity(device.accounts.len());
        for account in device.accounts {
            if !account_ids.contains(account.as_str()) {
                return Err(ConfigError::UnknownAccount {
                    device: id,
                    account,
                });
            }
            if device_accounts.contains(&account) {
                return Err(ConfigError::DuplicateDeviceAccount {
                    device: id,
                    account,
                });
            }
            device_accounts.push(account);
        }

        if device.scopes.is_empty() {
            return Err(ConfigError::DeviceWithoutScopes { device: id });
        }

        let mut scopes = Vec::with_capacity(device.scopes.len());
        let mut seen_groups = BTreeSet::new();
        for scope in device.scopes {
            let group = require_non_empty(scope.group, "devices[].scopes[].group", index)?;
            if !seen_groups.insert(group.clone()) {
                return Err(ConfigError::DuplicateScopeGroup { device: id, group });
            }

            if scope.receive.is_empty() {
                return Err(ConfigError::ScopeWithoutReceive { device: id, group });
            }
            if scope.send.is_empty() {
                return Err(ConfigError::ScopeWithoutSend { device: id, group });
            }

            for account in scope.receive.iter().chain(scope.send.iter()) {
                if !device_accounts.contains(account) {
                    return Err(ConfigError::ScopeAccountNotInDevice {
                        device: id,
                        group: group.clone(),
                        account: account.clone(),
                    });
                }
            }

            // 同一个 (群, 账号) 只能被一个设备声明。
            for account in &scope.receive {
                let key = (group.clone(), account.clone());
                if let Some(previous) = claimed.insert(key, id.clone()) {
                    return Err(ConfigError::ScopeClaimConflict {
                        group,
                        account: account.clone(),
                        first: previous,
                        second: id,
                    });
                }
            }

            scopes.push(ScopeConfig {
                group,
                receive: scope.receive,
                send: scope.send,
            });
        }

        let wake = device.wake.map(WakeConfig::try_from).transpose()?;

        devices.push(Device {
            id,
            persona,
            memory,
            accounts: device_accounts,
            scopes,
            wake: wake.unwrap_or_default(),
            hooks: device.hooks,
        });
    }

    Ok(devices)
}

impl TryFrom<RawWake> for WakeConfig {
    type Error = ConfigError;

    fn try_from(raw: RawWake) -> Result<Self, Self::Error> {
        let default = Self::default();

        if let Some(probability) = raw.probability
            && !(0.0..=1.0).contains(&probability)
        {
            return Err(ConfigError::ProbabilityOutOfRange { probability });
        }

        let sleep_until = match raw.sleep_until.as_deref() {
            None | Some("") => None,
            Some(text) => Some(OffsetDateTime::parse(text, &Rfc3339).map_err(|_| {
                ConfigError::BadSleepUntil {
                    value: text.to_owned(),
                }
            })?),
        };

        Ok(Self {
            at_mention: raw.at_mention.unwrap_or(default.at_mention),
            name_mention: raw.name_mention.unwrap_or(default.name_mention),
            names: raw.names.unwrap_or_default(),
            question: raw.question.unwrap_or(default.question),
            keywords: raw.keywords.unwrap_or_default(),
            poke: raw.poke.unwrap_or(default.poke),
            probability: raw.probability.unwrap_or(default.probability),
            any_message: raw.any_message.unwrap_or(default.any_message),
            ignore_speakers: raw
                .ignore_speakers
                .unwrap_or_default()
                .into_iter()
                .collect(),
            sleep_until,
            idle_prompt_after_min: raw.idle_prompt_after_min.filter(|minutes| *minutes > 0),
        })
    }
}

// ---------------------------------------------------------------------------
// 适配器与笔记
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct AdapterConfig {
    /// 内核监听，适配器连接。
    pub socket: String,
    /// 单回合上限，超时视为失败。
    pub turn_timeout_sec: u64,
}

impl Default for AdapterConfig {
    fn default() -> Self {
        Self {
            socket: "/run/user/1000/qqab.sock".to_owned(),
            turn_timeout_sec: 600,
        }
    }
}

impl AdapterConfig {
    fn validated(self) -> Result<Self, ConfigError> {
        if self.socket.trim().is_empty() {
            return Err(ConfigError::EmptySocket);
        }
        if self.turn_timeout_sec == 0 {
            return Err(ConfigError::ZeroTimeout);
        }
        Ok(self)
    }
}

#[derive(Debug, Clone)]
pub struct NotesConfig {
    /// 每记忆的笔记条数上限。
    pub max_entries: usize,
    /// 每回合注入的摘要预算，0 表示不注入。
    pub digest_budget_tokens: usize,
}

impl Default for NotesConfig {
    fn default() -> Self {
        Self {
            max_entries: 2000,
            digest_budget_tokens: 800,
        }
    }
}

impl NotesConfig {
    fn validated(self) -> Result<Self, ConfigError> {
        if self.max_entries == 0 {
            return Err(ConfigError::ZeroNoteCapacity);
        }
        Ok(self)
    }
}

// ---------------------------------------------------------------------------
// 原始 TOML 形态
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct RawConfig {
    #[serde(default)]
    accounts: Vec<RawAccount>,
    #[serde(default)]
    personas: Vec<RawPersona>,
    #[serde(default)]
    devices: Vec<RawDevice>,
    adapter: Option<RawAdapter>,
    notes: Option<RawNotes>,
}

#[derive(Debug, Deserialize)]
struct RawAccount {
    id: String,
    self_id: String,
    ws_url: String,
    token: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawPersona {
    id: String,
    text: Option<String>,
    file: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawDevice {
    id: String,
    persona: String,
    memory: String,
    #[serde(default)]
    accounts: Vec<String>,
    #[serde(default)]
    scopes: Vec<RawScope>,
    wake: Option<RawWake>,
    hooks: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawScope {
    group: String,
    #[serde(default)]
    receive: Vec<String>,
    #[serde(default)]
    send: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct RawWake {
    at_mention: Option<bool>,
    name_mention: Option<bool>,
    names: Option<Vec<String>>,
    question: Option<bool>,
    keywords: Option<Vec<String>>,
    poke: Option<bool>,
    probability: Option<f64>,
    any_message: Option<bool>,
    ignore_speakers: Option<Vec<String>>,
    sleep_until: Option<String>,
    idle_prompt_after_min: Option<u32>,
}

#[derive(Debug, Default, Deserialize)]
struct RawAdapter {
    socket: Option<String>,
    turn_timeout_sec: Option<u64>,
}

impl RawAdapter {
    fn validated(self) -> Result<AdapterConfig, ConfigError> {
        let default = AdapterConfig::default();
        AdapterConfig {
            socket: self.socket.unwrap_or(default.socket),
            turn_timeout_sec: self.turn_timeout_sec.unwrap_or(default.turn_timeout_sec),
        }
        .validated()
    }
}

#[derive(Debug, Default, Deserialize)]
struct RawNotes {
    max_entries: Option<usize>,
    digest_budget_tokens: Option<usize>,
}

impl RawNotes {
    fn validated(self) -> Result<NotesConfig, ConfigError> {
        let default = NotesConfig::default();
        NotesConfig {
            max_entries: self.max_entries.unwrap_or(default.max_entries),
            digest_budget_tokens: self
                .digest_budget_tokens
                .unwrap_or(default.digest_budget_tokens),
        }
        .validated()
    }
}

// ---------------------------------------------------------------------------
// 错误
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("TOML 语法错误")]
    Syntax {
        #[source]
        source: Box<toml::de::Error>,
    },
    #[error("至少需要一个账号")]
    NoAccounts,
    #[error("至少需要一个设备")]
    NoDevices,
    #[error("{field}[{index}] 不能为空")]
    EmptyField { field: &'static str, index: usize },
    #[error("账号 id {id} 重复")]
    DuplicateAccountId { id: String },
    #[error("QQ 号 {self_id} 被两个账号使用：{first}、{second}")]
    DuplicateSelfId {
        self_id: String,
        first: String,
        second: String,
    },
    #[error("人格 id {id} 重复")]
    DuplicatePersonaId { id: String },
    #[error("人格 {id} 同时给了 text 与 file，二选一")]
    PersonaSourceAmbiguous { id: String },
    #[error("人格 {id} 既没有 text 也没有 file")]
    PersonaSourceMissing { id: String },
    #[error("设备 id {id} 重复")]
    DuplicateDeviceId { id: String },
    #[error("设备 {device} 引用了不存在的人格 {persona}")]
    UnknownPersona { device: String, persona: String },
    #[error("设备 {device} 引用了不存在的账号 {account}")]
    UnknownAccount { device: String, account: String },
    #[error("设备 {device} 的 accounts 里有重复项 {account}")]
    DuplicateDeviceAccount { device: String, account: String },
    #[error("设备 {device} 没有任何账号")]
    DeviceWithoutAccounts { device: String },
    #[error("设备 {device} 没有任何范围")]
    DeviceWithoutScopes { device: String },
    #[error("设备 {device} 重复声明了群 {group}")]
    DuplicateScopeGroup { device: String, group: String },
    #[error("设备 {device} 的群 {group} 没有 receive 列表")]
    ScopeWithoutReceive { device: String, group: String },
    #[error("设备 {device} 的群 {group} 没有 send 列表")]
    ScopeWithoutSend { device: String, group: String },
    #[error("设备 {device} 的群 {group} 用了不在该设备 accounts 里的账号 {account}")]
    ScopeAccountNotInDevice {
        device: String,
        group: String,
        account: String,
    },
    #[error(
        "账号 {account} 在群 {group} 上被两个设备声明：{first}、{second}。\
         请删掉其中一个设备的该范围，或改用不同账号"
    )]
    ScopeClaimConflict {
        group: String,
        account: String,
        first: String,
        second: String,
    },
    #[error("wake.probability 必须在 0 到 1 之间，实际是 {probability}")]
    ProbabilityOutOfRange { probability: f64 },
    #[error("wake.sleep_until 不是合法 RFC 3339 时间：{value}")]
    BadSleepUntil { value: String },
    #[error("adapter.socket 不能为空")]
    EmptySocket,
    #[error("adapter.turn_timeout_sec 不能为 0")]
    ZeroTimeout,
    #[error("notes.max_entries 不能为 0")]
    ZeroNoteCapacity,
}

fn require_non_empty(
    value: String,
    field: &'static str,
    index: usize,
) -> Result<String, ConfigError> {
    if value.trim().is_empty() {
        return Err(ConfigError::EmptyField { field, index });
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"
[[accounts]]
id = "main"
self_id = "10001"
ws_url = "ws://127.0.0.1:6700"

[[personas]]
id = "whale"
text = "你是……"

[[devices]]
id = "alice"
persona = "whale"
memory = "alice"
accounts = ["main"]

  [[devices.scopes]]
  group = "20001"
  receive = ["main"]
  send = ["main"]
"#;

    #[test]
    fn minimal_config_parses_with_documented_defaults() {
        let config = Config::parse(MINIMAL).expect("应能解析");
        assert_eq!(config.accounts.len(), 1);
        assert_eq!(config.devices.len(), 1);

        let wake = &config.devices[0].wake;
        // 省略时只响应 @ 与名字。
        assert!(wake.at_mention);
        assert!(wake.name_mention);
        assert!(!wake.any_message);
        assert!(!wake.question);
        assert_eq!(wake.probability, 0.0);

        assert_eq!(config.adapter.turn_timeout_sec, 600);
        assert_eq!(config.notes.max_entries, 2000);
        assert_eq!(config.notes.digest_budget_tokens, 800);
    }

    #[test]
    fn scope_claim_conflict_fails_startup() {
        let text = r#"
[[accounts]]
id = "main"
self_id = "1"
ws_url = "ws://127.0.0.1:6700"

[[personas]]
id = "whale"
text = "p"

[[devices]]
id = "alice-phone"
persona = "whale"
memory = "alice"
accounts = ["main"]
  [[devices.scopes]]
  group = "20001"
  receive = ["main"]
  send = ["main"]

[[devices]]
id = "bob"
persona = "whale"
memory = "bob"
accounts = ["main"]
  [[devices.scopes]]
  group = "20001"
  receive = ["main"]
  send = ["main"]
"#;
        let error = Config::parse(text).expect_err("应冲突");
        assert!(
            matches!(error, ConfigError::ScopeClaimConflict { .. }),
            "得到 {error:?}"
        );
    }

    #[test]
    fn different_accounts_may_subscribe_same_group() {
        let text = r#"
[[accounts]]
id = "main"
self_id = "1"
ws_url = "ws://127.0.0.1:6700"

[[accounts]]
id = "backup"
self_id = "2"
ws_url = "ws://127.0.0.1:6701"

[[personas]]
id = "whale"
text = "p"

[[devices]]
id = "alice-phone"
persona = "whale"
memory = "alice"
accounts = ["main"]
  [[devices.scopes]]
  group = "20001"
  receive = ["main"]
  send = ["main"]

[[devices]]
id = "alice-pc"
persona = "whale"
memory = "alice"
accounts = ["backup"]
  [[devices.scopes]]
  group = "20001"
  receive = ["backup"]
  send = ["backup"]
"#;
        let config = Config::parse(text).expect("不同账号订阅同一个群是合法的");
        assert_eq!(config.devices.len(), 2);
        // 共享记忆：两个设备同一个记忆名。
        assert_eq!(config.devices[0].memory, config.devices[1].memory);
    }

    #[test]
    fn duplicate_self_id_rejected() {
        let text = r#"
[[accounts]]
id = "a"
self_id = "1"
ws_url = "ws://x"

[[accounts]]
id = "b"
self_id = "1"
ws_url = "ws://y"

[[personas]]
id = "p"
text = "p"

[[devices]]
id = "d"
persona = "p"
memory = "m"
accounts = ["a"]
  [[devices.scopes]]
  group = "1"
  receive = ["a"]
  send = ["a"]
"#;
        assert!(matches!(
            Config::parse(text).expect_err("应拒绝"),
            ConfigError::DuplicateSelfId { .. }
        ));
    }

    #[test]
    fn persona_needs_exactly_one_source() {
        let base = r#"
[[accounts]]
id = "a"
self_id = "1"
ws_url = "ws://x"
"#;
        let both = format!("{base}\n[[personas]]\nid = \"p\"\ntext = \"x\"\nfile = \"y\"\n");
        assert!(matches!(
            Config::parse(&both).expect_err("二选一"),
            ConfigError::PersonaSourceAmbiguous { .. }
        ));

        let neither = format!("{base}\n[[personas]]\nid = \"p\"\n");
        assert!(matches!(
            Config::parse(&neither).expect_err("必须有一个来源"),
            ConfigError::PersonaSourceMissing { .. }
        ));
    }

    #[test]
    fn scope_account_must_belong_to_device() {
        let text = r#"
[[accounts]]
id = "main"
self_id = "1"
ws_url = "ws://x"

[[accounts]]
id = "other"
self_id = "2"
ws_url = "ws://y"

[[personas]]
id = "p"
text = "p"

[[devices]]
id = "d"
persona = "p"
memory = "m"
accounts = ["main"]
  [[devices.scopes]]
  group = "1"
  receive = ["other"]
  send = ["main"]
"#;
        assert!(matches!(
            Config::parse(text).expect_err("范围账号必须属于设备"),
            ConfigError::ScopeAccountNotInDevice { .. }
        ));
    }

    #[test]
    fn wake_probability_is_bounded() {
        let text = MINIMAL.replace(
            "send = [\"main\"]",
            "send = [\"main\"]\n  [devices.wake]\n  probability = 1.5",
        );
        assert!(matches!(
            Config::parse(&text).expect_err("概率越界"),
            ConfigError::ProbabilityOutOfRange { .. }
        ));
    }

    #[test]
    fn wake_sleep_until_parses_rfc3339() {
        let text = MINIMAL.replace(
            "send = [\"main\"]",
            "send = [\"main\"]\n  [devices.wake]\n  sleep_until = \"2026-10-22T08:00:00+08:00\"",
        );
        let config = Config::parse(&text).expect("应能解析");
        assert!(config.devices[0].wake.sleep_until.is_some());
    }

    #[test]
    fn wake_sleep_until_empty_means_none() {
        let text = MINIMAL.replace(
            "send = [\"main\"]",
            "send = [\"main\"]\n  [devices.wake]\n  sleep_until = \"\"",
        );
        let config = Config::parse(&text).expect("应能解析");
        assert!(config.devices[0].wake.sleep_until.is_none());
    }

    #[test]
    fn idle_prompt_zero_disables() {
        let text = MINIMAL.replace(
            "send = [\"main\"]",
            "send = [\"main\"]\n  [devices.wake]\n  idle_prompt_after_min = 0",
        );
        let config = Config::parse(&text).expect("应能解析");
        assert!(config.devices[0].wake.idle_prompt_after_min.is_none());
    }

    #[test]
    fn secret_debug_does_not_leak_literal() {
        let secret = Secret::parse("super-secret-value".to_owned());
        let printed = format!("{secret:?}");
        assert!(!printed.contains("super-secret-value"), "得到 {printed}");
        assert!(printed.contains("redacted"));

        let from_env = Secret::parse("env:QQ_TOKEN".to_owned());
        assert_eq!(from_env, Secret::Env("QQ_TOKEN".to_owned()));
        assert!(format!("{from_env:?}").contains("QQ_TOKEN"));
    }

    #[test]
    fn zero_timeout_rejected() {
        let text = format!("{MINIMAL}\n[adapter]\nturn_timeout_sec = 0\n");
        assert!(matches!(
            Config::parse(&text).expect_err("超时不能为 0"),
            ConfigError::ZeroTimeout
        ));
    }
}
