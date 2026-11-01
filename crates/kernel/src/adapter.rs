//! 平台适配器契约（ADR-0027）。
//!
//! 适配器在 core 外：它拥有平台连接、解码、出站和平台方法目录；core 只按连接
//! 配置的 `kind` 实例化，并通过 [`AdapterHost`] 把入站队列和退出信号交给它。
//!
//! 适配器现在登记在 [`crate::plugin::PluginManager`] 里（扩展点
//! `platform_adapter`），所以内置、动态库和外置进程三种载体走同一套
//! manifest / 能力 / 版本审计。

use std::borrow::Cow;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use thiserror::Error;

use crate::config::Connection;
use crate::event_queue::EventQueue;
use crate::outbound::Outbound;
use crate::plugin_meta::PluginMetadata;

/// 适配器接口版本。manifest 要对得上。
pub const ADAPTER_API_VERSION: u32 = 1;

/// 适配器声明的能力。core 用它判断这条连接能做什么。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AdapterCapability {
    /// 能把平台事件翻译成记录。
    ReceiveEvents,
    /// 能发送消息。
    Send,
    /// 能声明平台方法目录并调用。
    Catalog,
}

/// 一条适配器的静态声明。
///
/// 字段是 `Cow`：内置适配器用静态借用，动态库/外置进程可以构造自己的 manifest。
#[derive(Debug, Clone, PartialEq)]
pub struct AdapterManifest {
    /// 全局唯一的 provider id（`acb.adapter.onebot11`）。
    pub id: Cow<'static, str>,
    pub platform: Cow<'static, str>,
    /// 对应配置里的 `kind`（如 `onebot11`）。
    pub kind: Cow<'static, str>,
    /// 支持的 `transport` 值（如 `ws-client`、`process`）。
    pub transports: Cow<'static, [Cow<'static, str>]>,
    pub api_version: u32,
    pub capabilities: Cow<'static, [AdapterCapability]>,
    /// 给 agent / 控制台看的说明。
    pub metadata: PluginMetadata,
}

/// 适配器启动时能拿到的东西。
///
/// 故意不含 `Bridge` / `Store`：适配器只往入站队列里丢事件、只看退出信号，
/// 入库、唤醒、水位都由 core 的 drain 线程统一处理。
pub struct AdapterHost {
    pub events: EventQueue,
    pub shutdown: Arc<AtomicBool>,
}

impl std::fmt::Debug for AdapterHost {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("AdapterHost").finish_non_exhaustive()
    }
}

/// 实例化后交给 core 的两样东西：出站通道 + 启动函数。
///
/// `launch` 由适配器自己拥有连接线程；core 不替它保管 socket，也不需要知道它
/// 怎么重连、怎么启动子进程或加载动态库。
pub struct AdapterInstance {
    pub outbound: Box<dyn Outbound>,
    pub launch: Box<dyn FnOnce(AdapterHost) + Send>,
}

impl std::fmt::Debug for AdapterInstance {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AdapterInstance")
            .finish_non_exhaustive()
    }
}

/// 一个适配器工厂。core 按 `kind` 取实例，不关心载体是进程内、动态库还是外置进程。
pub trait AdapterFactory: Send + Sync {
    /// 这个工厂的静态声明。返回 owned 值，动态适配器才能按连接构造。
    fn manifest(&self) -> AdapterManifest;

    /// 实例化一条连接。`events` 是 core 的入站队列写端；适配器只往里丢，
    /// 不直接调 `Bridge::ingest`。
    fn create(
        &self,
        connection: &Connection,
        events: EventQueue,
    ) -> Result<AdapterInstance, AdapterError>;
}

/// 适配器注册/实例化错误。
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum AdapterError {
    #[error("适配器 kind 重复注册：{0}")]
    DuplicateKind(String),
    #[error("适配器 id 重复：{0}")]
    DuplicateId(String),
    #[error("没有这个适配器 kind：{0}")]
    UnknownKind(String),
    #[error("适配器 {adapter} 不支持 transport {transport}")]
    UnsupportedTransport {
        adapter: String,
        transport: String,
    },
    #[error("适配器 manifest 的 kind 是 {manifest}，但连接 {connection} 配的是 {configured}")]
    KindMismatch {
        connection: String,
        manifest: String,
        configured: String,
    },
    #[error("适配器 {0} 的接口版本 {1} 不兼容（需要 {ADAPTER_API_VERSION}）")]
    ApiVersion(String, u32),
    #[error("适配器 id 不合法：{0}（要写成 namespace.name）")]
    BadId(String),
    #[error("适配器 {0} 实例化失败：{1}")]
    Create(String, String),
}

/// manifest 本身的校验：版本必须对得上，id 必须是 `namespace.name`。
pub fn validate_manifest(manifest: &AdapterManifest) -> Result<(), AdapterError> {
    if manifest.api_version != ADAPTER_API_VERSION {
        return Err(AdapterError::ApiVersion(
            manifest.id.as_ref().to_owned(),
            manifest.api_version,
        ));
    }
    if crate::plugin::ProviderId::parse(manifest.id.as_ref()).is_err() {
        return Err(AdapterError::BadId(manifest.id.as_ref().to_owned()));
    }
    Ok(())
}

/// 一条连接在配置里的 transport 是否被 manifest 允许。
pub fn manifest_supports_transport(manifest: &AdapterManifest, transport: &str) -> bool {
    manifest
        .transports
        .iter()
        .any(|supported| supported.as_ref() == transport)
}

