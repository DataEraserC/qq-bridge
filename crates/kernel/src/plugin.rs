//! 扩展宿主骨架（ADR-0026）。
//!
//! Phase 1 只落地 wake 这一个扩展点：RootManager + NativeLoader + 内置 wake provider。
//! 目的是先把“provider id 解析、能力校验、native loader、manager 子树委托”跑通，
//! 行为与原来写死的 `WakePolicy::decide` 完全一致。
//!
//! 目前已落地的扩展点：wake、access、delivery composer、send hooks、turn-error。
//! 其余（receive / clock / 换投递引擎 / 动态 platform adapter）按 ADR-0026 的 phase
//! 逐个加，不在这里提前造空壳。

use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;
use std::sync::Arc;

use acb_proto::{Element, Omitted, Record, RecordKind, RoomId, Sender};
use serde_json::Value;
use thiserror::Error;

use crate::adapter::{
    AdapterError, AdapterFactory, AdapterManifest, AdapterInstance, create_instance,
    validate_manifest,
};
use crate::config::{
    AccessPolicy, ClockPolicy, Config, Connection, DeliveryPolicy, ReceivePolicy,
    TurnErrorAction, TurnErrorPolicy, WakeDecision, WakeInput, WakePolicy, WakeVerdict,
};
use crate::event_queue::EventQueue;
use crate::outbound::Sent;
use crate::plugin_meta::{PluginEffect, PluginInfo, PluginMetadata, PluginSource};

/// 插件 API 版本。provider / manager / loader 的 manifest 都要对得上。
pub const PLUGIN_API_VERSION: u32 = 1;

/// 内置 wake provider 的 id。默认订阅没有显式写 provider 时用它。
pub const DEFAULT_WAKE_PROVIDER: &str = "acb.wake.default";

/// 内置 access provider 的 id。会话没有显式写 provider 时用它。
pub const DEFAULT_ACCESS_PROVIDER: &str = "acb.access.subscribed";

/// 内置投递 composer 的 id。会话没有显式写 delivery provider 时用它。
pub const DEFAULT_DELIVERY_PROVIDER: &str = "acb.delivery.default";

/// 内置失败通知 provider 的 id。会话没有显式写 turn_error provider 时用它。
pub const DEFAULT_TURN_ERROR_PROVIDER: &str = "acb.turn_error.default";

/// 内置「不启用时钟」provider 的 id。订阅没有显式写 clock provider 时用它。
pub const DEFAULT_CLOCK_PROVIDER: &str = "acb.clock.off";

/// 内置「固定间隔 / 房间空闲」时钟 provider 的 id。
pub const CLOCK_INTERVAL_IDLE_PROVIDER: &str = "acb.clock.interval_idle";

/// 内置入站策略：全部接收。
pub const DEFAULT_RECEIVE_PROVIDER: &str = "acb.receive.all";

/// 每轮从最旧端至少带几条（默认 composer 用）。
const DELIVERY_OLDEST: usize = 5;

/// 一轮投递的字节上限：整帧不能超过 1 MiB（协议上限），这里留一半余量给信封和记忆信息。
const MAX_DELIVERY_BYTES: usize = 512 * 1024;

/// 任何房间都允许访问（整个会话豁免订阅边界）。
pub const ALL_ROOMS_ACCESS_PROVIDER: &str = "acb.access.all_rooms";

/// 订阅的房间照常；另外列在 `access.rooms` 里的房间也算可访问。
///
/// 给「一个聊天完全豁免」用：不用为它加订阅（也就不会被它叫醒），
/// 但可以照常读、发、调平台方法。
pub const ELEVATED_ACCESS_PROVIDER: &str = "acb.access.elevated_in_rooms";

/// 内置 provider 的保留 namespace；树外 manager 不能占用。
pub const BUILTIN_NAMESPACE: &str = "acb";

/// 一个 provider id：`namespace.name`。第一个点前是 namespace，后面是 name。
///
/// name 自身可以再含点（`acb.wake.default`），但 namespace 不允许含点，否则
/// “按 namespace 找 manager”会变成猜前缀。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ProviderId {
    namespace: String,
    name: String,
}

impl ProviderId {
    /// 解析并校验。空 namespace / 空 name、非法字符、没有点都拒绝。
    pub fn parse(text: &str) -> Result<Self, PluginError> {
        let text = text.trim();
        let Some((namespace, name)) = text.split_once('.') else {
            return Err(PluginError::InvalidProviderId(text.to_owned()));
        };
        if namespace.is_empty() || name.is_empty() {
            return Err(PluginError::InvalidProviderId(text.to_owned()));
        }
        let namespace_ok = namespace
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-');
        let name_ok = name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' || ch == '.');
        if !namespace_ok || !name_ok {
            return Err(PluginError::InvalidProviderId(text.to_owned()));
        }
        Ok(Self {
            namespace: namespace.to_owned(),
            name: name.to_owned(),
        })
    }

    /// 内置 wake provider。
    pub fn builtin_wake() -> Self {
        Self::parse(DEFAULT_WAKE_PROVIDER).expect("内置 provider id 必须合法")
    }

    /// 内置 access provider（默认按订阅）。
    pub fn builtin_access() -> Self {
        Self::parse(DEFAULT_ACCESS_PROVIDER).expect("内置 provider id 必须合法")
    }

    /// 内置 access provider：所有房间。
    pub fn builtin_all_rooms_access() -> Self {
        Self::parse(ALL_ROOMS_ACCESS_PROVIDER).expect("内置 provider id 必须合法")
    }

    /// 内置 access provider：指定房间提权。
    pub fn builtin_elevated_access() -> Self {
        Self::parse(ELEVATED_ACCESS_PROVIDER).expect("内置 provider id 必须合法")
    }

    /// 内置投递 composer。
    pub fn builtin_delivery() -> Self {
        Self::parse(DEFAULT_DELIVERY_PROVIDER).expect("内置 provider id 必须合法")
    }

    /// 内置失败通知 provider。
    pub fn builtin_turn_error() -> Self {
        Self::parse(DEFAULT_TURN_ERROR_PROVIDER).expect("内置 provider id 必须合法")
    }

    /// 内置「不启用时钟」provider。
    pub fn builtin_clock_off() -> Self {
        Self::parse(DEFAULT_CLOCK_PROVIDER).expect("内置 provider id 必须合法")
    }

    /// 内置「固定间隔 / 房间空闲」时钟 provider。
    pub fn builtin_clock_interval_idle() -> Self {
        Self::parse(CLOCK_INTERVAL_IDLE_PROVIDER).expect("内置 provider id 必须合法")
    }

    /// 内置入站策略：全部接收。
    pub fn builtin_receive_all() -> Self {
        Self::parse(DEFAULT_RECEIVE_PROVIDER).expect("内置 provider id 必须合法")
    }

    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}

impl fmt::Display for ProviderId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}.{}", self.namespace, self.name)
    }
}

impl FromStr for ProviderId {
    type Err = PluginError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::parse(text)
    }
}

/// 一个扩展点。Phase 1 只用到 `Wake`；其余是后续 phase 的合同名。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExtensionPoint {
    Wake,
    Access,
    Receive,
    Send,
    Clock,
    Delivery,
    TurnError,
    PlatformAdapter,
    Manager,
    Loader,
}

impl ExtensionPoint {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Wake => "wake",
            Self::Access => "access",
            Self::Receive => "receive",
            Self::Send => "send",
            Self::Clock => "clock",
            Self::Delivery => "delivery",
            Self::TurnError => "turn_error",
            Self::PlatformAdapter => "platform_adapter",
            Self::Manager => "manager",
            Self::Loader => "loader",
        }
    }
}

/// provider 被授予的能力。插件不能自己声明“我可以做更多”。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Capability {
    WakeDecision,
    AccessDecision,
    Log,
    ReadTranscript,
    EmitSend,
    PlatformCatalog,
    DeliverPlan,
    SendHook,
    TurnErrorDecision,
    ClockDecision,
    ReceiveDecision,
}

impl Capability {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::WakeDecision => "wake_decision",
            Self::AccessDecision => "access_decision",
            Self::Log => "log",
            Self::ReadTranscript => "read_transcript",
            Self::EmitSend => "emit_send",
            Self::PlatformCatalog => "platform_catalog",
            Self::DeliverPlan => "deliver_plan",
            Self::SendHook => "send_hook",
            Self::TurnErrorDecision => "turn_error_decision",
            Self::ClockDecision => "clock_decision",
            Self::ReceiveDecision => "receive_decision",
        }
    }
}

/// provider 的描述。Root 用它做全局 id/版本/能力审计，也给 agent 看用途。
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderDescriptor {
    pub id: ProviderId,
    pub extension_point: ExtensionPoint,
    pub api_version: u32,
    pub capabilities: Vec<Capability>,
    /// 平台适配器的配置 `kind`（如 `onebot11` / `process`）；其他扩展点为 None。
    pub kind: Option<String>,
    /// 插件作者声明的说明。
    pub metadata: PluginMetadata,
    /// 插件来源，由 Root 标注。
    pub source: PluginSource,
}

impl ProviderDescriptor {
    pub fn new(
        id: ProviderId,
        extension_point: ExtensionPoint,
        api_version: u32,
        capabilities: Vec<Capability>,
    ) -> Self {
        Self {
            id,
            extension_point,
            api_version,
            capabilities,
            kind: None,
            metadata: PluginMetadata::EMPTY,
            source: PluginSource::Builtin,
        }
    }

    /// 平台适配器声明自己对应的连接 `kind`。
    pub fn with_kind(mut self, kind: impl Into<String>) -> Self {
        self.kind = Some(kind.into());
        self
    }

    pub fn with_metadata(mut self, metadata: PluginMetadata) -> Self {
        self.metadata = metadata;
        self
    }

    pub fn with_source(mut self, source: PluginSource) -> Self {
        self.source = source;
        self
    }
}

/// manager 子树最多几层（Root 算第 0 层）。超过就拒绝——防止一棵树无限往下长。
pub const MAX_MANAGER_DEPTH: usize = 4;

/// 一棵 manager 子树自己的 manifest。Root 用它做 namespace、版本和能力审计。
#[derive(Debug, Clone, PartialEq)]
pub struct ManagerManifest {
    pub namespace: String,
    pub api_version: u32,
    /// 子树里所有 provider 需要的能力并集。Root 要求每个 descriptor 所需能力都在这里。
    pub capabilities: Vec<Capability>,
    /// manager 自己的说明；descriptor 缺字段时由 Root 用它补全。
    pub metadata: PluginMetadata,
}

impl ManagerManifest {
    pub fn new(
        namespace: impl Into<String>,
        api_version: u32,
        capabilities: Vec<Capability>,
    ) -> Self {
        Self {
            namespace: namespace.into(),
            api_version,
            capabilities,
            metadata: PluginMetadata::EMPTY,
        }
    }

    pub fn with_metadata(mut self, metadata: PluginMetadata) -> Self {
        self.metadata = metadata;
        self
    }
}

/// 一种载体的 manifest。Root 用它校验版本和声明的扩展点。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoaderManifest {
    pub kind: LoaderKind,
    pub api_version: u32,
    pub extensions: Vec<ExtensionPoint>,
}

impl LoaderManifest {
    pub fn new(kind: LoaderKind, api_version: u32, extensions: Vec<ExtensionPoint>) -> Self {
        Self {
            kind,
            api_version,
            extensions,
        }
    }
}

/// 热重载范围。只换指定 namespace 的子树，或者全部。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReloadScope {
    All,
    Namespace(String),
}

/// manager 自报的健康状态。Root 只汇总，不会因为 unhealthy 就停掉整个桥。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Health {
    Healthy,
    Degraded(String),
    Unhealthy(String),
}

/// 一条 health 结果：哪个 namespace、什么状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagerHealth {
    pub namespace: String,
    pub health: Health,
}

