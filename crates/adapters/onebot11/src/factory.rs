//! OneBot 11 适配器的注册入口（ADR-0027）。
//!
//! core 不需要知道 `LinkSlot` / `OneBotOutbound` / `supervise` 怎么拼；它只按
//! `kind = "onebot11"` 从 Root 取一个 `AdapterInstance`。连接线程仍然由适配器
//! 自己启动（`launch`），core 不持有 socket，也不把 `Bridge` 交出去。

use std::borrow::Cow;

use acb_kernel::{
    AdapterCapability, AdapterError, AdapterFactory, AdapterHost, AdapterInstance,
    AdapterManifest, Connection, EventQueue, ADAPTER_API_VERSION,
};

use crate::{LinkSlot, OneBotOutbound, supervise};

static TRANSPORTS: &[Cow<'static, str>] = &[Cow::Borrowed("ws-client")];
static CAPABILITIES: &[AdapterCapability] = &[
    AdapterCapability::ReceiveEvents,
    AdapterCapability::Send,
    AdapterCapability::Catalog,
];

/// 这条适配器的静态声明。`kind` 必须和 config.toml 里的 `kind` 一致。
pub static MANIFEST: AdapterManifest = AdapterManifest {
    id: Cow::Borrowed("acb.adapter.onebot11"),
    platform: Cow::Borrowed("qq"),
    kind: Cow::Borrowed("onebot11"),
    transports: Cow::Borrowed(TRANSPORTS),
    api_version: ADAPTER_API_VERSION,
    capabilities: Cow::Borrowed(CAPABILITIES),
};

/// OneBot 11 的工厂。一个连接一个 `LinkSlot`，`launch` 里起守护线程。
#[derive(Debug, Default)]
pub struct OneBotAdapterFactory;

impl AdapterFactory for OneBotAdapterFactory {
    fn manifest(&self) -> AdapterManifest {
        MANIFEST.clone()
    }

    fn create(
        &self,
        connection: &Connection,
        _events: EventQueue,
    ) -> Result<AdapterInstance, AdapterError> {
        if connection.url.is_none() {
            return Err(AdapterError::Create(
                connection.id.clone(),
                "onebot11 需要 url（ws://主机:端口/路径）".into(),
            ));
        }
        let slot = LinkSlot::new();
        let outbound = Box::new(OneBotOutbound::new(slot.clone(), connection.id.clone()));

        let connection = connection.clone();
        let launch: Box<dyn FnOnce(AdapterHost) + Send> = Box::new(move |host| {
            supervise(slot, connection, host.shutdown, host.events);
        });

        Ok(AdapterInstance { outbound, launch })
    }
}
