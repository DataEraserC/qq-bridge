//! 配置：连接、人格、记忆、harness、harness 会话、订阅、三条策略。
//!
//! 五个一等对象，从下往上（ADR-0017 五对象模型）：
//! 1. **连接**：一条通到平台的管道（适配器种类 + 连接信息 + 登录账号）。同一平台可以有
//!    多条，每条一个账号——一个账号一条连接是平台的硬事实，不是我们的选择。
//! 2. **人格**：一张纯文本人设卡（id + name + 正文）。只管「是谁、什么语气、什么不能漏」，
//!    不带记忆、不带订阅、不声明能力（能力由会话配置与 harness/适配器决定）。
//! 3. **记忆**：一份长期知识，抽象接口（id + name + type + 位置参数）。type 可扩展，
//!    当前第一种是 git。可被多个会话共享，最终一致。
//! 4. **harness**：一个接管者（id + name）。桥不主动连它——它反过来连桥报名字 + 令牌。
//!    桥只存「有哪些 harness」用来校验会话的引用，不存它的连接地址。
//! 5. **harness 会话**：一个串行单元（id + name 可空 + harness id + 人格 id + 记忆 ids
//!    可空 + model 可空 + 订阅）。串行键 = harness id + 会话 id；同一会话内严格串行。
//!
//! 配置从 TOML 读进来后**完全校验**：非法配置在这里就死，不会带病启动。

use acb_proto::RoomId;
use serde::Deserialize;
use thiserror::Error;

/// 没配「攒多少条」时的默认值。来源：实现默认，可推翻。
pub const DEFAULT_ACCUMULATE: u32 = 10;

/// 没配存储路径时的默认值：工作目录下的 `state/acb.db`。
/// 来源：实现默认。放 `state/` 是因为那里已经在 .gitignore 里，不会被误提交。
pub const DEFAULT_STORAGE_PATH: &str = "state/acb.db";

/// 没配「每个房间留多少条」时的默认值。来源：实现默认。
/// 5000 条对一个群来说大概是几个月的体量，够 `get_history` 往前翻。
pub const DEFAULT_MAX_RECORDS_PER_ROOM: u64 = 5000;

/// 没配「留多少天」时的默认值。来源：实现默认。
pub const DEFAULT_MAX_AGE_DAYS: u64 = 90;

/// 一轮投递最多塞多少条新记录的默认值。来源：实现默认，可推翻。
/// 超过这个数就只投前这么多条，末尾提示「还有 N 条，用 fetch_records 抓」。
pub const DEFAULT_DELIVERY_LIMIT: usize = 30;

/// `[file_relay].dir` 不写时的默认值。跟 [storage].path 一样相对启动目录。
pub const DEFAULT_RELAY_DIR: &str = "state/relay";

/// 没配 worktree 根目录时的默认值：工作目录下的 `state/worktrees`。
/// 来源：实现默认。放 `state/` 是因为那里已经在 .gitignore 里。
/// 每个「会话 × 记忆」的 worktree 按 `根/记忆id/会话id` 分层，路径固定才谈得上幂等。
pub const DEFAULT_WORKTREES_ROOT: &str = "state/worktrees";

/// 整份配置。
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    /// 人格列表。
    pub personas: Vec<Persona>,
    /// 记忆列表。
    pub memories: Vec<Memory>,
    /// harness 列表。一等对象：会话用它引用「归谁接管」。
    pub harnesses: Vec<Harness>,
    /// harness 会话列表。
    pub sessions: Vec<Session>,
    /// 接入点配置。没配就不监听——只跑内存里的桥（测试、嵌入式）。
    pub endpoint: Option<EndpointConfig>,
    /// 控制台配置。没配就不起控制台（ADR-0018）。
    pub console: Option<ConsoleConfig>,
    /// 文件中继配置。没配就不起文件服务，本机路径原样透传（ADR-0025）。
    pub file_relay: Option<FileRelayConfig>,
    /// 平台侧连接。一个账号一条，可以有多条。
    pub connections: Vec<Connection>,
    /// 状态存哪。流水、水位、笔记、投递计数都在这里，重启不改。
    pub storage: StorageConfig,
    /// 流水保留：旧记录什么时候可以删。
    pub retention: RetentionConfig,
    /// 记忆 worktree 的根目录。每个「会话 × 记忆」checkout 到这下面。
    pub worktrees_root: String,
    /// 会话**没配自己的 `on_turn_error`** 时的默认失败通知动作列表。
    /// 空 = 只记日志。写法同会话级 `on_turn_error`（`log` / `send:<房间>`）。
    pub default_on_turn_error: TurnErrorActions,
    /// 一轮投递最多塞多少条新记录。超过就只投前 `delivery_limit` 条，并在末尾附一句
    /// 「还有 N 条没投，用 fetch_records 抓」。0 或不配 = 不截断（照旧投满 MAX_DELIVERY）。
    pub delivery_limit: usize,
}

/// 一轮失败后要做的通知动作。
#[derive(Debug, Clone, PartialEq)]
pub enum TurnErrorAction {
    /// 记进桥日志（默认行为，永远可做）。
    Log,
    /// 把失败信息作为一条消息发到指定房间。
    Send { room: RoomId },
}

/// 一轮失败时按顺序执行的「通知动作」列表（挂在**会话**上，不是全局）。
///
/// 每个动作是一条字符串：`log` 记进日志；`send:<房间>` 把失败信息作为一条消息
/// 发到指定房间（房间写法同订阅，如 `send:qq:group/123`）。顺序执行，一个动作
/// 失败不打断后面的。空 = 只记日志（现状）。
pub type TurnErrorActions = Vec<TurnErrorAction>;

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

/// 连接方式。协议相同、连接方向不同，就是两种连接。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// 正向 WebSocket：桥当客户端，主动连平台开出来的 WS 服务端。
    WsClient,
    /// 反向 WebSocket：桥当服务端，等平台连过来。还没实现。
    WsServer,
}

impl Transport {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::WsClient => "ws-client",
            Self::WsServer => "ws-server",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "ws-client" => Some(Self::WsClient),
            "ws-server" => Some(Self::WsServer),
            _ => None,
        }
    }
}

