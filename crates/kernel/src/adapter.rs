//! 平台适配器注册表（ADR-0027，Phase 2c 第一步）。
//!
//! 目标是把 `main.rs` 里「知道 onebot11 怎么 new」的知识挪回适配器 crate：
//! core 只认 `AdapterManifest` + `AdapterFactory`，按连接配置的 `kind` 取实例。
//! 适配器仍然在 core 外，拥有自己的连接线程；这里只做注册/实例化，不做事件队列
//! （队列是 Phase 2c 的下一刀）。

use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use thiserror::Error;

use crate::config::{Connection, Transport};
use crate::engine::Bridge;
use crate::event_queue::EventQueue;
use crate::outbound::Outbound;
use crate::storage::Store;

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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdapterManifest {
    pub id: &'static str,
    pub platform: &'static str,
    /// 对应配置里的 `kind`（如 `onebot11`）。
    pub kind: &'static str,
    /// 支持的 `transport` 值（如 `ws-client`）。
    pub transports: &'static [&'static str],
    pub api_version: u32,
    pub capabilities: &'static [AdapterCapability],
}

/// 实例化后交给 core 的两样东西：出站通道 + 启动函数。
///
/// `launch` 故意做成 `FnOnce(Arc<Mutex<Bridge<S>>>...)`：适配器自己拥有连接线程，
/// core 不替它保管 socket，也不需要知道它怎么重连。
pub struct AdapterInstance<S: Store> {
    pub outbound: Box<dyn Outbound>,
    pub launch: Box<dyn FnOnce(Arc<Mutex<Bridge<S>>>, Arc<AtomicBool>) + Send>,
}

impl<S: Store> std::fmt::Debug for AdapterInstance<S> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AdapterInstance")
            .finish_non_exhaustive()
    }
}

/// 一个适配器工厂。泛型在 `S` 上，这样 `main` 可以用具体的 `SqliteStore` 注册，
/// 适配器 crate 仍然能拿到完整的 `Bridge<S>`。
pub trait AdapterFactory<S: Store>: Send + Sync {
    fn manifest(&self) -> &AdapterManifest;

    /// 实例化一条连接。`events` 是 core 的入站队列写端；适配器只往里丢，
    /// 不直接调 `Bridge::ingest`。
    fn create(
        &self,
        connection: &Connection,
        events: EventQueue,
    ) -> Result<AdapterInstance<S>, AdapterError>;
}

/// 注册表错误。
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum AdapterError {
    #[error("适配器 kind 重复注册：{0}")]
    DuplicateKind(String),
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
    #[error("适配器 {0} 实例化失败：{1}")]
    Create(String, String),
}

/// 一套适配器工厂。按连接配置的 `kind` 挑实现。
pub struct AdapterRegistry<S: Store> {
    factories: HashMap<String, Arc<dyn AdapterFactory<S>>>,
}

impl<S: Store> Default for AdapterRegistry<S> {
    fn default() -> Self {
        Self {
            factories: HashMap::new(),
        }
    }
}

impl<S: Store> AdapterRegistry<S> {
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册一个适配器。`kind` 来自 manifest，不是连接配置。
    pub fn register(
        &mut self,
        factory: Arc<dyn AdapterFactory<S>>,
    ) -> Result<(), AdapterError> {
        let manifest = factory.manifest();
        if manifest.api_version != ADAPTER_API_VERSION {
            return Err(AdapterError::ApiVersion(
                manifest.id.to_owned(),
                manifest.api_version,
            ));
        }
        if self.factories.contains_key(manifest.kind) {
            return Err(AdapterError::DuplicateKind(manifest.kind.to_owned()));
        }
        self.factories
            .insert(manifest.kind.to_owned(), factory);
        Ok(())
    }

