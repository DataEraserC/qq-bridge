//! OneBot 11 适配器的注册入口（ADR-0027）。
//!
//! core 不需要知道 `LinkSlot` / `OneBotOutbound` / `supervise` 怎么拼；它只按
//! `kind = "onebot11"` 从注册表拿一个 `AdapterInstance`。连接线程仍然由适配器
//! 自己启动（`launch`），core 不持有 socket。

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use acb_kernel::{
    AdapterCapability, AdapterError, AdapterFactory, AdapterInstance, AdapterManifest,
    Connection, Store, ADAPTER_API_VERSION, Bridge,
};

use crate::{LinkSlot, OneBotOutbound, supervise};

const TRANSPORTS: &[&str] = &["ws-client"];
const CAPABILITIES: &[AdapterCapability] = &[
    AdapterCapability::ReceiveEvents,
    AdapterCapability::Send,
    AdapterCapability::Catalog,
];

/// 这条适配器的静态声明。`kind` 必须和 config.toml 里的 `kind` 一致。
pub static MANIFEST: AdapterManifest = AdapterManifest {
    id: "acb.adapter.onebot11",
    platform: "qq",
    kind: "onebot11",
    transports: TRANSPORTS,
    api_version: ADAPTER_API_VERSION,
    capabilities: CAPABILITIES,
};

/// OneBot 11 的工厂。一个连接一个 `LinkSlot`，`launch` 里起守护线程。
#[derive(Debug, Default)]
pub struct OneBotAdapterFactory;

impl<S: Store + Send + 'static> AdapterFactory<S> for OneBotAdapterFactory {
    fn manifest(&self) -> &AdapterManifest {
        &MANIFEST
    }

    fn create(
        &self,
        connection: &Connection,
        events: acb_kernel::EventQueue,
    ) -> Result<AdapterInstance<S>, AdapterError> {
        let slot = LinkSlot::new();
        let outbound = Box::new(OneBotOutbound::new(slot.clone(), connection.id.clone()));

        let url = connection.url.clone();
        let token = connection.token.clone();
        let id = connection.id.clone();
        let admin_qqs = connection.admin_qqs.clone();
        let launch: Box<dyn FnOnce(Arc<Mutex<Bridge<S>>>, Arc<AtomicBool>) + Send> =
            Box::new(move |bridge, shutdown| {
                supervise(
                    slot, url, token, id, admin_qqs, bridge, shutdown, events,
                );
            });

        Ok(AdapterInstance { outbound, launch })
    }
}