/// 校验 manifest 与连接配置对得上，然后实例化。
///
/// 注册时和实例化时各查一遍：连接配置可能在注册之后才出现，错误信息要指向
/// 具体那条连接，而不是笼统的「适配器坏了」。
pub fn create_instance(
    factory: &dyn AdapterFactory,
    connection: &Connection,
    events: EventQueue,
) -> Result<AdapterInstance, AdapterError> {
    let manifest = factory.manifest();
    validate_manifest(&manifest)?;

    if manifest.kind.as_ref() != connection.kind.as_str() {
        return Err(AdapterError::KindMismatch {
            connection: connection.id.clone(),
            manifest: manifest.kind.as_ref().to_owned(),
            configured: connection.kind.clone(),
        });
    }
    if !manifest_supports_transport(&manifest, connection.transport.as_str()) {
        return Err(AdapterError::UnsupportedTransport {
            adapter: manifest.id.as_ref().to_owned(),
            transport: connection.transport.as_str().to_owned(),
        });
    }

    factory.create(connection, events)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Transport;
    use crate::outbound::{OutboundError, Sent};
    use crate::plugin::PluginManager;
    use acb_proto::{Element, RoomId};
    use std::sync::Arc;

    struct FakeOutbound;

    impl Outbound for FakeOutbound {
        fn send(
            &mut self,
            _room: &RoomId,
            _elements: &[Element],
        ) -> Result<Sent, OutboundError> {
            Ok(Sent {
                platform_id: "p".into(),
                at: 0,
            })
        }
    }

    static FAKE_TRANSPORTS: &[Cow<'static, str>] = &[Cow::Borrowed("ws-client")];
    static FAKE_CAPS: &[AdapterCapability] =
        &[AdapterCapability::ReceiveEvents, AdapterCapability::Send];
    static FAKE_MANIFEST: AdapterManifest = AdapterManifest {
        id: Cow::Borrowed("acb.adapter.fake"),
        platform: Cow::Borrowed("qq"),
        kind: Cow::Borrowed("fake"),
        transports: Cow::Borrowed(FAKE_TRANSPORTS),
        api_version: ADAPTER_API_VERSION,
        capabilities: Cow::Borrowed(FAKE_CAPS),
        metadata: PluginMetadata::EMPTY,
    };

    struct FakeFactory;

    impl AdapterFactory for FakeFactory {
        fn manifest(&self) -> AdapterManifest {
            FAKE_MANIFEST.clone()
        }

        fn create(
            &self,
            _connection: &Connection,
            _events: EventQueue,
        ) -> Result<AdapterInstance, AdapterError> {
            Ok(AdapterInstance {
                outbound: Box::new(FakeOutbound),
                launch: Box::new(|_host| {}),
            })
        }
    }

    fn events() -> EventQueue {
        let (queue, _receiver) = EventQueue::channel();
        queue
    }

    fn connection(kind: &str, transport: Transport) -> Connection {
        Connection {
            id: "qq".into(),
            platform: "qq".into(),
            kind: kind.into(),
            transport,
            url: Some("ws://127.0.0.1:3001".into()),
            options: serde_json::json!({}),
            token: None,
            account: "10001".into(),
            admin_qqs: Vec::new(),
            allow_destructive: crate::config::DestructivePolicy::None,
            receive: Default::default(),
        }
    }

    #[test]
    fn root_creates_by_kind_and_validates_transport() {
        let mut plugins = PluginManager::with_native_builtins();
        plugins
            .register_adapter(Arc::new(FakeFactory))
            .expect("能注册");
        assert!(plugins
            .adapter_manifests()
            .iter()
            .any(|manifest| manifest.kind.as_ref() == "fake"));

        let instance = plugins
            .create_adapter(&connection("fake", Transport::WsClient), events())
            .expect("能实例化");
        let _ = instance.outbound;

        let bad_transport = plugins
            .create_adapter(&connection("fake", Transport::WsServer), events())
            .expect_err("transport 不支持");
        assert!(matches!(
            bad_transport,
            AdapterError::UnsupportedTransport { .. }
        ));

        let unknown = plugins
            .create_adapter(&connection("nope", Transport::WsClient), events())
            .expect_err("kind 不存在");
        assert!(matches!(unknown, AdapterError::UnknownKind(_)));
    }

    #[test]
    fn duplicate_kind_is_rejected() {
        let mut plugins = PluginManager::with_native_builtins();
        plugins
            .register_adapter(Arc::new(FakeFactory))
            .expect("第一次能注册");
        assert!(matches!(
            plugins.register_adapter(Arc::new(FakeFactory)),
            Err(AdapterError::DuplicateKind(_))
        ));
    }

    #[test]
    fn manifest_transport_helper_matches() {
        assert!(manifest_supports_transport(&FAKE_MANIFEST, "ws-client"));
        assert!(!manifest_supports_transport(&FAKE_MANIFEST, "ws-server"));
    }

    #[test]
    fn manifest_rejects_bad_ids_and_versions() {
        let bad_id = AdapterManifest {
            id: Cow::Borrowed("fake"),
            ..FAKE_MANIFEST.clone()
        };
        assert!(matches!(
            validate_manifest(&bad_id),
            Err(AdapterError::BadId(_))
        ));

        let bad_version = AdapterManifest {
            api_version: ADAPTER_API_VERSION + 1,
            ..FAKE_MANIFEST.clone()
        };
        assert!(matches!(
            validate_manifest(&bad_version),
            Err(AdapterError::ApiVersion(_, _))
        ));
    }
}