/// 插件系统错误。工具调用不在这里——那是 `acb_proto::ToolError`。
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PluginError {
    #[error("provider id 不合法：{0}（要写成 namespace.name）")]
    InvalidProviderId(String),
    #[error("没有这个 provider：{0}")]
    UnknownProvider(String),
    #[error("provider 重复注册：{0}")]
    DuplicateProvider(String),
    #[error("provider {id} 的扩展点不支持：{point:?}")]
    UnsupportedExtensionPoint {
        id: String,
        point: ExtensionPoint,
    },
    #[error("provider {id} 返回的实例是 {found:?}，但这里要 {expected:?}")]
    WrongProviderKind {
        id: String,
        expected: ExtensionPoint,
        found: ExtensionPoint,
    },
    #[error("provider {id} 的 API 版本 {found} 不兼容（需要 {required}）")]
    ApiVersion {
        id: String,
        found: u32,
        required: u32,
    },
    #[error("provider {id} 缺少能力：{capability:?}")]
    MissingCapability {
        id: String,
        capability: Capability,
    },
    #[error("namespace 重复：{0}")]
    DuplicateNamespace(String),
    #[error("namespace {0} 是内置保留名，树外 manager 不能占用")]
    ReservedNamespace(String),
    #[error("子树管理器 {namespace} 出错：{detail}")]
    Manager { namespace: String, detail: String },
    #[error("provider {id} 出错：{detail}")]
    Provider { id: String, detail: String },
    #[error("manager 子树太深：{namespace}（上限 {max} 层）")]
    ManagerDepth { namespace: String, max: usize },
    #[error("manager 环路：{0}")]
    ManagerCycle(String),
    #[error("插件加载失败：{0}")]
    Loader(String),
}

/// 传给 watcher / manager 的上下文。当前只带唤醒判定需要的东西。
///
/// `tags` 是适配器给的通用唤醒提示（未来由平台适配器填）；内置策略不读它，
/// 平台独有的策略插件可以读 `platform`、`event`、`tags`。
#[derive(Debug, Clone, Copy)]
pub struct WakeContext<'a> {
    pub session: &'a str,
    pub room: &'a RoomId,
    pub connection: &'a str,
    pub platform: &'a str,
    pub sender_id: &'a str,
    pub text: &'a str,
    pub at_self: bool,
    pub reply_to_self: bool,
    pub event: Option<&'a str>,
    pub tags: &'a [&'a str],
    pub unread: u32,
    pub now: i64,
}

/// 唤醒策略 provider。内置实现是 `DefaultWakeProvider`；它只做一层转发，
/// 把 `WakeContext` 收成原有的 `WakeInput`，保证行为不变。
pub trait WakeProvider: Send + Sync {
    fn decide(&self, options: &WakePolicy, context: &WakeContext<'_>) -> WakeDecision;

    /// 带原因的判定。默认调用 [`decide`](Self::decide)，reason 为空；
    /// 有能力的 provider 覆盖它，让 agent 知道「这次为什么醒」。
    fn decide_with_reason(
        &self,
        options: &WakePolicy,
        context: &WakeContext<'_>,
    ) -> WakeVerdict {
        WakeVerdict {
            decision: self.decide(options, context),
            reason: None,
        }
    }
}

/// 内置唤醒策略：等价于原来的 `WakePolicy::decide`。
#[derive(Debug, Default)]
pub struct DefaultWakeProvider;

impl WakeProvider for DefaultWakeProvider {
    fn decide(&self, options: &WakePolicy, context: &WakeContext<'_>) -> WakeDecision {
        self.decide_with_reason(options, context).decision
    }

    fn decide_with_reason(
        &self,
        options: &WakePolicy,
        context: &WakeContext<'_>,
    ) -> WakeVerdict {
        let input = WakeInput {
            sender_id: context.sender_id,
            text: context.text,
            at_self: context.at_self,
            reply_to_self: context.reply_to_self,
            event: context.event,
            unread: context.unread,
        };
        options.decide_with_reason(&input)
    }
}

/// 一次访问请求要做什么。
///
/// 粗粒度就够：策略只决定「能不能碰这个房间/这条连接」，具体动作（读、写、
/// 破坏性）仍由 core 和连接配置各自把关。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessAction {
    /// 读房间：历史、事件、笔记、只读平台方法。
    Read,
    /// 往房间写：发消息、写类平台方法。
    Send,
    /// 使用一条连接（账号级平台方法、平台目录）。
    Connection,
}

impl AccessAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Send => "send",
            Self::Connection => "connection",
        }
    }
}

/// 访问判定的输入。`room` 为 `None` 表示连接级请求。
#[derive(Debug, Clone, Copy)]
pub struct AccessRequest<'a> {
    pub session: &'a str,
    pub connection: &'a str,
    pub room: Option<&'a RoomId>,
    pub action: AccessAction,
    /// 这个会话自己订阅的房间。provider 只读，不写。
    pub subscriptions: &'a [RoomId],
}

/// 访问判定的结果。provider 只回答这个；拒绝时的错误文案由 core 组织。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessDecision {
    Allow,
    Deny,
}

/// 访问策略 provider。
pub trait AccessProvider: Send + Sync {
    fn check(&self, options: &AccessPolicy, request: &AccessRequest<'_>) -> AccessDecision;
}

/// 内置 access provider：只看会话订阅（原来写死的行为）。
#[derive(Debug, Default)]
pub struct SubscribedAccessProvider;

impl AccessProvider for SubscribedAccessProvider {
    fn check(&self, _options: &AccessPolicy, request: &AccessRequest<'_>) -> AccessDecision {
        match request.action {
            AccessAction::Connection => {
                if request
                    .subscriptions
                    .iter()
                    .any(|room| room.connection == request.connection)
                {
                    AccessDecision::Allow
                } else {
                    AccessDecision::Deny
                }
            }
            AccessAction::Read | AccessAction::Send => {
                if request
                    .room
                    .is_some_and(|room| request.subscriptions.iter().any(|sub| sub == room))
                {
                    AccessDecision::Allow
                } else {
                    AccessDecision::Deny
                }
            }
        }
    }
}

/// 整个会话豁免：任何房间、任何连接都允许。
#[derive(Debug, Default)]
pub struct AllRoomsAccessProvider;

impl AccessProvider for AllRoomsAccessProvider {
    fn check(&self, _options: &AccessPolicy, _request: &AccessRequest<'_>) -> AccessDecision {
        AccessDecision::Allow
    }
}

/// 订阅的房间照常；`access.rooms` 里列出的房间同样允许。
///
/// 用「房间列表」而不是「全部」，是因为豁免要精确到那一个聊天：
/// 不用为它加订阅（也就不会被它叫醒），但仍然能读、能发、能调平台方法。
#[derive(Debug, Default)]
pub struct ElevatedInRoomsAccessProvider;

impl AccessProvider for ElevatedInRoomsAccessProvider {
    fn check(&self, options: &AccessPolicy, request: &AccessRequest<'_>) -> AccessDecision {
        match request.action {
            AccessAction::Connection => {
                let subscribed = request
                    .subscriptions
                    .iter()
                    .any(|room| room.connection == request.connection);
                let elevated = options
                    .rooms
                    .iter()
                    .any(|room| room.connection == request.connection);
                if subscribed || elevated {
                    AccessDecision::Allow
                } else {
                    AccessDecision::Deny
                }
            }
            AccessAction::Read | AccessAction::Send => {
                let by_room = request
                    .room
                    .is_some_and(|room| request.subscriptions.iter().any(|sub| sub == room));
                let elevated = request
                    .room
                    .is_some_and(|room| options.rooms.iter().any(|listed| listed == room));
                if by_room || elevated {
                    AccessDecision::Allow
                } else {
                    AccessDecision::Deny
                }
            }
        }
    }
}

/// 一次开轮的候选记录。`records` 按编号从旧到新。
#[derive(Debug, Clone, Copy)]
pub struct DeliveryRequest<'a> {
    pub session: &'a str,
    pub room: &'a RoomId,
    pub records: &'a [Record],
    /// 条数上限；字节预算由 composer 自己管（默认实现有 512 KiB 上限）。
    pub limit: usize,
}

/// composer 决定本轮投哪些编号，以及中间跳过哪一段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryPlan {
    /// 要投的编号，顺序就是投递顺序。未知编号会被 core 忽略。
    pub keep: Vec<u64>,
    /// 被跳过的中段。`None` = 没跳。
    pub omitted: Option<Omitted>,
}

/// 投递批次选择 provider。它只决定「投什么」，不碰水位、票和 ack——
/// 那些仍是 Root 的职责（ADR-0026 硬约束）。
pub trait DeliveryComposer: Send + Sync {
    fn compose(&self, options: &DeliveryPolicy, request: &DeliveryRequest<'_>) -> DeliveryPlan;
}

/// 内置 composer：最旧一小段 + 最新一批，中间跳过。
#[derive(Debug, Default)]
pub struct DefaultDeliveryComposer;

impl DeliveryComposer for DefaultDeliveryComposer {
    fn compose(&self, _options: &DeliveryPolicy, request: &DeliveryRequest<'_>) -> DeliveryPlan {
        default_delivery_plan(request)
    }
}

/// 默认选批算法。composer 出错时 core 也用它兜底，保证一轮不会空手开。
pub fn default_delivery_plan(request: &DeliveryRequest<'_>) -> DeliveryPlan {
    let records = request.records;
    let len = records.len();
    if len == 0 {
        return DeliveryPlan {
            keep: Vec::new(),
            omitted: None,
        };
    }
    let limit = request.limit.max(1);

    // 条数切法：至少给最新留 1 条；`limit` 大时最旧端最多 5 条。
    let head_target = DELIVERY_OLDEST.min(limit.saturating_sub(1)).min(len);
    let tail_target = (limit - head_target).min(len - head_target);

    let sizes: Vec<usize> = records
        .iter()
        .map(|record| serde_json::to_vec(record).map(|bytes| bytes.len()).unwrap_or(0))
        .collect();

    let mut budget = MAX_DELIVERY_BYTES;
    // 最旧端：从旧往新装；第一条就算超预算也要留着（等同于「至少保一条」）。
    let mut head_keep = 0usize;
    while head_keep < head_target {
        let size = sizes[head_keep];
        if head_keep > 0 && size > budget {
            break;
        }
        budget = budget.saturating_sub(size);
        head_keep += 1;
    }

    // 最新端：从新往旧装，花剩下的预算。
    let mut tail_keep = 0usize;
    while tail_keep < tail_target {
        let index = len - 1 - tail_keep;
        if sizes[index] > budget {
            break;
        }
        budget -= sizes[index];
        tail_keep += 1;
    }
    // 一条都没留下：至少给最新 1 条，不能空手开轮。
    if head_keep + tail_keep == 0 {
        tail_keep = 1;
    }

    let mut keep = Vec::with_capacity(head_keep + tail_keep);
    keep.extend(records[..head_keep].iter().map(|record| record.id));
    keep.extend(
        records[len - tail_keep..]
            .iter()
            .map(|record| record.id),
    );
    let middle = &records[head_keep..len - tail_keep];
    let omitted = middle
        .first()
        .zip(middle.last())
        .map(|(first, last)| Omitted {
            from: first.id,
            to: last.id,
            count: middle.len(),
        });
    DeliveryPlan { keep, omitted }
}

/// 这次发送从哪儿来。hook 可以按来源区分行为。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendOrigin {
    /// `send_message` / `send_message_to` 的正文。
    Message,
    /// 一轮失败后的通知。
    TurnFailure,
}

impl SendOrigin {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Message => "message",
            Self::TurnFailure => "turn_failure",
        }
    }
}

/// 发送钩子的上下文。hook 只能看到这些，拿不到 Store / ticket / 水位。
#[derive(Debug, Clone, Copy)]
pub struct SendContext<'a> {
    pub session: &'a str,
    pub room: &'a RoomId,
    pub origin: SendOrigin,
}

/// 发送前/后钩子。`before_send` 可以改写正文；`after_send` 只观察回执。
///
/// `before_send` 出错时整次发送失败（不静默跳过 hook）；`after_send` 不影响已经发出去的消息。
pub trait SendHook: Send + Sync {
    fn before_send(
        &self,
        _context: &SendContext<'_>,
        _elements: &mut Vec<Element>,
    ) -> Result<(), PluginError> {
        Ok(())
    }

    fn after_send(&self, _context: &SendContext<'_>, _sent: &Sent) {}
}

/// 一次失败通知的输入。动作列表原样给 provider，它只负责选出要执行哪些。
#[derive(Debug, Clone, Copy)]
pub struct TurnErrorRequest<'a> {
    pub session: &'a str,
    pub room: &'a RoomId,
    pub error: &'a str,
    /// 会话自己配的动作（可能空）。
    pub actions: &'a [TurnErrorAction],
    /// 全局默认动作。
    pub default_actions: &'a [TurnErrorAction],
}