/// 一条连接到平台的管道。
#[derive(Debug, Clone, PartialEq)]
pub struct Connection {
    /// 连接名。房间、订阅都靠它引用；也出现在给模型看的房间串里。
    pub id: String,
    /// 平台名，如 `qq`。给日志、平台特有内容、显示用。
    pub platform: String,
    /// 适配器种类，目前只有 `onebot11`。
    pub kind: String,
    /// 连接方向。
    pub transport: Transport,
    /// 连接地址，形如 `ws://127.0.0.1:3001`。
    pub url: String,
    /// 访问令牌。OneBot 实现开了 token 就要填，握手时放在 `Authorization` 头里。
    pub token: Option<String>,
    /// 这条连接登录的账号。
    ///
    /// 必填：桥要靠它分辨「这条消息是不是我自己发的」。分不出来就会自己回自己，
    /// 而且水位会被自己的话挡住。连上之后还会跟平台报的号核对一次。
    pub account: String,
    /// 系统管理员的账号列表。发送者号命中其中之一，就标 `SenderRole::SystemAdmin`。
    /// 空 = 没有系统管理员。适配器判身份时先查这个，查不到再去问平台群角色。
    pub admin_qqs: Vec<String>,
    /// 破坏性平台方法（踢人、禁言、解散群……）放不放行。默认全拒。
    ///
    /// 这是「AI 能做任何人也做的事」和「群消息是不可信输入」之间唯一的那道闸：
    /// 白名单挡不住「模型合法地调一个它本来就有权调的方法」，只有默认关才挡得住。
    pub allow_destructive: DestructivePolicy,
}

/// 破坏性方法的放行策略。
#[derive(Debug, Clone, PartialEq, Default)]
pub enum DestructivePolicy {
    /// 一律拒绝（默认）。
    #[default]
    None,
    /// 全部放行。
    All,
    /// 只放行这几个方法名。
    Only(Vec<String>),
}

impl DestructivePolicy {
    /// 这个方法现在能不能调。非破坏性方法的判断不归它管。
    pub fn allows(&self, method: &str) -> bool {
        match self {
            Self::None => false,
            Self::All => true,
            Self::Only(names) => names.iter().any(|name| name == method),
        }
    }
}

/// 目前认识的适配器种类。加新适配器时在这里登记。
pub const KNOWN_ADAPTERS: &[&str] = &["onebot11"];

/// 目前认识的记忆存储形式。加新形式时在这里登记。
pub const KNOWN_MEMORY_KINDS: &[&str] = &["git"];

/// 接入点：harness 连进来的地址。
#[derive(Debug, Clone, PartialEq)]
pub struct EndpointConfig {
    /// 监听地址，形如 `127.0.0.1:7700`。
    pub listen: String,
}

/// 控制台：桥侧的统一管理 HTTP 服务（ADR-0018）。
///
/// 可远程访问（监听地址可配到非回环），因此 token 鉴权必填、必须认真校验。
#[derive(Debug, Clone, PartialEq)]
pub struct ConsoleConfig {
    /// 监听地址，形如 `0.0.0.0:3100` 或 `127.0.0.1:3100`。
    pub listen: String,
    /// 鉴权 token。所有请求都要带（`Authorization: Bearer <token>`）。
    pub token: String,
}

/// 文件中继（ADR-0025）。agent 和平台不共用文件系统时，桥把本机文件收下来
/// 换成平台取得到的 URL。没配就不起文件服务，本机路径原样透传。
#[derive(Debug, Clone, PartialEq)]
pub struct FileRelayConfig {
    /// 监听地址，如 `0.0.0.0:3131`。
    pub listen: String,
    /// 平台那边访问桥用的基地址（不含尾斜杠），如 `http://10.89.0.1:3131`。
    /// 平台在容器里时通常写宿主网关地址。
    pub public_base: String,
    /// 收下的文件落盘位置。
    pub dir: String,
}

/// 一张人格卡：纯文本人设。
#[derive(Debug, Clone, PartialEq)]
pub struct Persona {
    pub id: String,
    /// 人设名，给人看的。
    pub name: String,
    /// 人设正文：「你是谁、什么性格、什么语气」，以及信息边界（什么不能漏）。
    pub text: String,
}

/// 一份记忆：抽象接口。`type` 决定底层存储，当前第一种是 `git`。
#[derive(Debug, Clone, PartialEq)]
pub struct Memory {
    pub id: String,
    /// 记忆名，给人看的。
    pub name: String,
    /// 存储形式，如 `git`。可扩展。
    pub kind: String,
    /// 与 `kind` 对应的位置/参数。`git` 下是仓库路径。
    pub location: String,
}

/// 一个 harness：一等对象，会话靠它引用「归谁接管」。
///
/// 桥不主动连 harness——是 harness 反过来连桥、报出名字 + 令牌。所以桥对 harness
/// 只需要知道「有哪些、id 是什么」用来校验会话的引用，不存它的连接地址或内部配置。
#[derive(Debug, Clone, PartialEq)]
pub struct Harness {
    pub id: String,
    /// 显示名，给人看的。
    pub name: String,
}

/// 一个 harness 会话：一个串行单元。
#[derive(Debug, Clone, PartialEq)]
pub struct Session {
    pub id: String,
    /// 这个会话归哪个 harness 接管，引用 `harnesses` 里的某个 id。
    /// 串行键 = harness 名 + 会话 id。
    pub harness: String,
    /// 会话显示名，给人看的。可空，缺省 = id。
    pub name: String,
    /// 这个会话用哪个模型。可空 = 由 harness 侧自己决定。
    pub model: Option<String>,
    /// 人格卡 id。
    pub persona: String,
    /// 引用的记忆 id 列表。可空（空 = 不存长期记忆）。
    pub memories: Vec<String>,
    /// 接入令牌。harness 握手时报出令牌与 harness 名，桥据此绑定会话。
    pub token: String,
    /// 订阅列表。
    pub subscriptions: Vec<Subscription>,
    /// 这个会话一轮失败时按顺序执行的「通知动作」列表。空 = 只记日志。
    /// 见 [`TurnErrorAction`]。
    pub on_turn_error: TurnErrorActions,
}