    /// 按连接配置实例化。连接里的 `kind` / `transport` 必须和 manifest 对得上。
    pub fn create(
        &self,
        connection: &Connection,
        events: EventQueue,
    ) -> Result<AdapterInstance<S>, AdapterError> {
        let factory = self
            .factories
            .get(&connection.kind)
            .ok_or_else(|| AdapterError::UnknownKind(connection.kind.clone()))?;
        let manifest = factory.manifest();

        if manifest.kind != connection.kind {
            return Err(AdapterError::KindMismatch {
                connection: connection.id.clone(),
                manifest: manifest.kind.to_owned(),
                configured: connection.kind.clone(),
            });
        }
        let transport = connection.transport.as_str();
        if !manifest.transports.contains(&transport) {
            return Err(AdapterError::UnsupportedTransport {
                adapter: manifest.id.to_owned(),
                transport: transport.to_owned(),
            });
        }

        factory.create(connection, events)
    }

    /// 已注册的 manifest（给控制台/诊断用）。
    pub fn manifests(&self) -> Vec<&AdapterManifest> {
        let mut manifests: Vec<&AdapterManifest> =
            self.factories.values().map(|factory| factory.manifest()).collect();
        manifests.sort_by_key(|manifest| manifest.kind);
        manifests
    }
}

/// 一条连接在配置里的 transport 是否被 manifest 允许。
pub fn manifest_supports_transport(manifest: &AdapterManifest, transport: Transport) -> bool {
    manifest.transports.contains(&transport.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Connection;
    use crate::outbound::{OutboundError, Sent};
    use crate::storage::MemoryStore;
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

    static FAKE_TRANSPORTS: &[&str] = &["ws-client"];
    static FAKE_CAPS: &[AdapterCapability] =
        &[AdapterCapability::ReceiveEvents, AdapterCapability::Send];
    static FAKE_MANIFEST: AdapterManifest = AdapterManifest {
        id: "fake",
        platform: "qq",
        kind: "fake",
        transports: FAKE_TRANSPORTS,
        api_version: ADAPTER_API_VERSION,
        capabilities: FAKE_CAPS,
    };

    struct FakeFactory;

    impl AdapterFactory<MemoryStore> for FakeFactory {
        fn manifest(&self) -> &AdapterManifest {
            &FAKE_MANIFEST
        }

        fn create(
            &self,
            _connection: &Connection,
            _events: EventQueue,
        ) -> Result<AdapterInstance<MemoryStore>, AdapterError> {
            Ok(AdapterInstance {
                outbound: Box::new(FakeOutbound),
                launch: Box::new(|_bridge, _shutdown| {}),
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
            url: "ws://127.0.0.1:3001".into(),
            token: None,
            account: "10001".into(),
            admin_qqs: Vec::new(),
            allow_destructive: crate::config::DestructivePolicy::None,
        }
    }

    #[test]
    fn registry_creates_by_kind_and_validates_transport() {
        let mut registry = AdapterRegistry::<MemoryStore>::new();
        registry
            .register(Arc::new(FakeFactory))
            .expect("能注册");
        assert_eq!(registry.manifests()[0].kind, "fake");

        let instance = registry
            .create(&connection("fake", Transport::WsClient), events())
            .expect("能实例化");
        let _ = instance.outbound;

        let bad_transport = registry
            .create(&connection("fake", Transport::WsServer), events())
            .expect_err("transport 不支持");
        assert!(matches!(
            bad_transport,
            AdapterError::UnsupportedTransport { .. }
        ));

        let unknown = registry
            .create(&connection("nope", Transport::WsClient), events())
            .expect_err("kind 不存在");
        assert!(matches!(unknown, AdapterError::UnknownKind(_)));
    }

    #[test]
    fn duplicate_kind_is_rejected() {
        let mut registry = AdapterRegistry::<MemoryStore>::new();
        registry.register(Arc::new(FakeFactory)).expect("第一次能注册");
        assert!(matches!(
            registry.register(Arc::new(FakeFactory)),
            Err(AdapterError::DuplicateKind(_))
        ));
    }

    #[test]
    fn manifest_transport_helper_matches() {
        assert!(manifest_supports_transport(&FAKE_MANIFEST, Transport::WsClient));
        assert!(!manifest_supports_transport(&FAKE_MANIFEST, Transport::WsServer));
    }
}
