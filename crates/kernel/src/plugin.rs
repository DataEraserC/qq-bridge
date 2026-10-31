//! 扩展宿主骨架（ADR-0026）。
//!
//! Phase 1 只落地 wake 这一个扩展点：RootManager + NativeLoader + 内置 wake provider。
//! 目的是先把“provider id 解析、能力校验、native loader、manager 子树委托”跑通，
//! 行为与原来写死的 `WakePolicy::decide` 完全一致。
//!
//! 其余扩展点（receive / send / clock / delivery / turn-error / platform adapter）按
//! ADR-0026 的 phase 逐个加，不在这里提前造空壳。

use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;
use std::sync::Arc;

use acb_proto::RoomId;
use thiserror::Error;

use crate::config::{Config, WakeDecision, WakeInput, WakePolicy};

/// 插件 API 版本。provider / manager / loader 的 manifest 都要对得上。
pub const PLUGIN_API_VERSION: u32 = 1;

/// 内置 wake provider 的 id。默认订阅没有显式写 provider 时用它。
pub const DEFAULT_WAKE_PROVIDER: &str = "acb.wake.default";

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
    Log,
    ReadTranscript,
    EmitSend,
    PlatformCatalog,
    DeliverPlan,
}

impl Capability {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::WakeDecision => "wake_decision",
            Self::Log => "log",
            Self::ReadTranscript => "read_transcript",
            Self::EmitSend => "emit_send",
            Self::PlatformCatalog => "platform_catalog",
            Self::DeliverPlan => "deliver_plan",
        }
    }
}

/// provider 的描述。Root 用它做全局 id/版本/能力审计。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderDescriptor {
    pub id: ProviderId,
    pub extension_point: ExtensionPoint,
    pub api_version: u32,
    pub capabilities: Vec<Capability>,
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
        }
    }
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
}

/// 内置唤醒策略：等价于原来的 `WakePolicy::decide`。
#[derive(Debug, Default)]
pub struct DefaultWakeProvider;

impl WakeProvider for DefaultWakeProvider {
    fn decide(&self, options: &WakePolicy, context: &WakeContext<'_>) -> WakeDecision {
        let input = WakeInput {
            sender_id: context.sender_id,
            text: context.text,
            at_self: context.at_self,
            reply_to_self: context.reply_to_self,
            event: context.event,
            unread: context.unread,
        };
        options.decide(&input)
    }
}

/// manager 实例化 provider 时能拿到的 options。Phase 1 只有 wake。
#[derive(Debug, Clone, Copy)]
pub enum ProviderOptions<'a> {
    Wake(&'a WakePolicy),
}

/// manager 返回的 provider 实例。Phase 1 只有 wake 变体。
pub enum ProviderInstance {
    Wake(Arc<dyn WakeProvider>),
}

/// 一棵子树的管理者。Root 只和这个接口说话；子树内部怎么注册由它自己决定。
///
/// Phase 1 的 manager 只需要能返回 wake provider；其余扩展点在后续 phase 按
/// `ExtensionPoint` 扩展 `ProviderInstance`。
pub trait ManagerPlugin: Send + Sync {
    fn namespace(&self) -> &str;
    fn descriptors(&self) -> Vec<ProviderDescriptor>;

    fn instantiate(
        &self,
        id: &ProviderId,
        options: ProviderOptions<'_>,
    ) -> Result<ProviderInstance, PluginError>;
}

/// 一种载体的加载器。`NativeLoader` 是内置实现；动态/进程/Lua 后续加。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoaderKind {
    Native,
    Dynamic,
    Process,
    Script,
}

pub trait PluginLoader: Send + Sync {
    fn kind(&self) -> LoaderKind;
    fn install(&self, manager: &mut PluginManager) -> Result<(), PluginError>;
}

/// 内置 loader。它不读文件，只把编译进来的 provider 注册到 Root。
#[derive(Debug, Default)]
pub struct NativeLoader;

impl PluginLoader for NativeLoader {
    fn kind(&self) -> LoaderKind {
        LoaderKind::Native
    }

    fn install(&self, manager: &mut PluginManager) -> Result<(), PluginError> {
        manager.register_wake(
            ProviderDescriptor::new(
                ProviderId::builtin_wake(),
                ExtensionPoint::Wake,
                PLUGIN_API_VERSION,
                vec![Capability::WakeDecision],
            ),
            Arc::new(DefaultWakeProvider),
        )
    }
}

/// RootManager：固定门面。Phase 1 只做 wake 的注册/解析/校验。
#[derive(Default)]
pub struct PluginManager {
    wake: HashMap<ProviderId, Arc<dyn WakeProvider>>,
    managers: HashMap<String, Arc<dyn ManagerPlugin>>,
    descriptors: HashMap<ProviderId, ProviderDescriptor>,
}

impl PluginManager {
    /// 带内置 provider 的 Root。`Bridge::new` 用这个，保证默认行为不变。
    pub fn with_native_builtins() -> Self {
        let mut manager = Self::default();
        NativeLoader
            .install(&mut manager)
            .expect("内置 provider 必须能注册");
        manager
    }

    /// 安装一个 loader。动态/进程/Lua loader 后续通过这个入口注册。
    pub fn install_loader(&mut self, loader: &dyn PluginLoader) -> Result<(), PluginError> {
        loader.install(self)
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

    /// 安装一个树外 manager。它拥有自己的 namespace 和子树注册表；Root 只做
    /// namespace/id 审计，之后把它子树里的 id 委托给它。
    pub fn install_manager(
        &mut self,
        manager: Arc<dyn ManagerPlugin>,
    ) -> Result<(), PluginError> {
        let namespace = manager.namespace().trim();
        if namespace.is_empty() {
            return Err(PluginError::Manager {
                namespace: String::new(),
                detail: "namespace 不能为空".to_owned(),
            });
        }
        if namespace == BUILTIN_NAMESPACE {
            return Err(PluginError::ReservedNamespace(namespace.to_owned()));
        }
        if self.managers.contains_key(namespace) {
            return Err(PluginError::DuplicateNamespace(namespace.to_owned()));
        }

        let descriptors = manager.descriptors();
        for descriptor in &descriptors {
            if descriptor.id.namespace() != namespace {
                return Err(PluginError::Manager {
                    namespace: namespace.to_owned(),
                    detail: format!(
                        "descriptor {} 的 namespace 和 manager 的 {} 不一致",
                        descriptor.id, namespace
                    ),
                });
            }
            self.check_descriptor(descriptor, ExtensionPoint::Wake, Capability::WakeDecision)?;
            if self.descriptors.contains_key(&descriptor.id) {
                return Err(PluginError::DuplicateProvider(descriptor.id.to_string()));
            }
        }

        for descriptor in descriptors {
            self.descriptors
                .insert(descriptor.id.clone(), descriptor);
        }
        self.managers.insert(namespace.to_owned(), manager);
        Ok(())
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
        }
    }

    /// 启动时校验整份配置引用的 provider 都存在。`main` 在起线程前调用。
    pub fn validate_config(&self, config: &Config) -> Result<(), PluginError> {
        for session in &config.sessions {
            for subscription in &session.subscriptions {
                self.resolve_wake(&subscription.wake.provider, &subscription.wake)?;
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
    use crate::config::{Config, WakePolicy};

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
        fn namespace(&self) -> &str {
            "test"
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

    #[test]
    fn tree_out_manager_cannot_take_the_builtin_namespace() {
        struct AcbManager;
        impl ManagerPlugin for AcbManager {
            fn namespace(&self) -> &str {
                "acb"
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
}
