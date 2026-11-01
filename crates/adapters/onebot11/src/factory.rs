//! OneBot 11 适配器的注册入口（ADR-0027）。
//!
//! core 不需要知道 `LinkSlot` / `OneBotOutbound` / `supervise` 怎么拼；它只按
//! `kind = "onebot11"` 从 Root 取一个 `AdapterInstance`。连接线程仍然由适配器
//! 自己启动（`launch`），core 不持有 socket，也不把 `Bridge` 交出去。

use std::borrow::Cow;

use acb_kernel::{
    AdapterCapability, AdapterError, AdapterFactory, AdapterHost, AdapterInstance,
    AdapterManifest, Connection, EventQueue, PluginEffect, PluginMetadata, ADAPTER_API_VERSION,
};

use crate::{LinkSlot, OneBotOutbound, supervise};

static TRANSPORTS: &[Cow<'static, str>] = &[Cow::Borrowed("ws-client")];
static CAPABILITIES: &[AdapterCapability] = &[
    AdapterCapability::ReceiveEvents,
    AdapterCapability::Send,
    AdapterCapability::Catalog,
];
/// 已知会进流水的事件名（事件里的 `event` 字段）。OneBot 的 notice_type 是开放的，
/// 没列出来的也会原样记，所以这是「常见事件」清单，不是白名单。
static EVENTS: &[Cow<'static, str>] = &[
    Cow::Borrowed("poke"),
    Cow::Borrowed("group_increase"),
    Cow::Borrowed("group_decrease"),
    Cow::Borrowed("group_ban"),
    Cow::Borrowed("group_recall"),
    Cow::Borrowed("friend_recall"),
    Cow::Borrowed("group_admin"),
    Cow::Borrowed("group_upload"),
    Cow::Borrowed("group_card"),
    Cow::Borrowed("notify"),
    Cow::Borrowed("request_friend"),
    Cow::Borrowed("request_group"),
];
static EVENT_TAGS: &[Cow<'static, str>] = &[
    Cow::Borrowed("poke"),
    Cow::Borrowed("notice"),
    Cow::Borrowed("request"),
];

/// 这条适配器的静态声明。`kind` 必须和 config.toml 里的 `kind` 一致。
pub static MANIFEST: AdapterManifest = AdapterManifest {
    id: Cow::Borrowed("acb.adapter.onebot11"),
    platform: Cow::Borrowed("qq"),
    kind: Cow::Borrowed("onebot11"),
    transports: Cow::Borrowed(TRANSPORTS),
    api_version: ADAPTER_API_VERSION,
    capabilities: Cow::Borrowed(CAPABILITIES),
    events: Cow::Borrowed(EVENTS),
    event_tags: Cow::Borrowed(EVENT_TAGS),
    supports_at: true,
    supports_reply: true,
    metadata: PluginMetadata {
        title: Some(Cow::Borrowed("OneBot 11")),
        summary: Some(Cow::Borrowed("QQ 群与私聊：收消息、发消息、平台方法目录")),
        description: None,
        tags: Cow::Borrowed(&[]),
        effects: Cow::Borrowed(&[
            PluginEffect::ExternalCall,
            PluginEffect::Nondeterministic,
        ]),
        docs_url: None,
        options_schema: None,
    },
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