/// 失败通知 provider。默认实现保持原来的选择：会话动作非空就用它，否则用全局默认。
pub trait TurnErrorProvider: Send + Sync {
    fn decide(&self, request: &TurnErrorRequest<'_>) -> Vec<TurnErrorAction>;
}

#[derive(Debug, Default)]
pub struct DefaultTurnErrorProvider;

impl TurnErrorProvider for DefaultTurnErrorProvider {
    fn decide(&self, request: &TurnErrorRequest<'_>) -> Vec<TurnErrorAction> {
        if request.actions.is_empty() {
            request.default_actions.to_vec()
        } else {
            request.actions.to_vec()
        }
    }
}

/// 时钟判定的输入。`last_clock_at` 由 core 维护：这个（会话, 房间）上一次开时钟轮的时间。
#[derive(Debug, Clone, Copy)]
pub struct ClockContext<'a> {
    pub session: &'a str,
    pub room: &'a RoomId,
    /// 当前 Unix 毫秒。
    pub now: i64,
    /// 上一次开时钟轮的时间；第一次 tick 时初始化成当前时间。
    pub last_clock_at: i64,
    /// 房间里最后一条记录的时间；房间没记录就是 `None`。
    pub last_record_at: Option<i64>,
}

/// 时钟 provider：只回答「现在要不要开例行检查」，不碰消息和票。
pub trait ClockProvider: Send + Sync {
    fn is_due(&self, options: &ClockPolicy, context: &ClockContext<'_>) -> bool;
}

/// 默认时钟：永不触发。
#[derive(Debug, Default)]
pub struct ClockOffProvider;

impl ClockProvider for ClockOffProvider {
    fn is_due(&self, _options: &ClockPolicy, _context: &ClockContext<'_>) -> bool {
        false
    }
}

/// 固定间隔或房间空闲超时。两个阈值都可选，任一命中就开轮。
#[derive(Debug, Default)]
pub struct IntervalIdleClockProvider;

impl ClockProvider for IntervalIdleClockProvider {
    fn is_due(&self, options: &ClockPolicy, context: &ClockContext<'_>) -> bool {
        if let Some(interval_secs) = options.interval_secs {
            if interval_secs > 0 {
                let interval = (interval_secs as i64).saturating_mul(1000);
                if context.now.saturating_sub(context.last_clock_at) >= interval {
                    return true;
                }
            }
        }
        if let Some(idle_secs) = options.idle_secs {
            if idle_secs > 0 {
                if let Some(last_record_at) = context.last_record_at {
                    let idle = (idle_secs as i64).saturating_mul(1000);
                    if context.now.saturating_sub(last_record_at) >= idle {
                        return true;
                    }
                }
            }
        }
        false
    }
}

/// 一条入站记录的判定输入。provider 只能决定收不收，不能改内容或水位。
#[derive(Debug, Clone, Copy)]
pub struct ReceiveRequest<'a> {
    pub connection: &'a str,
    pub platform: &'a str,
    pub room: &'a RoomId,
    pub sender: &'a Sender,
    pub kind: &'a RecordKind,
    pub elements: &'a [Element],
    pub text: &'a str,
    pub at_self: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReceiveDecision {
    Record,
    Drop,
}

/// 入站策略 provider。默认全部接收；自定义策略可以丢机器人、丢噪声等。
pub trait ReceiveProvider: Send + Sync {
    fn decide(&self, options: &ReceivePolicy, request: &ReceiveRequest<'_>) -> ReceiveDecision;
}

#[derive(Debug, Default)]
pub struct AcceptAllReceiveProvider;

impl ReceiveProvider for AcceptAllReceiveProvider {
    fn decide(&self, _options: &ReceivePolicy, _request: &ReceiveRequest<'_>) -> ReceiveDecision {
        ReceiveDecision::Record
    }
}

/// manager 实例化 provider 时能拿到的 options。
#[derive(Debug, Clone, Copy)]
pub enum ProviderOptions<'a> {
    Wake(&'a WakePolicy),
    Access(&'a AccessPolicy),
    Delivery(&'a DeliveryPolicy),
    TurnError(&'a TurnErrorPolicy),
    Clock(&'a ClockPolicy),
    Receive(&'a ReceivePolicy),
    /// 平台适配器：按这条连接的配置实例化。
    PlatformAdapter(&'a Connection),
    /// 不需要 options 的扩展点（发送钩子）。
    None,
}

/// manager 返回的 provider 实例。
pub enum ProviderInstance {
    Wake(Arc<dyn WakeProvider>),
    Access(Arc<dyn AccessProvider>),
    Delivery(Arc<dyn DeliveryComposer>),
    Send(Arc<dyn SendHook>),
    TurnError(Arc<dyn TurnErrorProvider>),
    Clock(Arc<dyn ClockProvider>),
    Receive(Arc<dyn ReceiveProvider>),
    PlatformAdapter(Arc<dyn AdapterFactory>),
}

impl ProviderInstance {
    /// 这个实例对应哪个扩展点。Root 用它拒绝“manager 返回错类型”。
    pub fn extension_point(&self) -> ExtensionPoint {
        match self {
            Self::Wake(_) => ExtensionPoint::Wake,
            Self::Access(_) => ExtensionPoint::Access,
            Self::Delivery(_) => ExtensionPoint::Delivery,
            Self::Send(_) => ExtensionPoint::Send,
            Self::TurnError(_) => ExtensionPoint::TurnError,
            Self::Clock(_) => ExtensionPoint::Clock,
            Self::Receive(_) => ExtensionPoint::Receive,
            Self::PlatformAdapter(_) => ExtensionPoint::PlatformAdapter,
        }
    }
}

/// 一棵子树的管理者。Root 只和这个接口说话；子树内部怎么注册由它自己决定。
///
/// 除了解析 provider，manager 还要能报 manifest、被子 manager 嵌套、在轮次边界
/// 重载、退出时收尾、报告健康状态（ADR-0026 Phase 3）。
pub trait ManagerPlugin: Send + Sync {
    /// 自己的 namespace、API 版本、能力和 options schema。
    fn manifest(&self) -> ManagerManifest;

    /// 子树里的 provider 清单。Root 用它做全局 id 唯一性和能力审计。
    fn descriptors(&self) -> Vec<ProviderDescriptor>;

    fn instantiate(
        &self,
        id: &ProviderId,
        options: ProviderOptions<'_>,
    ) -> Result<ProviderInstance, PluginError>;

    /// 子树里的子 manager。Root 递归安装，并做深度上限与环路检测。
    fn children(&self) -> Vec<Arc<dyn ManagerPlugin>> {
        Vec::new()
    }

    /// 轮次边界热重载。默认什么都不做。
    fn reload(&self, _scope: &ReloadScope) -> Result<(), PluginError> {
        Ok(())
    }

    /// 进程退出前收尾。默认什么都不做。
    fn shutdown(&self) {}

    /// 健康检查。默认健康。
    fn health(&self) -> Health {
        Health::Healthy
    }
}

/// 一种载体的加载器。`NativeLoader` 是内置实现；动态/进程/Lua 后续加。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoaderKind {
    Native,
    Dynamic,
    Process,
    Script,
}

impl LoaderKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::Dynamic => "dynamic",
            Self::Process => "process",
            Self::Script => "script",
        }
    }
}

pub trait PluginLoader: Send + Sync {
    /// 载体类型、API 版本、它能带来的扩展点。
    fn manifest(&self) -> LoaderManifest;

    fn install(&self, manager: &mut PluginManager) -> Result<(), PluginError>;

    /// 卸载这个 loader 安装的 provider。默认不支持——动态 loader 再覆盖。
    fn unload(&self, _manager: &mut PluginManager, id: &ProviderId) -> Result<(), PluginError> {
        Err(PluginError::Loader(format!("这个 loader 不支持卸载 {id}")))
    }
}

fn native_meta(
    title: &'static str,
    summary: &'static str,
    effects: &[PluginEffect],
) -> PluginMetadata {
    PluginMetadata {
        title: Some(title.into()),
        summary: Some(summary.into()),
        description: None,
        tags: Vec::new().into(),
        effects: effects.to_vec().into(),
        docs_url: None,
        options_schema: None,
    }
}

/// 内置 loader。它不读文件，只把编译进来的 provider 注册到 Root。
#[derive(Debug, Default)]
pub struct NativeLoader;

impl PluginLoader for NativeLoader {
    fn manifest(&self) -> LoaderManifest {
        LoaderManifest::new(
            LoaderKind::Native,
            PLUGIN_API_VERSION,
            vec![
                ExtensionPoint::Wake,
                ExtensionPoint::Access,
                ExtensionPoint::Delivery,
                ExtensionPoint::TurnError,
                ExtensionPoint::Clock,
                ExtensionPoint::Receive,
            ],
        )
    }

    fn install(&self, manager: &mut PluginManager) -> Result<(), PluginError> {
        manager.register_wake(
            ProviderDescriptor::new(
                ProviderId::builtin_wake(),
                ExtensionPoint::Wake,
                PLUGIN_API_VERSION,
                vec![Capability::WakeDecision],
            )
            .with_metadata(native_meta(
                "默认唤醒",
                "被 @、引用、点名或关键词命中时叫醒会话",
                &[PluginEffect::MayWake, PluginEffect::MayStaySilent],
            )),
            Arc::new(DefaultWakeProvider),
        )?;
        manager.register_access(
            ProviderDescriptor::new(
                ProviderId::builtin_access(),
                ExtensionPoint::Access,
                PLUGIN_API_VERSION,
                vec![Capability::AccessDecision],
            )
            .with_metadata(native_meta(
                "只访问订阅房间",
                "允许读写当前会话已订阅的房间；拒绝其它房间",
                &[],
            )),
            Arc::new(SubscribedAccessProvider),
        )?;
        manager.register_access(
            ProviderDescriptor::new(
                ProviderId::builtin_all_rooms_access(),
                ExtensionPoint::Access,
                PLUGIN_API_VERSION,
                vec![Capability::AccessDecision],
            )
            .with_metadata(native_meta(
                "全房间访问",
                "允许访问任意房间，不受订阅列表限制（高风险）",
                &[],
            )),
            Arc::new(AllRoomsAccessProvider),
        )?;
        manager.register_access(
            ProviderDescriptor::new(
                ProviderId::builtin_elevated_access(),
                ExtensionPoint::Access,
                PLUGIN_API_VERSION,
                vec![Capability::AccessDecision],
            )
            .with_metadata(native_meta(
                "订阅房间内提升权限",
                "在已订阅房间内允许破坏性操作（踢人、禁言、改群名等）",
                &[],
            )),
            Arc::new(ElevatedInRoomsAccessProvider),
        )?;
        manager.register_delivery(
            ProviderDescriptor::new(
                ProviderId::builtin_delivery(),
                ExtensionPoint::Delivery,
                PLUGIN_API_VERSION,
                vec![Capability::DeliverPlan],
            )
            .with_metadata(native_meta(
                "默认投递",
                "按水位顺序投递未读；不改写正文，不额外延迟",
                &[],
            )),
            Arc::new(DefaultDeliveryComposer),
        )?;
        manager.register_turn_error(
            ProviderDescriptor::new(
                ProviderId::builtin_turn_error(),
                ExtensionPoint::TurnError,
                PLUGIN_API_VERSION,
                vec![Capability::TurnErrorDecision],
            )
            .with_metadata(native_meta(
                "默认轮次错误策略",
                "按配置动作处理轮次失败：记录、反馈或中止",
                &[],
            )),
            Arc::new(DefaultTurnErrorProvider),
        )?;
        manager.register_clock(
            ProviderDescriptor::new(
                ProviderId::builtin_clock_off(),
                ExtensionPoint::Clock,
                PLUGIN_API_VERSION,
                vec![Capability::ClockDecision],
            )
            .with_metadata(native_meta(
                "关闭定时唤醒",
                "不主动开新轮次，只由入站消息触发",
                &[PluginEffect::MayStaySilent],
            )),
            Arc::new(ClockOffProvider),
        )?;
        manager.register_clock(
            ProviderDescriptor::new(
                ProviderId::builtin_clock_interval_idle(),
                ExtensionPoint::Clock,
                PLUGIN_API_VERSION,
                vec![Capability::ClockDecision],
            )
            .with_metadata(native_meta(
                "空闲定时唤醒",
                "会话空闲一段时间后主动开新轮次",
                &[PluginEffect::MayWake],
            )),
            Arc::new(IntervalIdleClockProvider),
        )?;
        manager.register_receive(
            ProviderDescriptor::new(
                ProviderId::builtin_receive_all(),
                ExtensionPoint::Receive,
                PLUGIN_API_VERSION,
                vec![Capability::ReceiveDecision],
            )
            .with_metadata(native_meta(
                "接收全部入站",
                "所有入站记录都入库",
                &[],
            )),
            Arc::new(AcceptAllReceiveProvider),
        )
    }
}

/// RootManager：固定门面。负责所有扩展点的注册/解析/校验。
#[derive(Default)]
pub struct PluginManager {
    wake: HashMap<ProviderId, Arc<dyn WakeProvider>>,
    access: HashMap<ProviderId, Arc<dyn AccessProvider>>,
    delivery: HashMap<ProviderId, Arc<dyn DeliveryComposer>>,
    send_hooks: HashMap<ProviderId, Arc<dyn SendHook>>,
    turn_errors: HashMap<ProviderId, Arc<dyn TurnErrorProvider>>,
    clocks: HashMap<ProviderId, Arc<dyn ClockProvider>>,
    receives: HashMap<ProviderId, Arc<dyn ReceiveProvider>>,
    /// 直接注册的平台适配器工厂，键是连接配置里的 `kind`。
    adapters: HashMap<String, Arc<dyn AdapterFactory>>,
    /// manager 子树声明的适配器：`kind` → provider id。
    adapter_providers: HashMap<String, ProviderId>,
    managers: HashMap<String, Arc<dyn ManagerPlugin>>,
    /// 安装顺序（含嵌套子树）。reload/health 正序，shutdown 逆序。
    manager_order: Vec<String>,
    descriptors: HashMap<ProviderId, ProviderDescriptor>,
}

/// 安装前的预检结果：先攒齐整棵子树，再落表，避免装到一半失败留下半棵树。
struct CollectedManager {
    namespace: String,
    manager: Arc<dyn ManagerPlugin>,
    descriptors: Vec<ProviderDescriptor>,
}

impl PluginManager {
    /// 带内置 provider 的 Root。`Bridge::new` 用这个，保证默认行为不变。
    pub fn with_native_builtins() -> Self {
        let mut manager = Self::default();
        manager
            .install_loader(&NativeLoader)
            .expect("内置 provider 必须能注册");
        manager
    }

    /// 安装一个 loader。动态/进程/Lua loader 后续通过这个入口注册。
    ///
    /// 先校验 loader 的 manifest：API 版本要对上，声明的扩展点必须是 Root 认识的。
    pub fn install_loader(&mut self, loader: &dyn PluginLoader) -> Result<(), PluginError> {
        let manifest = loader.manifest();
        if manifest.api_version != PLUGIN_API_VERSION {
            return Err(PluginError::ApiVersion {
                id: format!("loader:{}", manifest.kind.as_str()),
                found: manifest.api_version,
                required: PLUGIN_API_VERSION,
            });
        }
        if manifest.extensions.is_empty() {
            return Err(PluginError::Loader(format!(
                "{} loader 没声明任何扩展点",
                manifest.kind.as_str()
            )));
        }
        for point in &manifest.extensions {
            if !matches!(
                point,
                ExtensionPoint::Wake
                    | ExtensionPoint::Access
                    | ExtensionPoint::Delivery
                    | ExtensionPoint::Send
                    | ExtensionPoint::TurnError
                    | ExtensionPoint::Clock
                    | ExtensionPoint::Receive
                    | ExtensionPoint::PlatformAdapter
            ) {
                return Err(PluginError::UnsupportedExtensionPoint {
                    id: format!("loader:{}", manifest.kind.as_str()),
                    point: *point,
                });
            }
        }
        loader.install(self)
    }

    /// 直接注册一个平台适配器工厂（内置适配器、native loader 用它）。
    ///
    /// 适配器也走 Root 的 manifest / 版本 / 能力审计，和 provider 一样。
    pub fn register_adapter(&mut self, factory: Arc<dyn AdapterFactory>) -> Result<(), AdapterError> {
        self.register_adapter_with_source(factory, PluginSource::Builtin)
    }

    /// 带来源注册适配器。动态库 loader 用它标注 `Dynamic`。
    pub fn register_adapter_with_source(
        &mut self,
        factory: Arc<dyn AdapterFactory>,
        source: PluginSource,
    ) -> Result<(), AdapterError> {
        let manifest = factory.manifest();
        validate_manifest(&manifest)?;
        let kind = manifest.kind.as_ref().to_owned();
        if source != PluginSource::Builtin
            && manifest
                .metadata
                .summary
                .as_deref()
                .map(str::trim)
                .unwrap_or("")
                .is_empty()
        {
            return Err(AdapterError::MissingDocs(kind));
        }
        if self.adapters.contains_key(&kind) || self.adapter_providers.contains_key(&kind) {
            return Err(AdapterError::DuplicateKind(kind));
        }

        let id = ProviderId::parse(manifest.id.as_ref())
            .map_err(|_| AdapterError::BadId(manifest.id.as_ref().to_owned()))?;
        if self.descriptors.contains_key(&id) {
            return Err(AdapterError::DuplicateId(id.to_string()));
        }
        self.descriptors.insert(
            id.clone(),
            ProviderDescriptor::new(
                id,
                ExtensionPoint::PlatformAdapter,
                PLUGIN_API_VERSION,
                vec![Capability::PlatformCatalog],
            )
            .with_kind(kind.clone())
            .with_metadata(manifest.metadata.clone())
            .with_source(source),
        );
        self.adapters.insert(kind, factory);
        Ok(())
    }

    /// Root 目录：所有 provider / 适配器的只读说明，按扩展点和 id 排序。
    ///
    /// 描述由插件作者声明，core 不靠它做行为判断；health 来自 manager 子树。
    pub fn catalog(&self) -> Vec<PluginInfo> {
        let mut infos: Vec<PluginInfo> = self
            .descriptors
            .values()
            .map(|descriptor| {
                let health = self
                    .managers
                    .get(descriptor.id.namespace())
                    .map(|manager| manager.health())
                    .unwrap_or(Health::Healthy);
                PluginInfo {
                    id: descriptor.id.clone(),
                    extension: descriptor.extension_point,
                    metadata: descriptor.metadata.clone(),
                    capabilities: descriptor.capabilities.clone(),
                    kind: descriptor.kind.clone(),
                    source: descriptor.source,
                    health,
                }
            })
            .collect();
        infos.sort_by(|left, right| {
            (left.extension.as_str(), left.id.to_string())
                .cmp(&(right.extension.as_str(), right.id.to_string()))
        });
        infos
    }

    /// 目录的 JSON 视图（控制台 / 测试用）。
    pub fn catalog_json(&self) -> Vec<Value> {
        self.catalog().iter().map(PluginInfo::to_json).collect()
    }

    /// 按 provider id 查一条目录。找不到返回 None（配置引用错误由启动校验先报）。
    pub fn info(&self, id: &ProviderId) -> Option<PluginInfo> {
        self.catalog().into_iter().find(|info| &info.id == id)
    }

    /// 按连接配置实例化适配器。先查直接注册的工厂，再按 `kind` 委托 manager 子树。
    pub fn create_adapter(
        &self,
        connection: &Connection,
        events: EventQueue,
    ) -> Result<AdapterInstance, AdapterError> {
        if let Some(factory) = self.adapters.get(&connection.kind) {
            return create_instance(factory.as_ref(), connection, events);
        }

        let Some(id) = self.adapter_providers.get(&connection.kind).cloned() else {
            return Err(AdapterError::UnknownKind(connection.kind.clone()));
        };
        let Some(manager) = self.managers.get(id.namespace()) else {
            return Err(AdapterError::Create(
                connection.id.clone(),
                format!("manager {} 没装", id.namespace()),
            ));
        };

        match manager.instantiate(&id, ProviderOptions::PlatformAdapter(connection)) {
            Ok(ProviderInstance::PlatformAdapter(factory)) => {
                create_instance(factory.as_ref(), connection, events)
            }
            Ok(other) => Err(AdapterError::Create(
                connection.id.clone(),
                format!("{} 返回的是 {:?}，不是平台适配器", id, other.extension_point()),
            )),
            Err(error) => Err(AdapterError::Create(
                connection.id.clone(),
                error.to_string(),
            )),
        }
    }

    /// 已直接注册的适配器 manifest（给控制台/诊断用）。
    pub fn adapter_manifests(&self) -> Vec<AdapterManifest> {
        let mut manifests: Vec<AdapterManifest> = self
            .adapters
            .values()
            .map(|factory| factory.manifest())
            .collect();
        manifests.sort_by(|left, right| left.kind.cmp(&right.kind));
        manifests
    }

    /// 注册 wake provider。Root 在这里校验扩展点、API 版本、能力和重复 id。
    pub fn register_wake(
        &mut self,
        descriptor: ProviderDescriptor,
        provider: Arc<dyn WakeProvider>,
    ) -> Result<(), PluginError> {
        self.check_descriptor(&descriptor, ExtensionPoint::Wake, Capability::WakeDecision)?;
        if self.descriptors.contains_key(&descriptor.id) {
            return Err(PluginError::DuplicateProvider(descriptor.id.to_string()));
        }
        self.descriptors
            .insert(descriptor.id.clone(), descriptor.clone());
        self.wake.insert(descriptor.id, provider);
        Ok(())
    }

    /// 注册 access provider。校验规则同 wake。
    pub fn register_access(
        &mut self,
        descriptor: ProviderDescriptor,
        provider: Arc<dyn AccessProvider>,
    ) -> Result<(), PluginError> {
        self.check_descriptor(
            &descriptor,
            ExtensionPoint::Access,
            Capability::AccessDecision,
        )?;
        if self.descriptors.contains_key(&descriptor.id) {
            return Err(PluginError::DuplicateProvider(descriptor.id.to_string()));
        }
        self.descriptors
            .insert(descriptor.id.clone(), descriptor.clone());
        self.access.insert(descriptor.id, provider);
        Ok(())
    }

    /// 注册投递 composer。校验规则同 wake。
    pub fn register_delivery(
        &mut self,
        descriptor: ProviderDescriptor,
        provider: Arc<dyn DeliveryComposer>,
    ) -> Result<(), PluginError> {
        self.check_descriptor(
            &descriptor,
            ExtensionPoint::Delivery,
            Capability::DeliverPlan,
        )?;
        if self.descriptors.contains_key(&descriptor.id) {
            return Err(PluginError::DuplicateProvider(descriptor.id.to_string()));
        }
        self.descriptors
            .insert(descriptor.id.clone(), descriptor.clone());
        self.delivery.insert(descriptor.id, provider);
        Ok(())
    }

    /// 注册发送钩子。校验规则同 wake。
    pub fn register_send_hook(
        &mut self,
        descriptor: ProviderDescriptor,
        hook: Arc<dyn SendHook>,
    ) -> Result<(), PluginError> {
        self.check_descriptor(&descriptor, ExtensionPoint::Send, Capability::SendHook)?;
        if self.descriptors.contains_key(&descriptor.id) {
            return Err(PluginError::DuplicateProvider(descriptor.id.to_string()));
        }
        self.descriptors
            .insert(descriptor.id.clone(), descriptor.clone());
        self.send_hooks.insert(descriptor.id, hook);
        Ok(())
    }

    /// 注册失败通知 provider。校验规则同 wake。
    pub fn register_turn_error(
        &mut self,
        descriptor: ProviderDescriptor,
        provider: Arc<dyn TurnErrorProvider>,
    ) -> Result<(), PluginError> {
        self.check_descriptor(
            &descriptor,
            ExtensionPoint::TurnError,
            Capability::TurnErrorDecision,
        )?;
        if self.descriptors.contains_key(&descriptor.id) {
            return Err(PluginError::DuplicateProvider(descriptor.id.to_string()));
        }
        self.descriptors
            .insert(descriptor.id.clone(), descriptor.clone());
        self.turn_errors.insert(descriptor.id, provider);
        Ok(())
    }

    /// 注册时钟 provider。校验规则同 wake。
    pub fn register_clock(
        &mut self,
        descriptor: ProviderDescriptor,
        provider: Arc<dyn ClockProvider>,
    ) -> Result<(), PluginError> {
        self.check_descriptor(
            &descriptor,
            ExtensionPoint::Clock,
            Capability::ClockDecision,
        )?;
        if self.descriptors.contains_key(&descriptor.id) {
            return Err(PluginError::DuplicateProvider(descriptor.id.to_string()));
        }
        self.descriptors
            .insert(descriptor.id.clone(), descriptor.clone());
        self.clocks.insert(descriptor.id, provider);
        Ok(())
    }

    /// 注册入站策略 provider。校验规则同 wake。
    pub fn register_receive(
        &mut self,
        descriptor: ProviderDescriptor,
        provider: Arc<dyn ReceiveProvider>,
    ) -> Result<(), PluginError> {
        self.check_descriptor(
            &descriptor,
            ExtensionPoint::Receive,
            Capability::ReceiveDecision,
        )?;
        if self.descriptors.contains_key(&descriptor.id) {
            return Err(PluginError::DuplicateProvider(descriptor.id.to_string()));
        }
        self.descriptors
            .insert(descriptor.id.clone(), descriptor.clone());
        self.receives.insert(descriptor.id, provider);
        Ok(())
    }

    /// 安装一棵树外 manager 子树。它拥有自己的 namespace 和子树注册表；Root 只做
    /// namespace / 版本 / 能力 / id 审计，之后把它子树里的 id 委托给它。
    ///
    /// 递归安装子 manager，并检查层级上限与环路。整棵子树先预检、再落表：
    /// 任何一层不合格都不会留下半棵装好的子树。
    pub fn install_manager(
        &mut self,
        manager: Arc<dyn ManagerPlugin>,
    ) -> Result<(), PluginError> {
        let mut collected = Vec::new();
        let mut ancestry = Vec::new();
        self.collect_manager(manager, &mut ancestry, &mut collected)?;
        for entry in collected {
            for descriptor in entry.descriptors {
                if descriptor.extension_point == ExtensionPoint::PlatformAdapter {
                    if let Some(kind) = descriptor.kind.clone() {
                        self.adapter_providers.insert(kind, descriptor.id.clone());
                    }
                }
                self.descriptors
                    .insert(descriptor.id.clone(), descriptor);
            }
            self.manager_order.push(entry.namespace.clone());
            self.managers.insert(entry.namespace, entry.manager);
        }
        Ok(())
    }

    /// 预检一棵子树：manifest、descriptor、深度、环路、重复 id/namespace。
    fn collect_manager(
        &self,
        manager: Arc<dyn ManagerPlugin>,
        ancestry: &mut Vec<String>,
        collected: &mut Vec<CollectedManager>,
    ) -> Result<(), PluginError> {
        let manifest = manager.manifest();
        let namespace = manifest.namespace.trim().to_owned();
        if namespace.is_empty() {
            return Err(PluginError::Manager {
                namespace: String::new(),
                detail: "namespace 不能为空".to_owned(),
            });
        }
        if namespace == BUILTIN_NAMESPACE {
            return Err(PluginError::ReservedNamespace(namespace));
        }
        if ancestry.iter().any(|parent| parent == &namespace) {
            let mut path = ancestry.clone();
            path.push(namespace);
            return Err(PluginError::ManagerCycle(path.join(" -> ")));
        }
        if ancestry.len() >= MAX_MANAGER_DEPTH {
            return Err(PluginError::ManagerDepth {
                namespace,
                max: MAX_MANAGER_DEPTH,
            });
        }
        let namespace_ok = namespace
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-');
        if !namespace_ok {
            return Err(PluginError::Manager {
                namespace,
                detail: "namespace 只能包含字母、数字、下划线和短横线".to_owned(),
            });
        }
        if self.managers.contains_key(&namespace)
            || collected
                .iter()
                .any(|entry| entry.namespace == namespace)
        {
            return Err(PluginError::DuplicateNamespace(namespace));
        }
        if manifest.api_version != PLUGIN_API_VERSION {
            return Err(PluginError::ApiVersion {
                id: format!("manager:{namespace}"),
                found: manifest.api_version,
                required: PLUGIN_API_VERSION,
            });
        }

        let mut descriptors = manager.descriptors();
        for descriptor in &mut descriptors {
            descriptor.metadata.merge_missing(&manifest.metadata);
        }
        for descriptor in &descriptors {
            if descriptor.source != PluginSource::Builtin
                && descriptor
                    .metadata
                    .summary
                    .as_deref()
                    .map(str::trim)
                    .unwrap_or("")
                    .is_empty()
            {
                return Err(PluginError::Manager {
                    namespace: namespace.clone(),
                    detail: format!(
                        "provider {} 没有声明 summary（agent 需要知道它是干什么的）",
                        descriptor.id
                    ),
                });
            }
            if descriptor.id.namespace() != namespace {
                return Err(PluginError::Manager {
                    namespace: namespace.clone(),
                    detail: format!(
                        "descriptor {} 的 namespace 和 manager 的 {} 不一致",
                        descriptor.id, namespace
                    ),
                });
            }
            let required = match descriptor.extension_point {
                ExtensionPoint::Wake => Capability::WakeDecision,
                ExtensionPoint::Access => Capability::AccessDecision,
                ExtensionPoint::Delivery => Capability::DeliverPlan,
                ExtensionPoint::Send => Capability::SendHook,
                ExtensionPoint::TurnError => Capability::TurnErrorDecision,
                ExtensionPoint::Clock => Capability::ClockDecision,
                ExtensionPoint::Receive => Capability::ReceiveDecision,
                ExtensionPoint::PlatformAdapter => Capability::PlatformCatalog,
                point => {
                    return Err(PluginError::UnsupportedExtensionPoint {
                        id: descriptor.id.to_string(),
                        point,
                    });
                }
            };
            if descriptor.extension_point == ExtensionPoint::PlatformAdapter {
                let kind = descriptor.kind.as_deref().map(str::trim).unwrap_or("");
                if kind.is_empty() {
                    return Err(PluginError::Manager {
                        namespace: namespace.clone(),
                        detail: format!("适配器 descriptor {} 没声明 kind", descriptor.id),
                    });
                }
                let duplicate = self.adapters.contains_key(kind)
                    || self.adapter_providers.contains_key(kind)
                    || collected.iter().any(|entry| {
                        entry.descriptors.iter().any(|existing| {
                            existing.extension_point == ExtensionPoint::PlatformAdapter
                                && existing.kind.as_deref() == Some(kind)
                        })
                    });
                if duplicate {
                    return Err(PluginError::DuplicateProvider(format!("adapter kind {kind}")));
                }
            } else if descriptor.kind.is_some() {
                return Err(PluginError::Manager {
                    namespace: namespace.clone(),
                    detail: format!("只有平台适配器能声明 kind，{} 声明了", descriptor.id),
                });
            }
            if !manifest.capabilities.contains(&required) {
                return Err(PluginError::MissingCapability {
                    id: format!("manager:{namespace}"),
                    capability: required,
                });
            }
            self.check_descriptor(descriptor, descriptor.extension_point, required)?;
            if self.descriptors.contains_key(&descriptor.id)
                || collected.iter().any(|entry| {
                    entry
                        .descriptors
                        .iter()
                        .any(|existing| existing.id == descriptor.id)
                })
            {
                return Err(PluginError::DuplicateProvider(descriptor.id.to_string()));
            }
        }

        collected.push(CollectedManager {
            namespace: namespace.clone(),
            manager,
            descriptors,
        });
        ancestry.push(namespace);
        for child in collected
            .last()
            .expect("刚刚推入")
            .manager
            .children()
        {
            self.collect_manager(child, ancestry, collected)?;
        }
        ancestry.pop();
        Ok(())
    }

    /// 轮次边界热重载。返回失败的 namespace 和原因；一个子树失败不影响其他子树。
    pub fn reload_managers(&self, scope: &ReloadScope) -> Vec<(String, PluginError)> {
        let mut failures = Vec::new();
        for namespace in &self.manager_order {
            if !matches!(scope, ReloadScope::All)
                && !matches!(scope, ReloadScope::Namespace(wanted) if wanted == namespace)
            {
                continue;
            }
            let Some(manager) = self.managers.get(namespace) else {
                continue;
            };
            if let Err(error) = manager.reload(scope) {
                failures.push((namespace.clone(), error));
            }
        }
        failures
    }

    /// 汇总所有 manager 的自报健康状态（按安装顺序）。
    pub fn manager_health(&self) -> Vec<ManagerHealth> {
        self.manager_order
            .iter()
            .filter_map(|namespace| {
                self.managers
                    .get(namespace)
                    .map(|manager| ManagerHealth {
                        namespace: namespace.clone(),
                        health: manager.health(),
                    })
            })
            .collect()
    }

    /// 进程退出前收尾。子 manager 先收，父 manager 后收。
    pub fn shutdown_managers(&self) {
        for namespace in self.manager_order.iter().rev() {
            if let Some(manager) = self.managers.get(namespace) {
                manager.shutdown();
            }
        }
    }

    /// 解析 wake provider。先查 Root 直接注册的，再按 namespace 委托 manager。
    ///
    /// 要求 manager 的 provider 也必须先报 descriptor——这是 Root 做全局 id
    /// 唯一性和能力审计的前提，不允许“manager 藏着不报”。
    pub fn resolve_wake(
        &self,
        id: &ProviderId,
        options: &WakePolicy,
    ) -> Result<Arc<dyn WakeProvider>, PluginError> {
        if let Some(provider) = self.wake.get(id) {
            return Ok(provider.clone());
        }
        let Some(descriptor) = self.descriptors.get(id) else {
            return Err(PluginError::UnknownProvider(id.to_string()));
        };
        if descriptor.extension_point != ExtensionPoint::Wake {
            return Err(PluginError::UnsupportedExtensionPoint {
                id: id.to_string(),
                point: descriptor.extension_point,
            });
        }
        let Some(manager) = self.managers.get(id.namespace()) else {
            return Err(PluginError::UnknownProvider(id.to_string()));
        };
        match manager.instantiate(id, ProviderOptions::Wake(options))? {
            ProviderInstance::Wake(provider) => Ok(provider),
            other => Err(PluginError::WrongProviderKind {
                id: id.to_string(),
                expected: ExtensionPoint::Wake,
                found: other.extension_point(),
            }),
        }
    }

    /// 解析 access provider。先查 Root 直接注册的，再按 namespace 委托 manager。
    pub fn resolve_access(
        &self,
        id: &ProviderId,
        options: &AccessPolicy,
    ) -> Result<Arc<dyn AccessProvider>, PluginError> {
        if let Some(provider) = self.access.get(id) {
            return Ok(provider.clone());
        }
        let Some(descriptor) = self.descriptors.get(id) else {
            return Err(PluginError::UnknownProvider(id.to_string()));
        };
        if descriptor.extension_point != ExtensionPoint::Access {
            return Err(PluginError::UnsupportedExtensionPoint {
                id: id.to_string(),
                point: descriptor.extension_point,
            });
        }
        let Some(manager) = self.managers.get(id.namespace()) else {
            return Err(PluginError::UnknownProvider(id.to_string()));
        };
        match manager.instantiate(id, ProviderOptions::Access(options))? {
            ProviderInstance::Access(provider) => Ok(provider),
            other => Err(PluginError::WrongProviderKind {
                id: id.to_string(),
                expected: ExtensionPoint::Access,
                found: other.extension_point(),
            }),
        }
    }

    /// 解析投递 composer。先查 Root 直接注册的，再按 namespace 委托 manager。
    pub fn resolve_delivery(
        &self,
        id: &ProviderId,
        options: &DeliveryPolicy,
    ) -> Result<Arc<dyn DeliveryComposer>, PluginError> {
        if let Some(provider) = self.delivery.get(id) {
            return Ok(provider.clone());
        }
        let Some(descriptor) = self.descriptors.get(id) else {
            return Err(PluginError::UnknownProvider(id.to_string()));
        };
        if descriptor.extension_point != ExtensionPoint::Delivery {
            return Err(PluginError::UnsupportedExtensionPoint {
                id: id.to_string(),
                point: descriptor.extension_point,
            });
        }
        let Some(manager) = self.managers.get(id.namespace()) else {
            return Err(PluginError::UnknownProvider(id.to_string()));
        };
        match manager.instantiate(id, ProviderOptions::Delivery(options))? {
            ProviderInstance::Delivery(provider) => Ok(provider),
            other => Err(PluginError::WrongProviderKind {
                id: id.to_string(),
                expected: ExtensionPoint::Delivery,
                found: other.extension_point(),
            }),
        }
    }

    /// 解析一个发送钩子。
    pub fn resolve_send_hook(&self, id: &ProviderId) -> Result<Arc<dyn SendHook>, PluginError> {
        if let Some(hook) = self.send_hooks.get(id) {
            return Ok(hook.clone());
        }
        let Some(descriptor) = self.descriptors.get(id) else {
            return Err(PluginError::UnknownProvider(id.to_string()));
        };
        if descriptor.extension_point != ExtensionPoint::Send {
            return Err(PluginError::UnsupportedExtensionPoint {
                id: id.to_string(),
                point: descriptor.extension_point,
            });
        }
        let Some(manager) = self.managers.get(id.namespace()) else {
            return Err(PluginError::UnknownProvider(id.to_string()));
        };
        match manager.instantiate(id, ProviderOptions::None)? {
            ProviderInstance::Send(hook) => Ok(hook),
            other => Err(PluginError::WrongProviderKind {
                id: id.to_string(),
                expected: ExtensionPoint::Send,
                found: other.extension_point(),
            }),
        }
    }

    /// 按配置顺序解析一组发送钩子。
    pub fn resolve_send_hooks(
        &self,
        ids: &[ProviderId],
    ) -> Result<Vec<Arc<dyn SendHook>>, PluginError> {
        ids.iter().map(|id| self.resolve_send_hook(id)).collect()
    }

    /// 解析失败通知 provider。先查 Root 直接注册的，再按 namespace 委托 manager。
    pub fn resolve_turn_error(
        &self,
        id: &ProviderId,
        options: &TurnErrorPolicy,
    ) -> Result<Arc<dyn TurnErrorProvider>, PluginError> {
        if let Some(provider) = self.turn_errors.get(id) {
            return Ok(provider.clone());
        }
        let Some(descriptor) = self.descriptors.get(id) else {
            return Err(PluginError::UnknownProvider(id.to_string()));
        };
        if descriptor.extension_point != ExtensionPoint::TurnError {
            return Err(PluginError::UnsupportedExtensionPoint {
                id: id.to_string(),
                point: descriptor.extension_point,
            });
        }
        let Some(manager) = self.managers.get(id.namespace()) else {
            return Err(PluginError::UnknownProvider(id.to_string()));
        };
        match manager.instantiate(id, ProviderOptions::TurnError(options))? {
            ProviderInstance::TurnError(provider) => Ok(provider),
            other => Err(PluginError::WrongProviderKind {
                id: id.to_string(),
                expected: ExtensionPoint::TurnError,
                found: other.extension_point(),
            }),
        }
    }

    /// 解析时钟 provider。先查 Root 直接注册的，再按 namespace 委托 manager。
    pub fn resolve_clock(
        &self,
        id: &ProviderId,
        options: &ClockPolicy,
    ) -> Result<Arc<dyn ClockProvider>, PluginError> {
        if let Some(provider) = self.clocks.get(id) {
            return Ok(provider.clone());
        }
        let Some(descriptor) = self.descriptors.get(id) else {
            return Err(PluginError::UnknownProvider(id.to_string()));
        };
        if descriptor.extension_point != ExtensionPoint::Clock {
            return Err(PluginError::UnsupportedExtensionPoint {
                id: id.to_string(),
                point: descriptor.extension_point,
            });
        }
        let Some(manager) = self.managers.get(id.namespace()) else {
            return Err(PluginError::UnknownProvider(id.to_string()));
        };
        match manager.instantiate(id, ProviderOptions::Clock(options))? {
            ProviderInstance::Clock(provider) => Ok(provider),
            other => Err(PluginError::WrongProviderKind {
                id: id.to_string(),
                expected: ExtensionPoint::Clock,
                found: other.extension_point(),
            }),
        }
    }

    /// 解析入站策略 provider。先查 Root 直接注册的，再按 namespace 委托 manager。
    pub fn resolve_receive(
        &self,
        id: &ProviderId,
        options: &ReceivePolicy,
    ) -> Result<Arc<dyn ReceiveProvider>, PluginError> {
        if let Some(provider) = self.receives.get(id) {
            return Ok(provider.clone());
        }
        let Some(descriptor) = self.descriptors.get(id) else {
            return Err(PluginError::UnknownProvider(id.to_string()));
        };
        if descriptor.extension_point != ExtensionPoint::Receive {
            return Err(PluginError::UnsupportedExtensionPoint {
                id: id.to_string(),
                point: descriptor.extension_point,
            });
        }
        let Some(manager) = self.managers.get(id.namespace()) else {
            return Err(PluginError::UnknownProvider(id.to_string()));
        };
        match manager.instantiate(id, ProviderOptions::Receive(options))? {
            ProviderInstance::Receive(provider) => Ok(provider),
            other => Err(PluginError::WrongProviderKind {
                id: id.to_string(),
                expected: ExtensionPoint::Receive,
                found: other.extension_point(),
            }),
        }
    }

    /// 启动时校验整份配置引用的 provider 都存在。`main` 在起线程前调用。
    pub fn validate_config(&self, config: &Config) -> Result<(), PluginError> {
        for connection in &config.connections {
            self.resolve_receive(&connection.receive.provider, &connection.receive)?;
        }
        for session in &config.sessions {
            self.resolve_access(&session.access.provider, &session.access)?;
            self.resolve_delivery(&session.delivery.provider, &session.delivery)?;
            self.resolve_send_hooks(&session.outbound.before)?;
            self.resolve_send_hooks(&session.outbound.after)?;
            self.resolve_turn_error(&session.turn_error.provider, &session.turn_error)?;
            for subscription in &session.subscriptions {
                self.resolve_wake(&subscription.wake.provider, &subscription.wake)?;
                self.resolve_clock(&subscription.clock.provider, &subscription.clock)?;
            }
        }
        Ok(())
    }

    /// 当前 registry 里的 provider 描述（给控制台/测试审计用）。
    pub fn descriptors(&self) -> Vec<ProviderDescriptor> {
        self.descriptors.values().cloned().collect()
    }

    fn check_descriptor(
        &self,
        descriptor: &ProviderDescriptor,
        expected: ExtensionPoint,
        required: Capability,
    ) -> Result<(), PluginError> {
        if descriptor.extension_point != expected {
            return Err(PluginError::UnsupportedExtensionPoint {
                id: descriptor.id.to_string(),
                point: descriptor.extension_point,
            });
        }
        if descriptor.api_version != PLUGIN_API_VERSION {
            return Err(PluginError::ApiVersion {
                id: descriptor.id.to_string(),
                found: descriptor.api_version,
                required: PLUGIN_API_VERSION,
            });
        }
        if !descriptor.capabilities.contains(&required) {
            return Err(PluginError::MissingCapability {
                id: descriptor.id.to_string(),
                capability: required,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AccessPolicy, Config, WakePolicy};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn test_room() -> RoomId {
        RoomId::new("qq", "group/1").expect("合法")
    }

    fn context(room: &RoomId) -> WakeContext<'_> {
        WakeContext {
            session: "main",
            room,
            connection: "qq",
            platform: "qq",
            sender_id: "10001",
            text: "在吗",
            at_self: true,
            reply_to_self: false,
            event: None,
            tags: &[],
            unread: 1,
            now: 0,
        }
    }

    #[test]
    fn provider_id_parses_and_rejects_garbage() {
        let id = ProviderId::parse("acb.wake.default").expect("合法");
        assert_eq!(id.namespace(), "acb");
        assert_eq!(id.name(), "wake.default");
        assert_eq!(id.to_string(), "acb.wake.default");

        for bad in ["", "acb", ".x", "x.", "x y.z", "x/y.z"] {
            assert!(
                ProviderId::parse(bad).is_err(),
                "应当拒绝不合法的 provider id：{bad}"
            );
        }
    }

    #[test]
    fn native_loader_registers_the_default_wake_provider() {
        let manager = PluginManager::with_native_builtins();
        let provider = manager
            .resolve_wake(&ProviderId::builtin_wake(), &WakePolicy::default())
            .expect("内置 provider 应当可解析");
        let room = test_room();
        let ctx = context(&room);
        assert_eq!(provider.decide(&WakePolicy::default(), &ctx), WakeDecision::Wake);

        assert!(manager.descriptors().iter().any(|descriptor| {
            descriptor.id == ProviderId::builtin_wake()
                && descriptor.extension_point == ExtensionPoint::Wake
        }));
    }

    #[test]
    fn native_loader_registers_the_access_providers() {
        let manager = PluginManager::with_native_builtins();
        for id in [
            ProviderId::builtin_access(),
            ProviderId::builtin_all_rooms_access(),
            ProviderId::builtin_elevated_access(),
        ] {
            manager
                .resolve_access(&id, &AccessPolicy::default())
                .expect("内置 access provider 应当可解析");
        }
        assert_eq!(
            manager
                .descriptors()
                .iter()
                .filter(|descriptor| descriptor.extension_point == ExtensionPoint::Access)
                .count(),
            3
        );
    }

    #[test]
    fn access_providers_follow_the_configured_boundary() {
        let subscribed = test_room();
        let inside = RoomId::new("qq", "group/999").expect("合法");
        let other = RoomId::new("qq", "group/888").expect("合法");
        let subscriptions = vec![subscribed.clone()];
        fn request<'a>(
            room: &'a RoomId,
            action: AccessAction,
            subscriptions: &'a [RoomId],
        ) -> AccessRequest<'a> {
            AccessRequest {
                session: "main",
                connection: "qq",
                room: Some(room),
                action,
                subscriptions,
            }
        }
        let default = AccessPolicy::default();

        // 默认：只认订阅。
        assert_eq!(
            SubscribedAccessProvider.check(
                &default,
                &request(&subscribed, AccessAction::Send, &subscriptions)
            ),
            AccessDecision::Allow
        );
        assert_eq!(
            SubscribedAccessProvider.check(
                &default,
                &request(&inside, AccessAction::Read, &subscriptions)
            ),
            AccessDecision::Deny
        );

        // 全豁免：任何房间都行。
        assert_eq!(
            AllRoomsAccessProvider.check(
                &default,
                &request(&inside, AccessAction::Send, &subscriptions)
            ),
            AccessDecision::Allow
        );

        // 指定房间：列表里的行，列表外不行。
        let elevated_policy = AccessPolicy {
            provider: ProviderId::builtin_elevated_access(),
            rooms: vec![inside.clone()],
        };
        assert_eq!(
            ElevatedInRoomsAccessProvider.check(
                &elevated_policy,
                &request(&inside, AccessAction::Read, &subscriptions)
            ),
            AccessDecision::Allow
        );
        assert_eq!(
            ElevatedInRoomsAccessProvider.check(
                &elevated_policy,
                &request(&other, AccessAction::Send, &subscriptions)
            ),
            AccessDecision::Deny
        );
    }

    #[test]
    fn connection_access_follows_the_same_policy() {
        let subscribed = test_room();
        let subscriptions = vec![subscribed];
        let request = AccessRequest {
            session: "main",
            connection: "dc",
            room: None,
            action: AccessAction::Connection,
            subscriptions: &subscriptions,
        };
        let default = AccessPolicy::default();
        assert_eq!(
            SubscribedAccessProvider.check(&default, &request),
            AccessDecision::Deny,
            "订阅里没有 dc 这条连接"
        );
        assert_eq!(
            AllRoomsAccessProvider.check(&default, &request),
            AccessDecision::Allow
        );

        let elevated_policy = AccessPolicy {
            provider: ProviderId::builtin_elevated_access(),
            rooms: vec![RoomId::new("dc", "channel/9").expect("合法")],
        };
        assert_eq!(
            ElevatedInRoomsAccessProvider.check(&elevated_policy, &request),
            AccessDecision::Allow,
            "豁免房间在 dc 上，连接级也该放行"
        );
    }

    #[test]
    fn default_wake_provider_keeps_the_documented_behavior() {
        let room = test_room();
        let ctx = context(&room);
        let policy = WakePolicy::default();
        assert_eq!(DefaultWakeProvider.decide(&policy, &ctx), WakeDecision::Wake);

        // 没有 @、没有昵称、没有累积、不是引用：不醒。
        let quiet = WakeContext {
            at_self: false,
            text: "随便说说",
            unread: 1,
            ..ctx
        };
        assert_eq!(DefaultWakeProvider.decide(&policy, &quiet), WakeDecision::Stay);
    }

    struct AlwaysWake;

    impl WakeProvider for AlwaysWake {
        fn decide(&self, _options: &WakePolicy, _context: &WakeContext<'_>) -> WakeDecision {
            WakeDecision::Wake
        }
    }

    struct TestManager;

    impl ManagerPlugin for TestManager {
        fn manifest(&self) -> ManagerManifest {
            ManagerManifest::new("test", PLUGIN_API_VERSION, vec![Capability::WakeDecision])
        }

        fn descriptors(&self) -> Vec<ProviderDescriptor> {
            vec![ProviderDescriptor::new(
                ProviderId::parse("test.always").expect("合法"),
                ExtensionPoint::Wake,
                PLUGIN_API_VERSION,
                vec![Capability::WakeDecision],
            )]
        }

        fn instantiate(
            &self,
            id: &ProviderId,
            _options: ProviderOptions<'_>,
        ) -> Result<ProviderInstance, PluginError> {
            if id.name() == "always" {
                Ok(ProviderInstance::Wake(Arc::new(AlwaysWake)))
            } else {
                Err(PluginError::UnknownProvider(id.to_string()))
            }
        }
    }

    #[test]
    fn manager_namespace_is_delegated() {
        let mut manager = PluginManager::with_native_builtins();
        manager
            .install_manager(Arc::new(TestManager))
            .expect("树外 manager 应当能安装");

        let id = ProviderId::parse("test.always").expect("合法");
        let provider = manager
            .resolve_wake(&id, &WakePolicy::default())
            .expect("manager 子树里的 provider 应当可解析");
        let room = test_room();
        let ctx = context(&room);
        assert_eq!(provider.decide(&WakePolicy::default(), &ctx), WakeDecision::Wake);

        let missing = ProviderId::parse("test.missing").expect("合法");
        assert!(matches!(
            manager.resolve_wake(&missing, &WakePolicy::default()),
            Err(PluginError::UnknownProvider(_))
        ));
    }

    struct AccessManager;

    impl ManagerPlugin for AccessManager {
        fn manifest(&self) -> ManagerManifest {
            ManagerManifest::new("vendor", PLUGIN_API_VERSION, vec![Capability::AccessDecision])
        }

        fn descriptors(&self) -> Vec<ProviderDescriptor> {
            vec![ProviderDescriptor::new(
                ProviderId::parse("vendor.all_rooms").expect("合法"),
                ExtensionPoint::Access,
                PLUGIN_API_VERSION,
                vec![Capability::AccessDecision],
            )]
        }

        fn instantiate(
            &self,
            id: &ProviderId,
            _options: ProviderOptions<'_>,
        ) -> Result<ProviderInstance, PluginError> {
            if id.name() == "all_rooms" {
                Ok(ProviderInstance::Access(Arc::new(
                    AllRoomsAccessProvider,
                )))
            } else {
                Err(PluginError::UnknownProvider(id.to_string()))
            }
        }
    }

    #[test]
    fn manager_can_supply_access_providers() {
        let mut manager = PluginManager::with_native_builtins();
        manager
            .install_manager(Arc::new(AccessManager))
            .expect("树外 manager 应当能装 access provider");

        let id = ProviderId::parse("vendor.all_rooms").expect("合法");
        manager
            .resolve_access(&id, &AccessPolicy::default())
            .expect("子树里的 access provider 应当可解析");
    }

    struct WrongKindManager;

    impl ManagerPlugin for WrongKindManager {
        fn manifest(&self) -> ManagerManifest {
            ManagerManifest::new("wrong", PLUGIN_API_VERSION, vec![Capability::AccessDecision])
        }

        fn descriptors(&self) -> Vec<ProviderDescriptor> {
            vec![ProviderDescriptor::new(
                ProviderId::parse("wrong.access").expect("合法"),
                ExtensionPoint::Access,
                PLUGIN_API_VERSION,
                vec![Capability::AccessDecision],
            )]
        }

        fn instantiate(
            &self,
            _id: &ProviderId,
            _options: ProviderOptions<'_>,
        ) -> Result<ProviderInstance, PluginError> {
            Ok(ProviderInstance::Wake(Arc::new(DefaultWakeProvider)))
        }
    }

    #[test]
    fn manager_returning_the_wrong_provider_kind_is_rejected() {
        let mut manager = PluginManager::with_native_builtins();
        manager
            .install_manager(Arc::new(WrongKindManager))
            .expect("descriptor 本身合法");
        let id = ProviderId::parse("wrong.access").expect("合法");
        assert!(matches!(
            manager.resolve_access(&id, &AccessPolicy::default()),
            Err(PluginError::WrongProviderKind {
                expected: ExtensionPoint::Access,
                found: ExtensionPoint::Wake,
                ..
            })
        ));
    }

    #[test]
    fn tree_out_manager_cannot_take_the_builtin_namespace() {
        struct AcbManager;
        impl ManagerPlugin for AcbManager {
            fn manifest(&self) -> ManagerManifest {
                ManagerManifest::new("acb", PLUGIN_API_VERSION, Vec::new())
            }
            fn descriptors(&self) -> Vec<ProviderDescriptor> {
                Vec::new()
            }
            fn instantiate(
                &self,
                id: &ProviderId,
                _options: ProviderOptions<'_>,
            ) -> Result<ProviderInstance, PluginError> {
                Err(PluginError::UnknownProvider(id.to_string()))
            }
        }

        let mut manager = PluginManager::with_native_builtins();
        assert!(matches!(
            manager.install_manager(Arc::new(AcbManager)),
            Err(PluginError::ReservedNamespace(_))
        ));
    }

    #[test]
    fn duplicate_provider_id_is_rejected() {
        let mut manager = PluginManager::with_native_builtins();
        let descriptor = ProviderDescriptor::new(
            ProviderId::builtin_wake(),
            ExtensionPoint::Wake,
            PLUGIN_API_VERSION,
            vec![Capability::WakeDecision],
        );
        assert!(matches!(
            manager.register_wake(descriptor, Arc::new(DefaultWakeProvider)),
            Err(PluginError::DuplicateProvider(_))
        ));
    }

    /// 树外 manager 的嵌套子树：父 manager 自己不提供 provider，子 manager 提供。
    struct NestedManager {
        namespace: String,
        provide: bool,
        child: Option<Arc<dyn ManagerPlugin>>,
    }

    fn nested(
        namespace: &str,
        provide: bool,
        child: Option<Arc<dyn ManagerPlugin>>,
    ) -> Arc<dyn ManagerPlugin> {
        Arc::new(NestedManager {
            namespace: namespace.to_owned(),
            provide,
            child,
        })
    }

    impl ManagerPlugin for NestedManager {
        fn manifest(&self) -> ManagerManifest {
            ManagerManifest::new(
                self.namespace.clone(),
                PLUGIN_API_VERSION,
                vec![Capability::WakeDecision],
            )
        }

        fn descriptors(&self) -> Vec<ProviderDescriptor> {
            if !self.provide {
                return Vec::new();
            }
            vec![ProviderDescriptor::new(
                ProviderId::parse(&format!("{}.always", self.namespace)).expect("合法"),
                ExtensionPoint::Wake,
                PLUGIN_API_VERSION,
                vec![Capability::WakeDecision],
            )]
        }

        fn instantiate(
            &self,
            id: &ProviderId,
            _options: ProviderOptions<'_>,
        ) -> Result<ProviderInstance, PluginError> {
            if self.provide && id.name() == "always" {
                Ok(ProviderInstance::Wake(Arc::new(AlwaysWake)))
            } else {
                Err(PluginError::UnknownProvider(id.to_string()))
            }
        }

        fn children(&self) -> Vec<Arc<dyn ManagerPlugin>> {
            self.child.clone().into_iter().collect()
        }
    }

    fn chain(levels: usize) -> Arc<dyn ManagerPlugin> {
        let mut current = None;
        for level in (1..=levels).rev() {
            current = Some(nested(&format!("level{level}"), true, current));
        }
        current.expect("至少一层")
    }

    #[test]
    fn nested_manager_subtree_is_installed_and_resolved() {
        let mut manager = PluginManager::with_native_builtins();
        manager
            .install_manager(nested(
                "parent",
                false,
                Some(nested("child", true, None)),
            ))
            .expect("嵌套子树应当能安装");

        let id = ProviderId::parse("child.always").expect("合法");
        manager
            .resolve_wake(&id, &WakePolicy::default())
            .expect("子 manager 的 provider 应当可解析");

        let health = manager.manager_health();
        assert_eq!(
            health
                .iter()
                .map(|entry| entry.namespace.as_str())
                .collect::<Vec<_>>(),
            vec!["parent", "child"],
            "安装顺序应该先父后子"
        );
    }

    #[test]
    fn manager_depth_is_limited() {
        let mut manager = PluginManager::with_native_builtins();
        manager
            .install_manager(chain(MAX_MANAGER_DEPTH))
            .expect("上限内应当能装");

        let mut manager = PluginManager::with_native_builtins();
        assert!(matches!(
            manager.install_manager(chain(MAX_MANAGER_DEPTH + 1)),
            Err(PluginError::ManagerDepth { max, .. }) if max == MAX_MANAGER_DEPTH
        ));
    }

    struct SelfCycle;

    impl ManagerPlugin for SelfCycle {
        fn manifest(&self) -> ManagerManifest {
            ManagerManifest::new("loop", PLUGIN_API_VERSION, vec![Capability::WakeDecision])
        }

        fn descriptors(&self) -> Vec<ProviderDescriptor> {
            Vec::new()
        }

        fn instantiate(
            &self,
            id: &ProviderId,
            _options: ProviderOptions<'_>,
        ) -> Result<ProviderInstance, PluginError> {
            Err(PluginError::UnknownProvider(id.to_string()))
        }

        fn children(&self) -> Vec<Arc<dyn ManagerPlugin>> {
            vec![Arc::new(SelfCycle)]
        }
    }

    #[test]
    fn manager_cycle_is_rejected() {
        let mut manager = PluginManager::with_native_builtins();
        assert!(matches!(
            manager.install_manager(Arc::new(SelfCycle)),
            Err(PluginError::ManagerCycle(path)) if path == "loop -> loop"
        ));
    }

    #[test]
    fn manager_manifest_is_checked() {
        struct BadNamespace;
        impl ManagerPlugin for BadNamespace {
            fn manifest(&self) -> ManagerManifest {
                ManagerManifest::new("bad", PLUGIN_API_VERSION, vec![Capability::WakeDecision])
            }
            fn descriptors(&self) -> Vec<ProviderDescriptor> {
                vec![ProviderDescriptor::new(
                    ProviderId::parse("other.always").expect("合法"),
                    ExtensionPoint::Wake,
                    PLUGIN_API_VERSION,
                    vec![Capability::WakeDecision],
                )]
            }
            fn instantiate(
                &self,
                id: &ProviderId,
                _options: ProviderOptions<'_>,
            ) -> Result<ProviderInstance, PluginError> {
                Err(PluginError::UnknownProvider(id.to_string()))
            }
        }
        let mut manager = PluginManager::with_native_builtins();
        assert!(matches!(
            manager.install_manager(Arc::new(BadNamespace)),
            Err(PluginError::Manager { .. })
        ));

        struct BadVersion;
        impl ManagerPlugin for BadVersion {
            fn manifest(&self) -> ManagerManifest {
                ManagerManifest::new(
                    "badver",
                    PLUGIN_API_VERSION + 1,
                    vec![Capability::WakeDecision],
                )
            }
            fn descriptors(&self) -> Vec<ProviderDescriptor> {
                Vec::new()
            }
            fn instantiate(
                &self,
                id: &ProviderId,
                _options: ProviderOptions<'_>,
            ) -> Result<ProviderInstance, PluginError> {
                Err(PluginError::UnknownProvider(id.to_string()))
            }
        }
        let mut manager = PluginManager::with_native_builtins();
        assert!(matches!(
            manager.install_manager(Arc::new(BadVersion)),
            Err(PluginError::ApiVersion { .. })
        ));

        struct MissingCapability;
        impl ManagerPlugin for MissingCapability {
            fn manifest(&self) -> ManagerManifest {
                ManagerManifest::new("nocap", PLUGIN_API_VERSION, Vec::new())
            }
            fn descriptors(&self) -> Vec<ProviderDescriptor> {
                vec![ProviderDescriptor::new(
                    ProviderId::parse("nocap.always").expect("合法"),
                    ExtensionPoint::Wake,
                    PLUGIN_API_VERSION,
                    vec![Capability::WakeDecision],
                )]
            }
            fn instantiate(
                &self,
                id: &ProviderId,
                _options: ProviderOptions<'_>,
            ) -> Result<ProviderInstance, PluginError> {
                Err(PluginError::UnknownProvider(id.to_string()))
            }
        }
        let mut manager = PluginManager::with_native_builtins();
        assert!(matches!(
            manager.install_manager(Arc::new(MissingCapability)),
            Err(PluginError::MissingCapability {
                capability: Capability::WakeDecision,
                ..
            })
        ));
    }

    #[derive(Default)]
    struct LifecycleManager {
        reloads: AtomicUsize,
        shutdowns: AtomicUsize,
    }

    impl ManagerPlugin for LifecycleManager {
        fn manifest(&self) -> ManagerManifest {
            ManagerManifest::new("life", PLUGIN_API_VERSION, vec![Capability::WakeDecision])
        }

        fn descriptors(&self) -> Vec<ProviderDescriptor> {
            Vec::new()
        }

        fn instantiate(
            &self,
            id: &ProviderId,
            _options: ProviderOptions<'_>,
        ) -> Result<ProviderInstance, PluginError> {
            Err(PluginError::UnknownProvider(id.to_string()))
        }

        fn reload(&self, _scope: &ReloadScope) -> Result<(), PluginError> {
            self.reloads.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        fn shutdown(&self) {
            self.shutdowns.fetch_add(1, Ordering::SeqCst);
        }

        fn health(&self) -> Health {
            Health::Degraded("测试状态".to_owned())
        }
    }

    #[test]
    fn manager_lifecycle_hooks_are_driven_by_root() {
        let lifecycle = Arc::new(LifecycleManager::default());
        let mut manager = PluginManager::with_native_builtins();
        manager
            .install_manager(lifecycle.clone())
            .expect("应当能安装");

        assert!(manager.reload_managers(&ReloadScope::All).is_empty());
        assert_eq!(lifecycle.reloads.load(Ordering::SeqCst), 1);

        // 范围不匹配的 namespace 不该被打扰。
        assert!(
            manager
                .reload_managers(&ReloadScope::Namespace("other".to_owned()))
                .is_empty()
        );
        assert_eq!(lifecycle.reloads.load(Ordering::SeqCst), 1);

        assert!(
            manager
                .reload_managers(&ReloadScope::Namespace("life".to_owned()))
                .is_empty()
        );
        assert_eq!(lifecycle.reloads.load(Ordering::SeqCst), 2);

        assert_eq!(
            manager.manager_health(),
            vec![ManagerHealth {
                namespace: "life".to_owned(),
                health: Health::Degraded("测试状态".to_owned()),
            }]
        );

        manager.shutdown_managers();
        assert_eq!(lifecycle.shutdowns.load(Ordering::SeqCst), 1);
    }

    struct BadVersionLoader;

    impl PluginLoader for BadVersionLoader {
        fn manifest(&self) -> LoaderManifest {
            LoaderManifest::new(
                LoaderKind::Native,
                PLUGIN_API_VERSION + 1,
                vec![ExtensionPoint::Wake],
            )
        }
        fn install(&self, _manager: &mut PluginManager) -> Result<(), PluginError> {
            Ok(())
        }
    }

    struct UnsupportedExtensionLoader;

    impl PluginLoader for UnsupportedExtensionLoader {
        fn manifest(&self) -> LoaderManifest {
            LoaderManifest::new(
                LoaderKind::Process,
                PLUGIN_API_VERSION,
                vec![ExtensionPoint::Manager],
            )
        }
        fn install(&self, _manager: &mut PluginManager) -> Result<(), PluginError> {
            Ok(())
        }
    }

    struct EmptyLoader;

    impl PluginLoader for EmptyLoader {
        fn manifest(&self) -> LoaderManifest {
            LoaderManifest::new(LoaderKind::Script, PLUGIN_API_VERSION, Vec::new())
        }
        fn install(&self, _manager: &mut PluginManager) -> Result<(), PluginError> {
            Ok(())
        }
    }

    #[test]
    fn loader_manifest_is_checked() {
        let mut manager = PluginManager::with_native_builtins();
        assert!(matches!(
            manager.install_loader(&BadVersionLoader),
            Err(PluginError::ApiVersion { .. })
        ));
        assert!(matches!(
            manager.install_loader(&UnsupportedExtensionLoader),
            Err(PluginError::UnsupportedExtensionPoint {
                point: ExtensionPoint::Manager,
                ..
            })
        ));
        assert!(matches!(
            manager.install_loader(&EmptyLoader),
            Err(PluginError::Loader(_))
        ));

        assert_eq!(
            NativeLoader.manifest().extensions,
            vec![
                ExtensionPoint::Wake,
                ExtensionPoint::Access,
                ExtensionPoint::Delivery,
                ExtensionPoint::TurnError,
                ExtensionPoint::Clock,
                ExtensionPoint::Receive
            ]
        );
    }

    #[test]
    fn native_loader_refuses_to_unload_by_default() {
        let mut manager = PluginManager::with_native_builtins();
        assert!(matches!(
            NativeLoader.unload(&mut manager, &ProviderId::builtin_wake()),
            Err(PluginError::Loader(_))
        ));
    }

    #[test]
    fn validate_config_rejects_an_unknown_provider_before_start() {
        let text = r#"
            [[connections]]
            id = "qq"
            platform = "qq"
            kind = "onebot11"
            url = "ws://127.0.0.1:3001"
            account = "10001"

            [[personas]]
            id = "p"
            name = "P"
            text = "你是桥。"

            [[harnesses]]
            id = "dsh"
            name = "DSH"

            [[sessions]]
            id = "main"
            harness = "dsh"
            persona = "p"
            token = "t-main"

            [[sessions.subscriptions]]
            connection = "qq"
            room = "group/1"

            [sessions.subscriptions.wake]
            provider = "vendor.missing"
        "#;
        let config = Config::parse(text).expect("语法合法");
        let manager = PluginManager::with_native_builtins();
        assert!(matches!(
            manager.validate_config(&config),
            Err(PluginError::UnknownProvider(_))
        ));
    }

    #[test]
    fn validate_config_rejects_an_unknown_access_provider_before_start() {
        let text = r#"
            [[connections]]
            id = "qq"
            platform = "qq"
            kind = "onebot11"
            url = "ws://127.0.0.1:3001"
            account = "10001"

            [[personas]]
            id = "p"
            name = "P"
            text = "你是桥。"

            [[harnesses]]
            id = "dsh"
            name = "DSH"

            [[sessions]]
            id = "main"
            harness = "dsh"
            persona = "p"
            token = "t-main"

            [sessions.access]
            provider = "vendor.missing"

            [[sessions.subscriptions]]
            connection = "qq"
            room = "group/1"
        "#;
        let config = Config::parse(text).expect("语法合法");
        let manager = PluginManager::with_native_builtins();
        assert!(matches!(
            manager.validate_config(&config),
            Err(PluginError::UnknownProvider(_))
        ));
    }

    fn config_with_extra(extra: &str) -> Config {
        let text = format!(
            r#"
            [[connections]]
            id = "qq"
            platform = "qq"
            kind = "onebot11"
            url = "ws://127.0.0.1:3001"
            account = "10001"

            [[personas]]
            id = "p"
            name = "P"
            text = "你是桥。"

            [[harnesses]]
            id = "dsh"
            name = "DSH"

            [[sessions]]
            id = "main"
            harness = "dsh"
            persona = "p"
            token = "t-main"

            {extra}

            [[sessions.subscriptions]]
            connection = "qq"
            room = "group/1"
            "#
        );
        Config::parse(&text).expect("语法合法")
    }

    #[test]
    fn validate_config_rejects_an_unknown_delivery_provider_before_start() {
        let config = config_with_extra(
            "[sessions.delivery]\nprovider = \"vendor.missing\"",
        );
        let manager = PluginManager::with_native_builtins();
        assert!(matches!(
            manager.validate_config(&config),
            Err(PluginError::UnknownProvider(_))
        ));
    }

    #[test]
    fn interval_idle_clock_respects_thresholds() {
        let room = test_room();
        let policy = ClockPolicy {
            provider: ProviderId::builtin_clock_interval_idle(),
            interval_secs: Some(60),
            idle_secs: Some(120),
        };
        let context = ClockContext {
            session: "main",
            room: &room,
            now: 0,
            last_clock_at: 0,
            last_record_at: None,
        };
        assert!(!ClockOffProvider.is_due(&policy, &context));
        assert!(!IntervalIdleClockProvider.is_due(&policy, &context));

        let due_interval = ClockContext {
            now: 61_000,
            ..context
        };
        assert!(IntervalIdleClockProvider.is_due(&policy, &due_interval));

        let idle_only = ClockPolicy {
            interval_secs: None,
            idle_secs: Some(120),
            ..policy
        };
        let due_idle = ClockContext {
            now: 120_000,
            last_clock_at: 0,
            last_record_at: Some(0),
            ..context
        };
        assert!(IntervalIdleClockProvider.is_due(&idle_only, &due_idle));
        let not_idle = ClockContext {
            now: 100_000,
            ..due_idle
        };
        assert!(!IntervalIdleClockProvider.is_due(&idle_only, &not_idle));
    }

    struct NeverDueClock;

    impl ClockProvider for NeverDueClock {
        fn is_due(&self, _options: &ClockPolicy, _context: &ClockContext<'_>) -> bool {
            false
        }
    }

    struct ClockManager;

    impl ManagerPlugin for ClockManager {
        fn manifest(&self) -> ManagerManifest {
            ManagerManifest::new(
                "ticker",
                PLUGIN_API_VERSION,
                vec![Capability::ClockDecision],
            )
        }

        fn descriptors(&self) -> Vec<ProviderDescriptor> {
            vec![ProviderDescriptor::new(
                ProviderId::parse("ticker.never").expect("合法"),
                ExtensionPoint::Clock,
                PLUGIN_API_VERSION,
                vec![Capability::ClockDecision],
            )]
        }

        fn instantiate(
            &self,
            id: &ProviderId,
            _options: ProviderOptions<'_>,
        ) -> Result<ProviderInstance, PluginError> {
            if id.name() == "never" {
                Ok(ProviderInstance::Clock(Arc::new(NeverDueClock)))
            } else {
                Err(PluginError::UnknownProvider(id.to_string()))
            }
        }
    }

    #[test]
    fn manager_can_supply_clock_providers() {
        let mut manager = PluginManager::with_native_builtins();
        manager
            .install_manager(Arc::new(ClockManager))
            .expect("应当能安装");
        let id = ProviderId::parse("ticker.never").expect("合法");
        manager
            .resolve_clock(&id, &ClockPolicy::default())
            .expect("时钟 provider 应当可解析");
    }

    #[test]
    fn default_turn_error_provider_keeps_the_config_behavior() {
        let room = test_room();
        let session_actions = vec![TurnErrorAction::Send { room: room.clone() }];
        let default_actions = vec![TurnErrorAction::Log];
        let request = TurnErrorRequest {
            session: "main",
            room: &room,
            error: "boom",
            actions: &session_actions,
            default_actions: &default_actions,
        };
        assert_eq!(DefaultTurnErrorProvider.decide(&request), session_actions);

        let empty: Vec<TurnErrorAction> = Vec::new();
        let request = TurnErrorRequest {
            actions: &empty,
            ..request
        };
        assert_eq!(DefaultTurnErrorProvider.decide(&request), default_actions);
    }

    struct NoopTurnErrorProvider;

    impl TurnErrorProvider for NoopTurnErrorProvider {
        fn decide(&self, _request: &TurnErrorRequest<'_>) -> Vec<TurnErrorAction> {
            Vec::new()
        }
    }

    struct TurnErrorManager;

    impl ManagerPlugin for TurnErrorManager {
        fn manifest(&self) -> ManagerManifest {
            ManagerManifest::new(
                "vendor",
                PLUGIN_API_VERSION,
                vec![Capability::TurnErrorDecision],
            )
        }

        fn descriptors(&self) -> Vec<ProviderDescriptor> {
            vec![ProviderDescriptor::new(
                ProviderId::parse("vendor.silent").expect("合法"),
                ExtensionPoint::TurnError,
                PLUGIN_API_VERSION,
                vec![Capability::TurnErrorDecision],
            )]
        }

        fn instantiate(
            &self,
            id: &ProviderId,
            _options: ProviderOptions<'_>,
        ) -> Result<ProviderInstance, PluginError> {
            if id.name() == "silent" {
                Ok(ProviderInstance::TurnError(Arc::new(NoopTurnErrorProvider)))
            } else {
                Err(PluginError::UnknownProvider(id.to_string()))
            }
        }
    }

    #[test]
    fn manager_can_supply_turn_error_providers() {
        let mut manager = PluginManager::with_native_builtins();
        manager
            .install_manager(Arc::new(TurnErrorManager))
            .expect("应当能安装");
        let id = ProviderId::parse("vendor.silent").expect("合法");
        let provider = manager
            .resolve_turn_error(&id, &TurnErrorPolicy::default())
            .expect("失败通知 provider 应当可解析");
        let room = test_room();
        let request = TurnErrorRequest {
            session: "main",
            room: &room,
            error: "boom",
            actions: &[],
            default_actions: &[],
        };
        assert!(provider.decide(&request).is_empty());
    }

    #[test]
    fn validate_config_rejects_an_unknown_turn_error_provider_before_start() {
        let config = config_with_extra(
            "[sessions.turn_error]\nprovider = \"vendor.missing\"",
        );
        let manager = PluginManager::with_native_builtins();
        assert!(matches!(
            manager.validate_config(&config),
            Err(PluginError::UnknownProvider(_))
        ));
    }

    #[test]
    fn validate_config_rejects_an_unknown_send_hook_before_start() {
        let config = config_with_extra(
            "[sessions.outbound]\nbefore = [\"vendor.missing\"]",
        );
        let manager = PluginManager::with_native_builtins();
        assert!(matches!(
            manager.validate_config(&config),
            Err(PluginError::UnknownProvider(_))
        ));
    }
}