/// 一条订阅：连接 + 房间 + 唤醒策略。
#[derive(Debug, Clone, PartialEq)]
pub struct Subscription {
    /// 房间（连接名 + 路径）。
    pub room: RoomId,
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
    #[error("会话 {session} 的 {field} 不能为空")]
    EmptyField {
        session: String,
        field: &'static str,
    },
    #[error("会话 id 重复：{session}")]
    DuplicateSession { session: String },
    #[error("会话 {session} 重复订阅了房间 {room}")]
    DuplicateRoom { session: String, room: String },
    #[error("会话 {session} 的房间标识不合法：{room}（{detail}）")]
    BadRoom {
        session: String,
        room: String,
        detail: String,
    },
    #[error("会话 {session} 订阅了 {room}，但没有名为 {connection} 的连接")]
    UnknownConnection {
        session: String,
        room: String,
        connection: String,
    },
    #[error("会话 {session} 的房间 {room} 的 accumulate 必须是正整数")]
    BadAccumulate { session: String, room: String },
    #[error("会话 {session} 的接入令牌不能为空")]
    EmptyToken { session: String },
    #[error("会话 {session} 引用了不存在的人格 {persona}")]
    UnknownPersona { session: String, persona: String },
    #[error("会话 {session} 引用了不存在的记忆 {memory}")]
    UnknownMemory { session: String, memory: String },
    #[error("人格 id 重复：{persona}")]
    DuplicatePersona { persona: String },
    #[error("人格 {persona} 的 {field} 不能为空")]
    EmptyPersonaField {
        persona: String,
        field: &'static str,
    },
    #[error("记忆 id 重复：{memory}")]
    DuplicateMemory { memory: String },
    #[error("记忆 {memory} 的 {field} 不能为空")]
    EmptyMemoryField { memory: String, field: &'static str },
    #[error("记忆 {memory} 的种类不认识：{kind}（目前只有 git）")]
    UnknownMemoryKind { memory: String, kind: String },
    #[error("harness id 重复：{harness}")]
    DuplicateHarness { harness: String },
    #[error("harness {harness} 的 {field} 不能为空")]
    EmptyHarnessField {
        harness: String,
        field: &'static str,
    },
    #[error("会话 {session} 引用了不存在的 harness {harness}")]
    UnknownHarness { session: String, harness: String },
    #[error("接入点监听地址不合法：{address}（{detail}）")]
    BadListen { address: String, detail: String },
    #[error("连接名不能为空")]
    EmptyConnectionId,
    #[error("连接名重复：{connection}")]
    DuplicateConnection { connection: String },
    #[error("连接 {connection} 的 platform 不能为空")]
    EmptyConnectionPlatform { connection: String },
    #[error("连接 {connection} 的种类不认识：{kind}（目前只有 onebot11）")]
    UnknownKind { connection: String, kind: String },
    #[error("连接 {connection} 的连接方式不认识：{transport}（ws-client / ws-server）")]
    UnknownTransport {
        connection: String,
        transport: String,
    },
    #[error("连接 {connection} 用的是 {transport}，这个还没实现（现在只有 ws-client）")]
    UnsupportedTransport {
        connection: String,
        transport: &'static str,
    },
    #[error("连接 {connection} 的地址不合法：{url}（ws-client 要写成 ws://主机:端口/路径）")]
    BadUrl { connection: String, url: String },
    #[error("连接 {connection} 的登录账号不能为空——桥要靠它分辨「哪句是我说的」")]
    EmptyAccount { connection: String },
    #[error("存储路径不能为空（不配就用默认的 {DEFAULT_STORAGE_PATH}）")]
    EmptyStoragePath,
    #[error("worktree 根目录不能为空（不配就用默认的 {DEFAULT_WORKTREES_ROOT}）")]
    EmptyWorktreesRoot,
    #[error("失败通知动作不合法：{action}（要写成 log 或 send:连接:段/段）")]
    BadTurnErrorAction { action: String },
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
        let default_on_turn_error = parse_turn_error_actions(&raw.default_on_turn_error)?;

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

        let worktrees_root = match raw.worktrees_root {
            Some(raw) => {
                let root = raw.trim().to_owned();
                if root.is_empty() {
                    return Err(ConfigError::EmptyWorktreesRoot);
                }
                root
            }
            None => DEFAULT_WORKTREES_ROOT.to_owned(),
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

        let console = match raw.console {
            Some(raw) => {
                let address = raw.listen.trim().to_owned();
                if address.parse::<std::net::SocketAddr>().is_err() {
                    return Err(ConfigError::BadListen {
                        address,
                        detail: "控制台要写成 主机:端口，如 127.0.0.1:3100".into(),
                    });
                }
                let token = raw.token.trim().to_owned();
                if token.is_empty() {
                    return Err(ConfigError::BadListen {
                        address,
                        detail: "控制台可远程访问，token 不能为空".into(),
                    });
                }
                Some(ConsoleConfig { listen: address, token })
            }
            None => None,
        };

        let file_relay = match raw.file_relay {
            Some(raw) => {
                let address = raw.listen.trim().to_owned();
                if address.parse::<std::net::SocketAddr>().is_err() {
                    return Err(ConfigError::BadListen {
                        address,
                        detail: "文件中继要写成 主机:端口，如 0.0.0.0:3131".into(),
                    });
                }
                let public_base = raw.public_base.trim().trim_end_matches('/').to_owned();
                if !public_base.starts_with("http://") && !public_base.starts_with("https://") {
                    return Err(ConfigError::BadListen {
                        address,
                        detail: "file_relay.public_base 要写平台那边访问桥的完整地址，\
                                 如 http://10.89.0.1:3131（平台在容器里时通常是宿主网关）"
                            .into(),
                    });
                }
                Some(FileRelayConfig {
                    listen: address,
                    public_base,
                    dir: raw.dir.unwrap_or_else(|| DEFAULT_RELAY_DIR.to_owned()),
                })
            }
            None => None,
        };

        let mut connections: Vec<Connection> = Vec::with_capacity(raw.connections.len());
        for raw_connection in raw.connections {
            let connection = build_connection(raw_connection)?;
            if connections.iter().any(|seen| seen.id == connection.id) {
                return Err(ConfigError::DuplicateConnection {
                    connection: connection.id,
                });
            }
            connections.push(connection);
        }

        // 人格：一张卡一份，id 唯一。
        let mut personas: Vec<Persona> = Vec::with_capacity(raw.personas.len());
        for raw_persona in raw.personas {
            let id = raw_persona.id.trim().to_owned();
            if id.is_empty() {
                return Err(ConfigError::EmptyPersonaField {
                    persona: id,
                    field: "id",
                });
            }
            if personas.iter().any(|seen| seen.id == id) {
                return Err(ConfigError::DuplicatePersona { persona: id });
            }
            let name = raw_persona.name.trim().to_owned();
            if name.is_empty() {
                return Err(ConfigError::EmptyPersonaField {
                    persona: id,
                    field: "name",
                });
            }
            let text = raw_persona.text.trim().to_owned();
            if text.is_empty() {
                return Err(ConfigError::EmptyPersonaField {
                    persona: id,
                    field: "text",
                });
            }
            personas.push(Persona { id, name, text });
        }

        // 记忆：一份一个 id，kind 目前只有 git。
        let mut memories: Vec<Memory> = Vec::with_capacity(raw.memories.len());
        for raw_memory in raw.memories {
            let id = raw_memory.id.trim().to_owned();
            if id.is_empty() {
                return Err(ConfigError::EmptyMemoryField {
                    memory: id,
                    field: "id",
                });
            }
            if memories.iter().any(|seen| seen.id == id) {
                return Err(ConfigError::DuplicateMemory { memory: id });
            }
            let name = raw_memory.name.trim().to_owned();
            if name.is_empty() {
                return Err(ConfigError::EmptyMemoryField {
                    memory: id,
                    field: "name",
                });
            }
            let kind = raw_memory.kind.trim().to_owned();
            if !KNOWN_MEMORY_KINDS.contains(&kind.as_str()) {
                return Err(ConfigError::UnknownMemoryKind { memory: id, kind });
            }
            let location = raw_memory.location.trim().to_owned();
            if location.is_empty() {
                return Err(ConfigError::EmptyMemoryField {
                    memory: id,
                    field: "location",
                });
            }
            memories.push(Memory {
                id,
                name,
                kind,
                location,
            });
        }

        // harness：一等对象，一个 id 一个。会话靠它引用「归谁接管」。
        let mut harnesses: Vec<Harness> = Vec::with_capacity(raw.harnesses.len());
        for raw_harness in raw.harnesses {
            let id = raw_harness.id.trim().to_owned();
            if id.is_empty() {
                return Err(ConfigError::EmptyHarnessField {
                    harness: id,
                    field: "id",
                });
            }
            if harnesses.iter().any(|seen| seen.id == id) {
                return Err(ConfigError::DuplicateHarness { harness: id });
            }
            let name = raw_harness.name.trim().to_owned();
            if name.is_empty() {
                return Err(ConfigError::EmptyHarnessField {
                    harness: id,
                    field: "name",
                });
            }
            harnesses.push(Harness { id, name });
        }

        let mut sessions = Vec::with_capacity(raw.sessions.len());
        let mut seen_sessions: Vec<String> = Vec::new();

        for raw_session in raw.sessions {
            let id = raw_session.id;
            if seen_sessions.iter().any(|seen| seen == &id) {
                return Err(ConfigError::DuplicateSession { session: id });
            }
            seen_sessions.push(id.clone());

            check_not_blank(&id, "id").map_err(|field| ConfigError::EmptyField {
                session: id.clone(),
                field,
            })?;
            check_not_blank(&raw_session.harness, "harness").map_err(|field| {
                ConfigError::EmptyField {
                    session: id.clone(),
                    field,
                }
            })?;
            check_not_blank(&raw_session.persona, "persona").map_err(|field| {
                ConfigError::EmptyField {
                    session: id.clone(),
                    field,
                }
            })?;
            check_not_blank(&raw_session.token, "token").map_err(|_| ConfigError::EmptyToken {
                session: id.clone(),
            })?;

            // 引用的 harness 必须真的存在。
            let harness = raw_session.harness.trim().to_owned();
            if !harnesses.iter().any(|h| h.id == harness) {
                return Err(ConfigError::UnknownHarness {
                    session: id.clone(),
                    harness,
                });
            }

            // 引用的人格与记忆必须真的存在。
            if !personas
                .iter()
                .any(|persona| persona.id == raw_session.persona)
            {
                return Err(ConfigError::UnknownPersona {
                    session: id.clone(),
                    persona: raw_session.persona.clone(),
                });
            }
            for memory in &raw_session.memories {
                if !memories.iter().any(|m| &m.id == memory) {
                    return Err(ConfigError::UnknownMemory {
                        session: id.clone(),
                        memory: memory.clone(),
                    });
                }
            }

            let mut subscriptions = Vec::with_capacity(raw_session.subscriptions.len());
            let mut seen_rooms: Vec<RoomId> = Vec::new();
            for raw_subscription in raw_session.subscriptions {
                let subscription = build_subscription(&id, raw_subscription)?;
                // 订阅连的连接必须真的存在，否则这个房间永远发不出去。
                if !connections
                    .iter()
                    .any(|connection| connection.id == subscription.room.connection)
                {
                    return Err(ConfigError::UnknownConnection {
                        session: id.clone(),
                        room: subscription.room.to_string(),
                        connection: subscription.room.connection.clone(),
                    });
                }
                if seen_rooms.iter().any(|room| room == &subscription.room) {
                    return Err(ConfigError::DuplicateRoom {
                        session: id.clone(),
                        room: subscription.room.to_string(),
                    });
                }
                seen_rooms.push(subscription.room.clone());
                subscriptions.push(subscription);
            }

            // name 可空：缺省 = id。model 可空：由 harness 侧决定。
            let name = {
                let trimmed = raw_session.name.trim().to_owned();
                if trimmed.is_empty() {
                    id.clone()
                } else {
                    trimmed
                }
            };
            let model = raw_session
                .model
                .map(|m| {
                    let trimmed = m.trim().to_owned();
                    if trimmed.is_empty() {
                        None
                    } else {
                        Some(trimmed)
                    }
                })
                .flatten();

            let on_turn_error = parse_turn_error_actions(&raw_session.on_turn_error)?;

            sessions.push(Session {
                id,
                harness,
                name,
                model,
                persona: raw_session.persona,
                memories: raw_session.memories,
                token: raw_session.token.trim().to_owned(),
                subscriptions,
                on_turn_error,
            });
        }

        Ok(Self {
            personas,
            memories,
            harnesses,
            sessions,
            endpoint,
            console,
            file_relay,
            connections,
            storage,
            retention,
            worktrees_root,
            default_on_turn_error,
            delivery_limit: raw.delivery_limit.unwrap_or(DEFAULT_DELIVERY_LIMIT),
        })
    }

    /// 找某条连接的配置。
    pub fn connection(&self, id: &str) -> Option<&Connection> {
        self.connections
            .iter()
            .find(|connection| connection.id == id)
    }

    /// 找一张人格卡。
    pub fn persona(&self, id: &str) -> Option<&Persona> {
        self.personas.iter().find(|persona| persona.id == id)
    }

    /// 找一份记忆。
    pub fn memory(&self, id: &str) -> Option<&Memory> {
        self.memories.iter().find(|memory| memory.id == id)
    }

    /// 找一个 harness。
    pub fn harness(&self, id: &str) -> Option<&Harness> {
        self.harnesses.iter().find(|harness| harness.id == id)
    }

    /// 找一个会话。
    pub fn session(&self, id: &str) -> Option<&Session> {
        self.sessions.iter().find(|session| session.id == id)
    }

    /// 找会话的某条订阅。发送与工具鉴权都从这里查。
    pub fn subscription(&self, session: &str, room: &RoomId) -> Option<&Subscription> {
        self.sessions
            .iter()
            .find(|candidate| candidate.id == session)?
            .subscriptions
            .iter()
            .find(|subscription| &subscription.room == room)
    }

    /// 哪（几）个会话订阅了这个房间。
    pub fn subscribers_of(&self, room: &RoomId) -> Vec<&Session> {
        self.sessions
            .iter()
            .filter(|session| {
                session
                    .subscriptions
                    .iter()
                    .any(|subscription| &subscription.room == room)
            })
            .collect()
    }

    /// 房间所属连接的登录账号。房间里没有这个连接就返回空串。
    pub fn account_of_room(&self, room: &RoomId) -> String {
        self.connection(&room.connection)
            .map(|connection| connection.account.clone())
            .unwrap_or_default()
    }

    /// 这个号是不是房间所属连接点名的系统管理员。
    pub fn is_admin(&self, room: &RoomId, sender_id: &str) -> bool {
        self.connection(&room.connection)
            .map(|connection| connection.admin_qqs.iter().any(|qq| qq == sender_id))
            .unwrap_or(false)
    }

    /// 按 harness 名找它接管的会话。握手时用：harness 报出名字 + 令牌，
    /// 令牌对上，这条连接就代表这个名字下它接管的那组会话。
    pub fn sessions_by_harness(&self, harness: &str) -> Vec<&Session> {
        self.sessions
            .iter()
            .filter(|candidate| candidate.harness == harness)
            .collect()
    }

    /// 启动时要打给人看的提醒。都不阻止启动——用户故意的配置也得能跑。
    pub fn warnings(&self) -> Vec<String> {
        let mut warnings = Vec::new();

        // 同一房间被多个会话订阅：谁说话就说不准了，发消息时要指明会话。
        let mut rooms: Vec<(String, Vec<&str>)> = Vec::new();
        for session in &self.sessions {
            for subscription in &session.subscriptions {
                let name = subscription.room.to_string();
                match rooms.iter_mut().find(|(room, _)| *room == name) {
                    Some((_, sessions)) => sessions.push(session.id.as_str()),
                    None => rooms.push((name, vec![session.id.as_str()])),
                }
            }
        }
        for (room, sessions) in rooms {
            if sessions.len() > 1 {
                warnings.push(format!(
                    "房间 {room} 同时被会话 {} 订阅：两边各有各的水位，\
                     发消息到这个房间时要用 session 指明是谁在说。",
                    sessions.join("、")
                ));
            }
        }

        warnings
    }
}

fn build_connection(raw: RawConnection) -> Result<Connection, ConfigError> {
    let id = raw.id.trim().to_owned();
    if id.is_empty() {
        return Err(ConfigError::EmptyConnectionId);
    }

    let platform = raw.platform.trim().to_owned();
    if platform.is_empty() {
        return Err(ConfigError::EmptyConnectionPlatform { connection: id });
    }

    let kind = raw.kind.trim().to_owned();
    if !KNOWN_ADAPTERS.contains(&kind.as_str()) {
        return Err(ConfigError::UnknownKind {
            connection: id,
            kind,
        });
    }

    let transport_text = raw.transport.trim().to_owned();
    let transport =
        Transport::parse(&transport_text).ok_or_else(|| ConfigError::UnknownTransport {
            connection: id.clone(),
            transport: transport_text.clone(),
        })?;
    if transport == Transport::WsServer {
        return Err(ConfigError::UnsupportedTransport {
            connection: id,
            transport: "ws-server",
        });
    }

    let url = raw.url.trim().to_owned();
    // 只支持不加密的 ws://：wss 要拉 TLS 依赖，等真有需求再说。
    if !(url.starts_with("ws://") && url.len() > "ws://".len()) {
        return Err(ConfigError::BadUrl {
            connection: id,
            url,
        });
    }

    let account = raw.account.map(|value| value.trim().to_owned());
    let account = match account {
        Some(account) if !account.is_empty() => account,
        _ => return Err(ConfigError::EmptyAccount { connection: id }),
    };

    Ok(Connection {
        id,
        platform,
        kind,
        transport,
        url,
        token: raw
            .token
            .map(|token| token.trim().to_owned())
            .filter(|token| !token.is_empty()),
        account,
        admin_qqs: raw
            .admin_qqs
            .into_iter()
            .map(|qq| qq.trim().to_owned())
            .filter(|qq| !qq.is_empty())
            .collect(),
        allow_destructive: raw
            .allow_destructive
            .map(RawDestructive::into_policy)
            .unwrap_or_default(),
    })
}

fn build_subscription(session: &str, raw: RawSubscription) -> Result<Subscription, ConfigError> {
    let room = RoomId::new(raw.connection.trim(), raw.room.trim()).map_err(|error| {
        ConfigError::BadRoom {
            session: session.to_owned(),
            room: format!("{}:{}", raw.connection.trim(), raw.room.trim()),
            detail: error.to_string(),
        }
    })?;

    let wake = build_wake(session, &room, raw.wake.unwrap_or_default())?;

    Ok(Subscription { room, wake })
}

fn build_wake(session: &str, room: &RoomId, raw: RawWake) -> Result<WakePolicy, ConfigError> {
    let default = WakePolicy::default();

    if let Some(accumulate) = raw.accumulate {
        if accumulate == 0 {
            return Err(ConfigError::BadAccumulate {
                session: session.to_owned(),
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

/// 把 TOML 里的失败通知动作字符串列表解析成 [`TurnErrorAction`]。
///
/// 每条是 `log` 或 `send:<连接>:<段/段>`（房间写法同订阅，如 `send:qq:group/123`）。
/// 解析不了就报错——这是配置层面的错，不该带病启动。
fn parse_turn_error_actions(raw: &[String]) -> Result<Vec<TurnErrorAction>, ConfigError> {
    let mut actions = Vec::with_capacity(raw.len());
    for entry in raw {
        let action = entry.trim();
        if action == "log" {
            actions.push(TurnErrorAction::Log);
            continue;
        }
        if let Some(room_text) = action.strip_prefix("send:") {
            // 房间写法的第一段是连接名，后面是段/段；这里拆出连接名和路径。
            let room_text = room_text.trim();
            let (connection, path) = match room_text.split_once(':') {
                Some((connection, path)) => (connection, path),
                None => {
                    return Err(ConfigError::BadTurnErrorAction {
                        action: entry.clone(),
                    });
                }
            };
            let room = RoomId::new(connection.trim(), path.trim()).map_err(|_| {
                ConfigError::BadTurnErrorAction {
                    action: entry.clone(),
                }
            })?;
            actions.push(TurnErrorAction::Send { room });
            continue;
        }
        return Err(ConfigError::BadTurnErrorAction {
            action: entry.clone(),
        });
    }
    Ok(actions)
}

// ---------------------------------------------------------------------------
// 原始 TOML 形态
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct RawConfig {
    #[serde(default)]
    personas: Vec<RawPersona>,
    #[serde(default)]
    memories: Vec<RawMemory>,
    #[serde(default)]
    harnesses: Vec<RawHarness>,
    #[serde(default)]
    sessions: Vec<RawSession>,
    endpoint: Option<RawEndpoint>,
    console: Option<RawConsole>,
    file_relay: Option<RawFileRelay>,
    #[serde(default)]
    connections: Vec<RawConnection>,
    storage: Option<RawStorage>,
    retention: Option<RawRetention>,
    worktrees_root: Option<String>,
    /// 会话没配 `on_turn_error` 时的默认动作列表（默认空 = 只记日志）。
    #[serde(default)]
    default_on_turn_error: Vec<String>,
    /// 一轮投递最多塞多少条。0 = 不截断。
    #[serde(default)]
    delivery_limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct RawPersona {
    id: String,
    name: String,
    text: String,
}

#[derive(Debug, Deserialize)]
struct RawMemory {
    id: String,
    name: String,
    kind: String,
    location: String,
}

#[derive(Debug, Deserialize)]
struct RawHarness {
    id: String,
    name: String,
}

#[derive(Debug, Deserialize)]
struct RawSession {
    id: String,
    harness: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    model: Option<String>,
    persona: String,
    #[serde(default)]
    memories: Vec<String>,
    /// 接入令牌。必填——没有令牌就没有身份，桥不知道该把这条连接绑到哪些会话。
    token: String,
    #[serde(default)]
    subscriptions: Vec<RawSubscription>,
    /// 一轮失败时的通知动作列表（默认空 = 只记日志）。
    #[serde(default)]
    on_turn_error: Vec<String>,
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
struct RawConnection {
    id: String,
    platform: String,
    kind: String,
    /// 没写就是 `ws-client`（正向 WS），因为这是现在唯一实现的。
    #[serde(default = "default_transport")]
    transport: String,
    url: String,
    token: Option<String>,
    account: Option<String>,
    /// 系统管理员账号列表，可为空。
    #[serde(default)]
    admin_qqs: Vec<String>,
    /// `true` 全开，或写方法名数组只开那几个。不写 = 全关。
    #[serde(default)]
    allow_destructive: Option<RawDestructive>,
}

/// `allow_destructive` 有两种写法：布尔，或方法名数组。
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawDestructive {
    Bool(bool),
    List(Vec<String>),
}

impl RawDestructive {
    fn into_policy(self) -> DestructivePolicy {
        match self {
            Self::Bool(true) => DestructivePolicy::All,
            Self::Bool(false) => DestructivePolicy::None,
            Self::List(names) => {
                let names: Vec<String> = names
                    .into_iter()
                    .map(|name| name.trim().to_owned())
                    .filter(|name| !name.is_empty())
                    .collect();
                // 写了 `["*"]` 就当全开，省得列一大串。
                if names.iter().any(|name| name == "*") {
                    DestructivePolicy::All
                } else if names.is_empty() {
                    DestructivePolicy::None
                } else {
                    DestructivePolicy::Only(names)
                }
            }
        }
    }
}

fn default_transport() -> String {
    "ws-client".to_owned()
}

#[derive(Debug, Deserialize)]
struct RawEndpoint {
    listen: String,
}

#[derive(Debug, Deserialize)]
struct RawConsole {
    listen: String,
    token: String,
}

#[derive(Debug, Deserialize)]
struct RawFileRelay {
    listen: String,
    public_base: String,
    dir: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawSubscription {
    /// 走哪条连接收发。
    connection: String,
    /// 平台侧房间路径，如 `group/123`。
    room: String,
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

    fn config_with(connections: &str, sessions: &str) -> String {
        format!("{connections}\n{sessions}")
    }

    const ONE_CONNECTION: &str = r#"
        [[connections]]
        id = "qq"
        platform = "qq"
        kind = "onebot11"
        url = "ws://127.0.0.1:3001"
        account = "10001"
    "#;

    const ONE_SESSION: &str = r#"
        [[personas]]
        id = "default"
        name = "默认人格"
        text = "你是桥。"

        [[harnesses]]
        id = "dsh"
        name = "DeepSeek"

        [[sessions]]
        id = "main"
        harness = "dsh"
        persona = "default"
        token = "t-main"

        [[sessions.subscriptions]]
        connection = "qq"
        room = "group/123"
    "#;

    #[test]
    fn minimal_config_uses_documented_defaults() {
        let config = Config::parse(&config_with(ONE_CONNECTION, ONE_SESSION)).expect("应当能解析");
        let subscription = &config.sessions[0].subscriptions[0];

        assert_eq!(subscription.room.to_string(), "qq:group/123");
        assert!(subscription.wake.at_mention);
        assert!(subscription.wake.reply);
        assert!(subscription.wake.name_mention);
        assert!(subscription.wake.poke);
        assert_eq!(subscription.wake.accumulate, Some(DEFAULT_ACCUMULATE));
        assert!(subscription.wake.keywords.is_empty());
        assert_eq!(config.connections[0].transport, Transport::WsClient);
    }

    #[test]
    fn transport_defaults_to_forward_websocket() {
        let config = Config::parse(&config_with(ONE_CONNECTION, ONE_SESSION)).expect("能解析");
        assert_eq!(config.connections[0].transport.as_str(), "ws-client");
    }

    /// 破坏性方法默认全关：AI 能做人也做的事，但那道闸默认是落下的。
    #[test]
    fn destructive_methods_are_closed_unless_the_connection_says_otherwise() {
        let config = Config::parse(&config_with(ONE_CONNECTION, ONE_SESSION)).expect("能解析");
        let policy = &config.connections[0].allow_destructive;
        assert_eq!(*policy, DestructivePolicy::None);
        assert!(!policy.allows("group.kick"));
    }

    #[test]
    fn allow_destructive_accepts_true_false_and_a_method_list() {
        let with = |extra: &str| {
            let text = ONE_CONNECTION.replace(
                "account = \"10001\"",
                &format!("account = \"10001\"\n{extra}"),
            );
            Config::parse(&config_with(&text, ONE_SESSION))
                .expect("能解析")
                .connections[0]
                .allow_destructive
                .clone()
        };

        let all = with("allow_destructive = true");
        assert_eq!(all, DestructivePolicy::All);
        assert!(all.allows("group.kick"));
        assert!(all.allows("message.recall"));

        let none = with("allow_destructive = false");
        assert!(!none.allows("group.kick"));

        let some = with("allow_destructive = [\"group.kick\"]");
        assert_eq!(some, DestructivePolicy::Only(vec!["group.kick".to_owned()]));
        assert!(some.allows("group.kick"));
        assert!(!some.allows("group.ban"), "没列出来的不许");

        // `["*"]` 是「全开」的简写。
        let star = with("allow_destructive = [\"*\"]");
        assert_eq!(star, DestructivePolicy::All);

        // 空数组等于没开，不能因为「写了这一行」就放开。
        let empty = with("allow_destructive = []");
        assert!(!empty.allows("group.kick"));
    }

    #[test]
    fn reverse_websocket_is_rejected_until_it_exists() {
        let text = ONE_CONNECTION.replace(
            r#"kind = "onebot11""#,
            "kind = \"onebot11\"\ntransport = \"ws-server\"",
        );
        assert!(matches!(
            Config::parse(&config_with(&text, ONE_SESSION)),
            Err(ConfigError::UnsupportedTransport { .. })
        ));
    }

    #[test]
    fn two_connections_on_one_platform_are_allowed() {
        let two = format!(
            "{ONE_CONNECTION}\n{}",
            ONE_CONNECTION
                .replace("id = \"qq\"", "id = \"qq2\"")
                .replace("10001", "10002")
        );
        let config =
            Config::parse(&config_with(&two, ONE_SESSION)).expect("同一平台两条连接要允许");
        assert_eq!(config.connections.len(), 2);
        assert_eq!(config.connection("qq2").expect("有第二条").account, "10002");
    }

    #[test]
    fn duplicate_connection_id_is_rejected() {
        let twice = format!("{ONE_CONNECTION}\n{ONE_CONNECTION}");
        assert!(matches!(
            Config::parse(&config_with(&twice, ONE_SESSION)),
            Err(ConfigError::DuplicateConnection { .. })
        ));
    }

    #[test]
    fn connection_needs_a_login_account() {
        let no_account = ONE_CONNECTION.replace("        account = \"10001\"", "");
        assert!(matches!(
            Config::parse(&config_with(&no_account, ONE_SESSION)),
            Err(ConfigError::EmptyAccount { .. })
        ));
    }

    #[test]
    fn subscription_to_a_missing_connection_is_rejected() {
        let session = ONE_SESSION.replace("connection = \"qq\"", "connection = \"nope\"");
        assert!(matches!(
            Config::parse(&config_with(ONE_CONNECTION, &session)),
            Err(ConfigError::UnknownConnection { .. })
        ));
    }

    #[test]
    fn same_room_twice_in_one_session_is_rejected() {
        let session = format!(
            "{ONE_SESSION}\n{}",
            r#"
        [[sessions.subscriptions]]
        connection = "qq"
        room = "group/123"
        "#
        );
        assert!(matches!(
            Config::parse(&config_with(ONE_CONNECTION, &session)),
            Err(ConfigError::DuplicateRoom { .. })
        ));
    }

    #[test]
    fn same_harness_groups_sessions() {
        let session = format!(
            "{ONE_SESSION}\n{}",
            r#"
        [[sessions]]
        id = "second"
        harness = "dsh"
        persona = "default"
        token = "t-main"
        "#
        );
        let config =
            Config::parse(&config_with(ONE_CONNECTION, &session)).expect("同 harness 要算一组");
        let group = config.sessions_by_harness("dsh");
        assert_eq!(group.len(), 2);
        assert_eq!(group[1].id, "second");
    }

    #[test]
    fn shared_room_is_a_warning_naming_the_room() {
        let session = format!(
            "{ONE_SESSION}\n{}",
            r#"
        [[sessions]]
        id = "second"
        harness = "dsh"
        persona = "default"
        token = "t-second"

        [[sessions.subscriptions]]
        connection = "qq"
        room = "group/123"
        "#
        );
        let config = Config::parse(&config_with(ONE_CONNECTION, &session)).expect("能解析");
        let warnings = config.warnings();
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("qq:group/123")),
            "应当提醒房间撞车：{warnings:?}"
        );
    }

    #[test]
    fn empty_room_path_is_rejected() {
        let session = ONE_SESSION.replace("room = \"group/123\"", "room = \"  \"");
        assert!(matches!(
            Config::parse(&config_with(ONE_CONNECTION, &session)),
            Err(ConfigError::BadRoom { .. })
        ));
    }

    #[test]
    fn zero_accumulate_is_rejected() {
        let session = ONE_SESSION.replace(
            "room = \"group/123\"",
            "room = \"group/123\"\n        [sessions.subscriptions.wake]\n        accumulate = 0",
        );
        assert!(matches!(
            Config::parse(&config_with(ONE_CONNECTION, &session)),
            Err(ConfigError::BadAccumulate { .. })
        ));
    }

    #[test]
    fn empty_token_is_rejected() {
        let session = ONE_SESSION.replace("token = \"t-main\"", "token = \" \"");
        assert!(matches!(
            Config::parse(&config_with(ONE_CONNECTION, &session)),
            Err(ConfigError::EmptyToken { .. })
        ));
    }

    #[test]
    fn unknown_persona_is_rejected() {
        let session = ONE_SESSION.replace("persona = \"default\"", "persona = \"nope\"");
        assert!(matches!(
            Config::parse(&config_with(ONE_CONNECTION, &session)),
            Err(ConfigError::UnknownPersona { .. })
        ));
    }

    #[test]
    fn unknown_memory_is_rejected() {
        let session = ONE_SESSION.replace(
            "persona = \"default\"",
            "persona = \"default\"\n        memories = [\"nope\"]",
        );
        assert!(matches!(
            Config::parse(&config_with(ONE_CONNECTION, &session)),
            Err(ConfigError::UnknownMemory { .. })
        ));
    }

    #[test]
    fn unknown_harness_is_rejected() {
        let session = ONE_SESSION.replace("harness = \"dsh\"", "harness = \"nope\"");
        assert!(matches!(
            Config::parse(&config_with(ONE_CONNECTION, &session)),
            Err(ConfigError::UnknownHarness { .. })
        ));
    }

    #[test]
    fn duplicate_harness_is_rejected() {
        let session = format!(
            "{ONE_SESSION}\n{}",
            r#"
        [[harnesses]]
        id = "dsh"
        name = "另一个"
        "#
        );
        assert!(matches!(
            Config::parse(&config_with(ONE_CONNECTION, &session)),
            Err(ConfigError::DuplicateHarness { .. })
        ));
    }

    #[test]
    fn session_name_defaults_to_id_and_model_is_optional() {
        // 不写 name / model：name 默认 = id，model 为 None。
        let config = Config::parse(&config_with(ONE_CONNECTION, ONE_SESSION)).expect("能解析");
        let session = &config.sessions[0];
        assert_eq!(session.name, "main");
        assert_eq!(session.model, None);

        // 写 name + model：都读进来。
        let with = ONE_SESSION.replace(
            "token = \"t-main\"",
            "name = \"主群助手\"\n        model = \"deepseek-chat\"\n        token = \"t-main\"",
        );
        let config = Config::parse(&config_with(ONE_CONNECTION, &with)).expect("能解析");
        let session = &config.sessions[0];
        assert_eq!(session.name, "主群助手");
        assert_eq!(session.model.as_deref(), Some("deepseek-chat"));
    }

    #[test]
    fn harness_is_a_first_class_object() {
        let config = Config::parse(&config_with(ONE_CONNECTION, ONE_SESSION)).expect("能解析");
        assert_eq!(config.harnesses.len(), 1);
        let harness = config.harness("dsh").expect("有 dsh");
        assert_eq!(harness.id, "dsh");
        assert_eq!(harness.name, "DeepSeek");
    }

    #[test]
    fn memory_kind_is_checked() {
        let session = format!(
            "{ONE_SESSION}\n{}",
            r#"
        [[memories]]
        id = "m1"
        name = "记忆"
        kind = "sqlite"
        location = "/tmp/m.db"
        "#
        );
        assert!(matches!(
            Config::parse(&config_with(ONE_CONNECTION, &session)),
            Err(ConfigError::UnknownMemoryKind { .. })
        ));
    }

    #[test]
    fn wake_defaults_are_the_five_documented_switches() {
        let policy = WakePolicy::default();
        assert!(policy.at_mention && policy.reply && policy.name_mention && policy.poke);
        assert_eq!(policy.accumulate, Some(DEFAULT_ACCUMULATE));
    }

    #[test]
    fn worktrees_root_defaults_and_can_be_overridden() {
        let config = Config::parse(&config_with(ONE_CONNECTION, ONE_SESSION)).expect("能解析");
        assert_eq!(config.worktrees_root, DEFAULT_WORKTREES_ROOT);

        // worktrees_root 是顶层字段，得放在任何 [[...]] 表格之前。
        let with_root =
            format!("worktrees_root = \"/srv/worktrees\"\n{ONE_CONNECTION}\n{ONE_SESSION}");
        let config = Config::parse(&with_root).expect("能解析");
        assert_eq!(config.worktrees_root, "/srv/worktrees");
    }

    #[test]
    fn quoting_my_message_wakes_by_default() {
        let policy = WakePolicy::default();
        let input = WakeInput {
            sender_id: "someone",
            text: "接着说",
            at_self: false,
            reply_to_self: true,
            event: None,
            unread: 1,
        };
        assert_eq!(policy.decide(&input), WakeDecision::Wake);
    }

    #[test]
    fn reply_switch_can_be_turned_off() {
        let policy = WakePolicy {
            reply: false,
            ..WakePolicy::default()
        };
        let input = WakeInput {
            sender_id: "someone",
            text: "接着说",
            at_self: false,
            reply_to_self: true,
            event: None,
            unread: 1,
        };
        assert_eq!(policy.decide(&input), WakeDecision::Stay);
    }

    #[test]
    fn keywords_can_wake() {
        let policy = WakePolicy {
            keywords: vec!["桥".into()],
            accumulate: None,
            ..WakePolicy::default()
        };
        // 关掉攒条数，避免它把「没命中关键词」也判成醒。
        let hit = WakeInput {
            sender_id: "someone",
            text: "这个桥能不能重启一下",
            at_self: false,
            reply_to_self: false,
            event: None,
            unread: 1,
        };
        let miss = WakeInput {
            text: "今天天气不错",
            ..hit
        };
        assert_eq!(policy.decide(&hit), WakeDecision::Wake);
        assert_eq!(policy.decide(&miss), WakeDecision::Stay);
    }

    #[test]
    fn accumulating_enough_messages_wakes() {
        let policy = WakePolicy {
            accumulate: Some(3),
            ..WakePolicy::default()
        };
        let input = WakeInput {
            sender_id: "someone",
            text: "闲聊",
            at_self: false,
            reply_to_self: false,
            event: None,
            unread: 2,
        };
        assert_eq!(policy.decide(&input), WakeDecision::Stay);

        let enough = WakeInput { unread: 3, ..input };
        assert_eq!(policy.decide(&enough), WakeDecision::Wake);
    }

    #[test]
    fn accumulate_off_means_no_waiting_for_a_number() {
        let policy = WakePolicy {
            accumulate: None,
            ..WakePolicy::default()
        };
        let input = WakeInput {
            sender_id: "someone",
            text: "闲聊",
            at_self: false,
            reply_to_self: false,
            event: None,
            unread: 999,
        };
        // 不看条数：攒再多也不醒，得靠 @ / 引用 / 关键词这些。
        assert_eq!(policy.decide(&input), WakeDecision::Stay);
    }

    #[test]
    fn name_mention_wakes_only_when_names_configured() {
        let without_names = WakePolicy {
            accumulate: None,
            ..WakePolicy::default()
        };
        let input = WakeInput {
            sender_id: "someone",
            text: "小桥你说说看",
            at_self: false,
            reply_to_self: false,
            event: None,
            unread: 1,
        };
        // 没配昵称就等于没法点名：默认开着也不能凭空认出一个名字。
        assert_eq!(without_names.decide(&input), WakeDecision::Stay);

        let with_names = WakePolicy {
            names: vec!["小桥".into()],
            ..without_names
        };
        assert_eq!(with_names.decide(&input), WakeDecision::Wake);
    }

    #[test]
    fn poke_wakes_only_when_it_targets_me() {
        let policy = WakePolicy::default();
        let aimed = WakeInput {
            sender_id: "someone",
            text: "",
            at_self: true,
            reply_to_self: false,
            event: Some("poke"),
            unread: 1,
        };
        let elsewhere = WakeInput {
            at_self: false,
            ..aimed
        };
        assert_eq!(policy.decide(&aimed), WakeDecision::Wake);
        // 拍别人不该把我也吵起来。
        assert_eq!(policy.decide(&elsewhere), WakeDecision::Stay);

        let deaf = WakePolicy {
            poke: false,
            ..policy
        };
        assert_eq!(deaf.decide(&aimed), WakeDecision::Stay);
    }

    #[test]
    fn plain_chatter_stays_quiet_under_the_default_policy() {
        let policy = WakePolicy {
            accumulate: None,
            ..WakePolicy::default()
        };
        let input = WakeInput {
            sender_id: "someone",
            text: "群里日常闲聊",
            at_self: false,
            reply_to_self: false,
            event: None,
            unread: 1,
        };
        // 默认策略不能变成「谁说话都回」，否则群里没法待。
        assert_eq!(policy.decide(&input), WakeDecision::Stay);
    }

    #[test]
    fn ignored_speaker_never_wakes() {
        let policy = WakePolicy {
            ignore_speakers: vec!["noisy".into()],
            ..WakePolicy::default()
        };
        let input = WakeInput {
            sender_id: "noisy",
            text: "@bot",
            at_self: true,
            reply_to_self: true,
            event: None,
            unread: 99,
        };
        assert_eq!(policy.decide(&input), WakeDecision::Stay);
    }
}
