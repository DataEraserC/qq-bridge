//! 动态库平台适配器（ADR-0027 Phase 3 P3）。
//!
//! `.so` / `.dylib` 只导出一个符号 `acb_adapter_entry_v1`，返回一张
//! [`AdapterV1`] 函数表。跨 ABI 的所有文本都是 JSON，形状与外置进程共用
//! `acb-adapter-api` 的 wire：
//!
//! - `manifest()` → `WireManifest`；
//! - `create(WireConnection JSON, HostV1)` → 不透明 handle；
//! - `invoke(handle, CoreMessage JSON)` → `AdapterMessage` JSON；
//! - 插件通过 `HostV1.submit_event` 把入站记录交给 core。
//!
//! 配置：
//!
//! ```toml
//! [[adapters]]
//! path = "/path/to/libdemo_adapter.so"
//! kind = "demo"   # 可选；manifest 的 kind 必须等于它
//! ```

use std::ffi::{CStr, CString, c_char, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use acb_adapter_api::abi::{ABI_VERSION, AdapterV1, ENTRY_SYMBOL, EntryFn, HostV1};
use acb_adapter_api::{
    AdapterMessage, CoreMessage, WireAccessRequest, WireClockRequest, WireConnection,
    WireDeliveryRequest, WireInstanceConfig, WireManifest, WireReceiveRequest, WireResult,
    WireSendRequest, WireTurnErrorAction, WireTurnErrorRequest, WireWakeRequest, WireError,
    decode_line, encode_line,
};
use acb_kernel::wire::{call_error, convert_catalog, send_error};
use acb_kernel::{
    AccessDecision, AccessPolicy, AccessProvider, AccessRequest, AdapterError, AdapterFactory,
    AdapterHost, AdapterInstance, AdapterManifest, CallError, CallOutcome, Capability,
    ClockContext, ClockPolicy, ClockProvider, Connection, DefaultTurnErrorProvider,
    DeliveryComposer, DeliveryPlan, DeliveryPolicy, DeliveryRequest, Element, EventQueue,
    ExtensionPoint, Health, LoaderKind, LoaderManifest, ManagerManifest, ManagerPlugin, MethodSpec,
    Outbound, OutboundError, OutgoingRecord, PluginConfig, PluginError, PluginLoader, PluginManager,
    PluginMetadata, PluginSource, ProviderDescriptor, ProviderId, ProviderInstance,
    ProviderOptions, ReceiveDecision, ReceivePolicy, ReceiveProvider, ReceiveRequest, RoomId,
    SendContext, SendHook, Sent, TurnErrorAction, TurnErrorPolicy, TurnErrorProvider,
    TurnErrorRequest, WakeContext, WakeDecision, WakePolicy, WakeProvider, WakeVerdict,
    PLUGIN_API_VERSION, default_delivery_plan, validate_manifest,
};
use libloading::Library;
use serde_json::{Value, json};
use std::sync::atomic::AtomicU64;

/// 动态库的一次发送/调用最多等这么久。超时只保证 core 不被卡死；插件线程本身
/// 无法安全打断，超时后会把该 handle 视为不可用（后续调用继续超时，重启桥即可）。
pub const CALL_TIMEOUT: Duration = Duration::from_secs(2);

/// 动态库 loader：把配置里声明的 `.so` 装进 Root。
#[derive(Debug, Default)]
pub struct DynamicLoader {
    plugins: Vec<PluginConfig>,
}

impl DynamicLoader {
    pub fn new(plugins: &[PluginConfig]) -> Self {
        Self {
            plugins: plugins.to_vec(),
        }
    }
}

impl PluginLoader for DynamicLoader {
    fn manifest(&self) -> LoaderManifest {
        LoaderManifest::new(
            LoaderKind::Dynamic,
            PLUGIN_API_VERSION,
            vec![
                ExtensionPoint::PlatformAdapter,
                ExtensionPoint::Wake,
                ExtensionPoint::Access,
                ExtensionPoint::Delivery,
                ExtensionPoint::Send,
                ExtensionPoint::TurnError,
                ExtensionPoint::Receive,
                ExtensionPoint::Clock,
            ],
        )
    }

    fn install(&self, manager: &mut PluginManager) -> Result<(), PluginError> {
        for plugin in &self.plugins {
            let loaded = LoadedPlugin::load(&plugin.path, plugin.kind.as_deref())
                .map_err(|detail| {
                    PluginError::Loader(format!("加载 {} 失败：{detail}", plugin.path))
                })?;
            match loaded {
                LoadedPlugin::Adapter(factory) => manager
                    .register_adapter_with_source(Arc::new(factory), PluginSource::Dynamic)
                    .map_err(|error| PluginError::Loader(error.to_string()))?,
                LoadedPlugin::Wake(wake) => manager
                    .install_manager(Arc::new(wake))
                    .map_err(|error| PluginError::Loader(error.to_string()))?,
                LoadedPlugin::Decision(decision) => manager
                    .install_manager(Arc::new(decision))
                    .map_err(|error| PluginError::Loader(error.to_string()))?,
            }
        }
        Ok(())
    }
}

enum LoadedPlugin {
    Adapter(DylibAdapterFactory),
    Wake(DylibWakeManager),
    Decision(DylibDecisionManager),
}

impl LoadedPlugin {
    fn load(path: &str, expected_kind: Option<&str>) -> Result<Self, String> {
        let library = unsafe { Library::new(path) }
            .map_err(|error| format!("打不开动态库：{error}"))?;
        let entry: libloading::Symbol<'_, EntryFn> =
            unsafe { library.get(ENTRY_SYMBOL) }.map_err(|error| {
                format!("没有导出 {}：{error}", String::from_utf8_lossy(ENTRY_SYMBOL))
            })?;
        let table_ptr = unsafe { entry() };
        if table_ptr.is_null() {
            return Err("acb_adapter_entry_v1 返回了空表".into());
        }
        let table = unsafe { *table_ptr };
        if table.abi_version != ABI_VERSION {
            return Err(format!(
                "ABI 版本 {} 不兼容（需要 {ABI_VERSION}）",
                table.abi_version
            ));
        }
        if table.manifest.is_none()
            || table.create.is_none()
            || table.invoke.is_none()
            || table.destroy.is_none()
            || table.free_string.is_none()
        {
            return Err("插件函数表不完整（manifest/create/invoke/destroy/free_string 都要有）".into());
        }

        let wire = read_manifest(&table)?;
        match wire.extension.as_str() {
            "platform_adapter" => {
                let manifest = AdapterManifest::try_from(wire)
                    .map_err(|detail| format!("manifest 不合法：{detail}"))?;
                validate_manifest(&manifest).map_err(|error| error.to_string())?;
                if let Some(expected) = expected_kind {
                    if manifest.kind.as_ref() != expected {
                        return Err(format!(
                            "manifest 的 kind 是 {}，配置里写的是 {expected}",
                            manifest.kind
                        ));
                    }
                }
                Ok(Self::Adapter(DylibAdapterFactory {
                    _library: Arc::new(library),
                    table,
                    manifest,
                }))
            }
            "wake" => {
                let provider = ProviderId::parse(&wire.id)
                    .map_err(|error| format!("wake provider id 不合法：{error}"))?;
                if provider.namespace() == "acb" {
                    return Err("树外插件不能用 acb namespace".into());
                }
                if wire.api_version != PLUGIN_API_VERSION {
                    return Err(format!(
                        "插件 API 版本 {} 不兼容（需要 {PLUGIN_API_VERSION}）",
                        wire.api_version
                    ));
                }
                let metadata = PluginMetadata::try_from(wire.metadata)
                    .map_err(|detail| format!("metadata 不合法：{detail}"))?;
                if metadata
                    .summary
                    .as_deref()
                    .map(str::trim)
                    .unwrap_or("")
                    .is_empty()
                {
                    return Err("wake 插件必须声明 metadata.summary".into());
                }
                if metadata.effects.is_empty() {
                    return Err(
                        "wake 插件必须声明 effects（至少 may_wake / may_stay_silent）".into(),
                    );
                }
                Ok(Self::Wake(DylibWakeManager::new(
                    Arc::new(library),
                    table,
                    provider,
                    wire.platform,
                    wire.id,
                    metadata,
                )))
            }
            "access" | "delivery" | "send" | "turn_error" | "receive" | "clock" => {
                let provider = ProviderId::parse(&wire.id)
                    .map_err(|error| format!("{} provider id 不合法：{error}", wire.extension))?;
                if provider.namespace() == "acb" {
                    return Err("树外插件不能用 acb namespace".into());
                }
                if wire.api_version != PLUGIN_API_VERSION {
                    return Err(format!(
                        "插件 API 版本 {} 不兼容（需要 {PLUGIN_API_VERSION}）",
                        wire.api_version
                    ));
                }
                let metadata = PluginMetadata::try_from(wire.metadata)
                    .map_err(|detail| format!("metadata 不合法：{detail}"))?;
                if metadata
                    .summary
                    .as_deref()
                    .map(str::trim)
                    .unwrap_or("")
                    .is_empty()
                {
                    return Err(format!("{} 插件必须声明 metadata.summary", wire.extension));
                }
                let point = match wire.extension.as_str() {
                    "access" => ExtensionPoint::Access,
                    "delivery" => ExtensionPoint::Delivery,
                    "send" => ExtensionPoint::Send,
                    "turn_error" => ExtensionPoint::TurnError,
                    "receive" => ExtensionPoint::Receive,
                    "clock" => ExtensionPoint::Clock,
                    _ => unreachable!("已经按扩展点分流"),
                };
                Ok(Self::Decision(DylibDecisionManager::new(
                    Arc::new(library),
                    table,
                    provider,
                    point,
                    wire.id,
                    metadata,
                )))
            }
            other => Err(format!("不认识的插件扩展点：{other}")),
        }
    }
}

struct DylibAdapterFactory {
    /// 库必须活到进程退出：函数指针都指向它。
    _library: Arc<Library>,
    table: AdapterV1,
    manifest: AdapterManifest,
}

fn read_manifest(table: &AdapterV1) -> Result<WireManifest, String> {
    let manifest_fn = table.manifest.ok_or("没有 manifest 函数")?;
    let pointer = unsafe { manifest_fn() };
    let json = take_owned_string(pointer, table)?;
    serde_json::from_str(&json).map_err(|error| format!("manifest 不是合法 JSON：{error}"))
}

impl AdapterFactory for DylibAdapterFactory {
    fn manifest(&self) -> AdapterManifest {
        self.manifest.clone()
    }

    fn create(
        &self,
        connection: &Connection,
        events: EventQueue,
    ) -> Result<AdapterInstance, AdapterError> {
        self.create_instance(connection, events)
            .map_err(|detail| AdapterError::Create(connection.id.clone(), detail))
    }
}

impl DylibAdapterFactory {
    fn create_instance(
        &self,
        connection: &Connection,
        events: EventQueue,
    ) -> Result<AdapterInstance, String> {
        let context = Arc::new(HostContext {
            events,
            stop: Arc::new(AtomicBool::new(false)),
            connection: connection.id.clone(),
        });
        let host = HostV1 {
            abi_version: ABI_VERSION,
            ctx: Arc::as_ptr(&context) as *mut c_void,
            submit_event: Some(submit_event),
            should_stop: Some(should_stop),
            log: Some(log),
        };

        let config = WireInstanceConfig::Adapter {
            connection: WireConnection::from(connection),
        };
        let config_json = CString::new(
            encode_line(&config).map_err(|error| format!("连接配置序列化失败：{error}"))?,
        )
        .map_err(|_| "连接配置里含有 NUL".to_owned())?;

        let create = self.table.create.ok_or("没有 create 函数")?;
        let handle_ptr = unsafe { create(config_json.as_ptr(), &host) };
        if handle_ptr.is_null() {
            return Err("create 返回了空 handle".into());
        }
        let handle = Arc::new(Mutex::new(AdapterHandle {
            ptr: handle_ptr,
            alive: true,
        }));

        let catalog = match describe(&self.table, &handle) {
            Ok(catalog) => catalog,
            Err(detail) => {
                destroy_handle(&self.table, &handle);
                return Err(format!("读平台方法目录失败：{detail}"));
            }
        };

        let outbound: Box<dyn Outbound> = Box::new(DylibOutbound {
            _library: Arc::clone(&self._library),
            _context: Arc::clone(&context),
            table: self.table,
            handle: Arc::clone(&handle),
            connection: connection.id.clone(),
            catalog,
            seq: 0,
        });

        let table = self.table;
        let stop = Arc::clone(&context.stop);
        let connection_id = connection.id.clone();
        let launch: Box<dyn FnOnce(AdapterHost) + Send> = Box::new(move |host| {
            monitor(
                host.shutdown,
                stop,
                table,
                handle,
                context,
                connection_id,
            );
        });

        Ok(AdapterInstance {
            outbound,
            launch,
            manifest: self.manifest.clone(),
        })
    }
}

fn describe(
    table: &AdapterV1,
    handle: &Arc<Mutex<AdapterHandle>>,
) -> Result<Arc<[MethodSpec]>, String> {
    let reply = invoke(table, handle, &CoreMessage::Describe { id: 1 })?;
    match reply {
        AdapterMessage::Reply {
            result: WireResult::Catalog { catalog },
            ..
        } => convert_catalog(catalog).map_err(|detail| format!("平台方法目录不合法：{detail}")),
        AdapterMessage::Reply {
            result: WireResult::Error { error },
            ..
        } => Err(format!("插件拒绝 describe：{error:?}")),
        other => Err(format!("describe 回执不对：{other:?}")),
    }
}

/// 动态库 wake provider 的 manager。一个插件一个子树，namespace 来自 provider id。
struct DylibWakeManager {
    library: Arc<Library>,
    table: AdapterV1,
    provider: ProviderId,
    platform: String,
    plugin_id: String,
    metadata: PluginMetadata,
    cached: Mutex<Option<Arc<DylibWakeProvider>>>,
}

impl DylibWakeManager {
    fn new(
        library: Arc<Library>,
        table: AdapterV1,
        provider: ProviderId,
        platform: String,
        plugin_id: String,
        metadata: PluginMetadata,
    ) -> Self {
        Self {
            library,
            table,
            provider,
            platform,
            plugin_id,
            metadata,
            cached: Mutex::new(None),
        }
    }
}

impl ManagerPlugin for DylibWakeManager {
    fn manifest(&self) -> ManagerManifest {
        ManagerManifest::new(
            self.provider.namespace(),
            PLUGIN_API_VERSION,
            vec![Capability::WakeDecision],
        )
        .with_metadata(self.metadata.clone())
    }

    fn descriptors(&self) -> Vec<ProviderDescriptor> {
        vec![ProviderDescriptor::new(
            self.provider.clone(),
            ExtensionPoint::Wake,
            PLUGIN_API_VERSION,
            vec![Capability::WakeDecision],
        )
        .with_metadata(self.metadata.clone())
        .with_source(PluginSource::Dynamic)]
    }

    fn instantiate(
        &self,
        id: &ProviderId,
        options: ProviderOptions<'_>,
    ) -> Result<ProviderInstance, PluginError> {
        if id != &self.provider {
            return Err(PluginError::UnknownProvider(id.to_string()));
        }
        let ProviderOptions::Wake(policy) = options else {
            return Err(PluginError::Provider {
                id: id.to_string(),
                detail: "wake provider 需要 WakePolicy".into(),
            });
        };

        let mut cached = self
            .cached
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(provider) = cached.as_ref() {
            let provider: Arc<dyn WakeProvider> = provider.clone();
            return Ok(ProviderInstance::Wake(provider));
        }
        let provider = DylibWakeProvider::create(
            Arc::clone(&self.library),
            self.table,
            &self.plugin_id,
            &self.platform,
            policy,
        )
        .map_err(|detail| PluginError::Provider {
            id: id.to_string(),
            detail,
        })?;
        let provider = Arc::new(provider);
        *cached = Some(Arc::clone(&provider));
        let dynamic: Arc<dyn WakeProvider> = provider;
        Ok(ProviderInstance::Wake(dynamic))
    }

    fn shutdown(&self) {
        let provider = self
            .cached
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(provider) = provider {
            provider.destroy();
        }
    }

    fn health(&self) -> Health {
        Health::Healthy
    }
}

struct DylibWakeProvider {
    _library: Arc<Library>,
    _context: Arc<HostContext>,
    table: AdapterV1,
    handle: Arc<Mutex<AdapterHandle>>,
    seq: AtomicU64,
    failures: AtomicU64,
}

impl DylibWakeProvider {
    fn create(
        library: Arc<Library>,
        table: AdapterV1,
        plugin_id: &str,
        platform: &str,
        policy: &WakePolicy,
    ) -> Result<Self, String> {
        // wake 判定不需要入站事件；给一个空队列，插件误发事件时会被记录为队列关闭。
        let (events, _receiver) = EventQueue::channel();
        let context = Arc::new(HostContext {
            events,
            stop: Arc::new(AtomicBool::new(false)),
            connection: plugin_id.to_owned(),
        });
        let host = HostV1 {
            abi_version: ABI_VERSION,
            ctx: Arc::as_ptr(&context) as *mut c_void,
            submit_event: Some(submit_event),
            should_stop: Some(should_stop),
            log: Some(log),
        };
        let config = WireInstanceConfig::Provider {
            provider: plugin_id.to_owned(),
            extension: "wake".into(),
            options: wake_options(policy),
        };
        let config_json = CString::new(
            encode_line(&config).map_err(|error| format!("wake 配置序列化失败：{error}"))?,
        )
        .map_err(|_| "wake 配置里含有 NUL".to_owned())?;

        let create = table.create.ok_or("没有 create 函数")?;
        let handle_ptr = unsafe { create(config_json.as_ptr(), &host) };
        if handle_ptr.is_null() {
            return Err(format!(
                "wake 插件 {plugin_id}（platform {platform}）create 返回空 handle"
            ));
        }

        Ok(Self {
            _library: library,
            _context: context,
            table,
            handle: Arc::new(Mutex::new(AdapterHandle {
                ptr: handle_ptr,
                alive: true,
            })),
            seq: AtomicU64::new(0),
            failures: AtomicU64::new(0),
        })
    }

    fn note_failure(&self, detail: &str) {
        let count = self.failures.fetch_add(1, Ordering::Relaxed);
        if count < 5 || count % 100 == 0 {
            eprintln!("wake 插件失败（第 {} 次）：{detail}", count + 1);
        }
    }

    fn destroy(&self) {
        destroy_handle(&self.table, &self.handle);
    }
}

impl WakeProvider for DylibWakeProvider {
    fn decide(&self, options: &WakePolicy, context: &WakeContext<'_>) -> WakeDecision {
        self.decide_with_reason(options, context).decision
    }

    fn decide_with_reason(
        &self,
        options: &WakePolicy,
        context: &WakeContext<'_>,
    ) -> WakeVerdict {
        let id = self.seq.fetch_add(1, Ordering::Relaxed);
        let request = WireWakeRequest {
            session: context.session.to_owned(),
            room: context.room.to_string(),
            connection: context.connection.to_owned(),
            platform: context.platform.to_owned(),
            sender_id: context.sender_id.to_owned(),
            text: context.text.to_owned(),
            at_self: context.at_self,
            reply_to_self: context.reply_to_self,
            event: context.event.map(str::to_owned),
            tags: context.tags.iter().map(|tag| (*tag).to_owned()).collect(),
            unread: context.unread,
            last_wake_at: context.last_wake_at,
            now: context.now,
        };
        let request = serde_json::to_value(request).unwrap_or(Value::Null);
        let message = CoreMessage::Decide {
            id,
            point: "wake".into(),
            options: wake_options(options),
            request,
        };

        match invoke(&self.table, &self.handle, &message) {
            Ok(AdapterMessage::Reply {
                result: WireResult::Decision { decision, reason },
                ..
            }) => match decision.as_str() {
                "wake" => WakeVerdict {
                    decision: WakeDecision::Wake,
                    reason,
                },
                "stay" => WakeVerdict::stay(),
                other => {
                    self.note_failure(&format!("不认识的 wake 判定：{other}"));
                    WakeVerdict::stay()
                }
            },
            Ok(other) => {
                self.note_failure(&format!("回执不是 wake 判定：{other:?}"));
                WakeVerdict::stay()
            }
            Err(detail) => {
                self.note_failure(&detail);
                WakeVerdict::stay()
            }
        }
    }
}

fn wake_options(policy: &WakePolicy) -> Value {
    json!({
        "provider": policy.provider.to_string(),
        "at_mention": policy.at_mention,
        "reply": policy.reply,
        "name_mention": policy.name_mention,
        "names": policy.names,
        "accumulate": policy.accumulate,
        "event_tags": policy.event_tags,
        "events": policy.events,
        "keywords": policy.keywords,
        "ignore_speakers": policy.ignore_speakers,
        "extra": policy.options,
    })
}

/// 动态库决策型策略 provider 的 manager（access / receive / clock）。
///
/// 和 wake 一样：一个插件一个子树，namespace 来自 provider id；判定在 core 的同步路径里，
/// 所以只开放动态库载体（外置进程会在 core 锁内做 I/O）。
struct DylibDecisionManager {
    library: Arc<Library>,
    table: AdapterV1,
    provider: ProviderId,
    point: ExtensionPoint,
    plugin_id: String,
    metadata: PluginMetadata,
    cached: Mutex<Option<CachedDecision>>,
}

enum CachedDecision {
    Access(Arc<DylibAccessProvider>),
    Delivery(Arc<DylibDeliveryComposer>),
    Send(Arc<DylibSendHook>),
    TurnError(Arc<DylibTurnErrorProvider>),
    Receive(Arc<DylibReceiveProvider>),
    Clock(Arc<DylibClockProvider>),
}

impl CachedDecision {
    fn instance(&self) -> ProviderInstance {
        match self {
            Self::Access(provider) => ProviderInstance::Access(provider.clone()),
            Self::Delivery(provider) => ProviderInstance::Delivery(provider.clone()),
            Self::Send(provider) => ProviderInstance::Send(provider.clone()),
            Self::TurnError(provider) => ProviderInstance::TurnError(provider.clone()),
            Self::Receive(provider) => ProviderInstance::Receive(provider.clone()),
            Self::Clock(provider) => ProviderInstance::Clock(provider.clone()),
        }
    }

    fn destroy(&self) {
        match self {
            Self::Access(provider) => provider.channel.destroy(),
            Self::Delivery(provider) => provider.channel.destroy(),
            Self::Send(provider) => provider.channel.destroy(),
            Self::TurnError(provider) => provider.channel.destroy(),
            Self::Receive(provider) => provider.channel.destroy(),
            Self::Clock(provider) => provider.channel.destroy(),
        }
    }
}

impl DylibDecisionManager {
    fn new(
        library: Arc<Library>,
        table: AdapterV1,
        provider: ProviderId,
        point: ExtensionPoint,
        plugin_id: String,
        metadata: PluginMetadata,
    ) -> Self {
        Self {
            library,
            table,
            provider,
            point,
            plugin_id,
            metadata,
            cached: Mutex::new(None),
        }
    }

    fn capability(&self) -> Capability {
        match self.point {
            ExtensionPoint::Access => Capability::AccessDecision,
            ExtensionPoint::Delivery => Capability::DeliverPlan,
            ExtensionPoint::Send => Capability::SendHook,
            ExtensionPoint::TurnError => Capability::TurnErrorDecision,
            ExtensionPoint::Receive => Capability::ReceiveDecision,
            ExtensionPoint::Clock => Capability::ClockDecision,
            _ => Capability::AccessDecision,
        }
    }
}

impl ManagerPlugin for DylibDecisionManager {
    fn manifest(&self) -> ManagerManifest {
        ManagerManifest::new(
            self.provider.namespace(),
            PLUGIN_API_VERSION,
            vec![self.capability()],
        )
        .with_metadata(self.metadata.clone())
    }

    fn descriptors(&self) -> Vec<ProviderDescriptor> {
        vec![ProviderDescriptor::new(
            self.provider.clone(),
            self.point,
            PLUGIN_API_VERSION,
            vec![self.capability()],
        )
        .with_metadata(self.metadata.clone())
        .with_source(PluginSource::Dynamic)]
    }

    fn instantiate(
        &self,
        id: &ProviderId,
        options: ProviderOptions<'_>,
    ) -> Result<ProviderInstance, PluginError> {
        if id != &self.provider {
            return Err(PluginError::UnknownProvider(id.to_string()));
        }
        let (extension, options_json) = match (self.point, options) {
            (ExtensionPoint::Access, ProviderOptions::Access(policy)) => {
                ("access", access_options(policy))
            }
            (ExtensionPoint::Delivery, ProviderOptions::Delivery(policy)) => {
                ("delivery", delivery_options(policy))
            }
            (ExtensionPoint::Send, ProviderOptions::None) => ("send", json!({})),
            (ExtensionPoint::TurnError, ProviderOptions::TurnError(policy)) => {
                ("turn_error", turn_error_options(policy))
            }
            (ExtensionPoint::Receive, ProviderOptions::Receive(policy)) => {
                ("receive", receive_options(policy))
            }
            (ExtensionPoint::Clock, ProviderOptions::Clock(policy)) => {
                ("clock", clock_options(policy))
            }
            (point, _) => {
                return Err(PluginError::Provider {
                    id: id.to_string(),
                    detail: format!("{point:?} provider 收到的 options 类型不对"),
                });
            }
        };

        let mut cached = self
            .cached
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(provider) = cached.as_ref() {
            return Ok(provider.instance());
        }

        let channel = Arc::new(
            DecisionChannel::create(
                Arc::clone(&self.library),
                self.table,
                &self.plugin_id,
                extension,
                options_json.clone(),
            )
            .map_err(|detail| PluginError::Provider {
                id: id.to_string(),
                detail,
            })?,
        );
        let provider = match extension {
            "access" => CachedDecision::Access(Arc::new(DylibAccessProvider {
                channel: Arc::clone(&channel),
            })),
            "delivery" => CachedDecision::Delivery(Arc::new(DylibDeliveryComposer {
                channel: Arc::clone(&channel),
            })),
            "send" => CachedDecision::Send(Arc::new(DylibSendHook {
                channel: Arc::clone(&channel),
                provider: id.to_string(),
            })),
            "turn_error" => CachedDecision::TurnError(Arc::new(DylibTurnErrorProvider {
                channel: Arc::clone(&channel),
            })),
            "receive" => CachedDecision::Receive(Arc::new(DylibReceiveProvider {
                channel: Arc::clone(&channel),
            })),
            "clock" => CachedDecision::Clock(Arc::new(DylibClockProvider {
                channel: Arc::clone(&channel),
            })),
            other => {
                return Err(PluginError::Provider {
                    id: id.to_string(),
                    detail: format!("不认识的决策扩展点：{other}"),
                });
            }
        };
        let instance = provider.instance();
        *cached = Some(provider);
        Ok(instance)
    }

    fn shutdown(&self) {
        let provider = self
            .cached
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(provider) = provider {
            provider.destroy();
        }
    }

    fn health(&self) -> Health {
        Health::Healthy
    }
}

/// 决策型 provider 的共用通道：一个 handle + 一串请求号。
struct DecisionChannel {
    _library: Arc<Library>,
    _context: Arc<HostContext>,
    table: AdapterV1,
    handle: Arc<Mutex<AdapterHandle>>,
    seq: AtomicU64,
    failures: AtomicU64,
    label: String,
}

impl DecisionChannel {
    fn create(
        library: Arc<Library>,
        table: AdapterV1,
        plugin_id: &str,
        extension: &str,
        options: Value,
    ) -> Result<Self, String> {
        let (events, _receiver) = EventQueue::channel();
        let context = Arc::new(HostContext {
            events,
            stop: Arc::new(AtomicBool::new(false)),
            connection: plugin_id.to_owned(),
        });
        let host = HostV1 {
            abi_version: ABI_VERSION,
            ctx: Arc::as_ptr(&context) as *mut c_void,
            submit_event: Some(submit_event),
            should_stop: Some(should_stop),
            log: Some(log),
        };
        let config = WireInstanceConfig::Provider {
            provider: plugin_id.to_owned(),
            extension: extension.to_owned(),
            options,
        };
        let config_json = CString::new(encode_line(&config).map_err(|error| {
            format!("{extension} 配置序列化失败：{error}")
        })?)
        .map_err(|_| format!("{extension} 配置里含有 NUL"))?;

        let create = table.create.ok_or("没有 create 函数")?;
        let handle_ptr = unsafe { create(config_json.as_ptr(), &host) };
        if handle_ptr.is_null() {
            return Err(format!(
                "{extension} 插件 {plugin_id} create 返回空 handle"
            ));
        }

        Ok(Self {
            _library: library,
            _context: context,
            table,
            handle: Arc::new(Mutex::new(AdapterHandle {
                ptr: handle_ptr,
                alive: true,
            })),
            seq: AtomicU64::new(0),
            failures: AtomicU64::new(0),
            label: extension.to_owned(),
        })
    }

    fn next(&self) -> u64 {
        self.seq.fetch_add(1, Ordering::Relaxed)
    }

    fn call(&self, message: &CoreMessage) -> Result<AdapterMessage, String> {
        invoke(&self.table, &self.handle, message)
    }

    fn note_failure(&self, detail: &str) {
        let count = self.failures.fetch_add(1, Ordering::Relaxed);
        if count < 5 || count % 100 == 0 {
            eprintln!("{} 插件失败（第 {} 次）：{detail}", self.label, count + 1);
        }
    }

    fn destroy(&self) {
        destroy_handle(&self.table, &self.handle);
    }
}

/// 发一次 Decide，取回 `decision` 字符串。回执形状不对或插件报错返回 `None`。
fn plugin_decision(channel: &DecisionChannel, message: CoreMessage) -> Option<String> {
    match channel.call(&message) {
        Ok(AdapterMessage::Reply {
            result: WireResult::Decision { decision, .. },
            ..
        }) => Some(decision),
        Ok(other) => {
            channel.note_failure(&format!("回执不是决策：{other:?}"));
            None
        }
        Err(detail) => {
            channel.note_failure(&detail);
            None
        }
    }
}

struct DylibAccessProvider {
    channel: Arc<DecisionChannel>,
}

impl AccessProvider for DylibAccessProvider {
    fn check(&self, options: &AccessPolicy, request: &AccessRequest<'_>) -> AccessDecision {
        let wire = WireAccessRequest {
            session: request.session.to_owned(),
            connection: request.connection.to_owned(),
            room: request.room.map(|room| room.to_string()),
            action: request.action.as_str().to_owned(),
            subscriptions: request
                .subscriptions
                .iter()
                .map(|room| room.to_string())
                .collect(),
        };
        let message = CoreMessage::Decide {
            id: self.channel.next(),
            point: "access".into(),
            options: access_options(options),
            request: serde_json::to_value(wire).unwrap_or(Value::Null),
        };
        match plugin_decision(&self.channel, message).as_deref() {
            Some("allow") => AccessDecision::Allow,
            Some("deny") => AccessDecision::Deny,
            Some(other) => {
                self.channel
                    .note_failure(&format!("不认识的 access 判定：{other}"));
                AccessDecision::Deny
            }
            None => AccessDecision::Deny,
        }
    }
}

struct DylibDeliveryComposer {
    channel: Arc<DecisionChannel>,
}

impl DeliveryComposer for DylibDeliveryComposer {
    fn compose(&self, options: &DeliveryPolicy, request: &DeliveryRequest<'_>) -> DeliveryPlan {
        let wire = WireDeliveryRequest {
            session: request.session.to_owned(),
            room: request.room.to_string(),
            limit: request.limit,
            records: request.records.to_vec(),
        };
        let message = CoreMessage::Decide {
            id: self.channel.next(),
            point: "delivery".into(),
            options: delivery_options(options),
            request: serde_json::to_value(wire).unwrap_or(Value::Null),
        };
        match self.channel.call(&message) {
            Ok(AdapterMessage::Reply {
                result: WireResult::Delivery { plan: Some(plan) },
                ..
            }) => DeliveryPlan {
                keep: plan.keep,
                omitted: plan.omitted,
            },
            // plan = null 与 Lua 的 nil 一样：明确要求用内置选批。
            Ok(AdapterMessage::Reply {
                result: WireResult::Delivery { plan: None },
                ..
            }) => default_delivery_plan(request),
            Ok(other) => {
                self.channel
                    .note_failure(&format!("回执不是投递计划：{other:?}"));
                default_delivery_plan(request)
            }
            Err(detail) => {
                self.channel.note_failure(&detail);
                default_delivery_plan(request)
            }
        }
    }
}

struct DylibTurnErrorProvider {
    channel: Arc<DecisionChannel>,
}

impl TurnErrorProvider for DylibTurnErrorProvider {
    fn decide(
        &self,
        policy: &TurnErrorPolicy,
        request: &TurnErrorRequest<'_>,
    ) -> Vec<TurnErrorAction> {
        let wire = WireTurnErrorRequest {
            session: request.session.to_owned(),
            room: request.room.to_string(),
            error: request.error.to_owned(),
            actions: request
                .actions
                .iter()
                .map(wire_turn_error_action)
                .collect(),
            default_actions: request
                .default_actions
                .iter()
                .map(wire_turn_error_action)
                .collect(),
        };
        let message = CoreMessage::Decide {
            id: self.channel.next(),
            point: "turn_error".into(),
            options: turn_error_options(policy),
            request: serde_json::to_value(wire).unwrap_or(Value::Null),
        };
        match self.channel.call(&message) {
            Ok(AdapterMessage::Reply {
                result: WireResult::Actions { actions },
                ..
            }) => actions
                .into_iter()
                .filter_map(|action| parse_turn_error_action(&self.channel, action))
                .collect(),
            Ok(other) => {
                self.channel
                    .note_failure(&format!("回执不是失败通知动作：{other:?}"));
                DefaultTurnErrorProvider.decide(policy, request)
            }
            Err(detail) => {
                self.channel.note_failure(&detail);
                DefaultTurnErrorProvider.decide(policy, request)
            }
        }
    }
}

fn wire_turn_error_action(action: &TurnErrorAction) -> WireTurnErrorAction {
    match action {
        TurnErrorAction::Log => WireTurnErrorAction::Log,
        TurnErrorAction::Send { room } => WireTurnErrorAction::Send {
            room: room.to_string(),
        },
    }
}

fn parse_turn_error_action(
    channel: &DecisionChannel,
    action: WireTurnErrorAction,
) -> Option<TurnErrorAction> {
    match action {
        WireTurnErrorAction::Log => Some(TurnErrorAction::Log),
        WireTurnErrorAction::Send { room } => match RoomId::parse(&room) {
            Ok(room) => Some(TurnErrorAction::Send { room }),
            Err(error) => {
                channel.note_failure(&format!("失败通知房间不合法：{room}（{error}）"));
                None
            }
        },
    }
}

struct DylibSendHook {
    channel: Arc<DecisionChannel>,
    provider: String,
}

impl SendHook for DylibSendHook {
    fn before_send(
        &self,
        context: &SendContext<'_>,
        elements: &mut Vec<Element>,
    ) -> Result<(), PluginError> {
        let wire = WireSendRequest {
            session: context.session.to_owned(),
            room: context.room.to_string(),
            phase: "before".into(),
            origin: context.origin.as_str().to_owned(),
            elements: elements.clone(),
            platform_id: None,
            at: None,
        };
        let message = CoreMessage::Decide {
            id: self.channel.next(),
            point: "send".into(),
            options: json!({}),
            request: serde_json::to_value(wire).unwrap_or(Value::Null),
        };
        match self.channel.call(&message) {
            Ok(AdapterMessage::Reply {
                result: WireResult::Elements { elements: replacement },
                ..
            }) => {
                *elements = replacement;
                Ok(())
            }
            // 插件只实现了 after 时，before 返回 keep 即可明确表示「不改」。
            Ok(AdapterMessage::Reply {
                result: WireResult::Decision { decision, .. },
                ..
            }) if decision == "keep" || decision == "ok" => Ok(()),
            Ok(AdapterMessage::Reply {
                result: WireResult::Error { error: WireError::Unsupported { .. } },
                ..
            }) => Ok(()),
            Ok(AdapterMessage::Reply {
                result: WireResult::Error { error },
                ..
            }) => {
                self.channel
                    .note_failure(&format!("发送前钩子报错：{error:?}"));
                Err(PluginError::Provider {
                    id: self.provider.clone(),
                    detail: format!("发送前钩子报错：{error:?}"),
                })
            }
            Ok(other) => {
                self.channel
                    .note_failure(&format!("发送前钩子回执不对：{other:?}"));
                Err(PluginError::Provider {
                    id: self.provider.clone(),
                    detail: format!("发送前钩子回执不对：{other:?}"),
                })
            }
            Err(detail) => {
                self.channel.note_failure(&detail);
                Err(PluginError::Provider {
                    id: self.provider.clone(),
                    detail,
                })
            }
        }
    }

    fn after_send(&self, context: &SendContext<'_>, sent: &Sent) {
        let wire = WireSendRequest {
            session: context.session.to_owned(),
            room: context.room.to_string(),
            phase: "after".into(),
            origin: context.origin.as_str().to_owned(),
            elements: Vec::new(),
            platform_id: Some(sent.platform_id.clone()),
            at: Some(sent.at),
        };
        let message = CoreMessage::Decide {
            id: self.channel.next(),
            point: "send".into(),
            options: json!({}),
            request: serde_json::to_value(wire).unwrap_or(Value::Null),
        };
        if let Err(detail) = self.channel.call(&message) {
            self.channel.note_failure(&detail);
        }
    }
}

struct DylibReceiveProvider {
    channel: Arc<DecisionChannel>,
}

impl ReceiveProvider for DylibReceiveProvider {
    fn decide(&self, options: &ReceivePolicy, request: &ReceiveRequest<'_>) -> ReceiveDecision {
        let wire = WireReceiveRequest {
            connection: request.connection.to_owned(),
            platform: request.platform.to_owned(),
            room: request.room.to_string(),
            sender: request.sender.clone(),
            kind: request.kind.clone(),
            elements: request.elements.to_vec(),
            text: request.text.to_owned(),
            at_self: request.at_self,
        };
        let message = CoreMessage::Decide {
            id: self.channel.next(),
            point: "receive".into(),
            options: receive_options(options),
            request: serde_json::to_value(wire).unwrap_or(Value::Null),
        };
        match plugin_decision(&self.channel, message).as_deref() {
            Some("record") => ReceiveDecision::Record,
            Some("drop") => ReceiveDecision::Drop,
            Some(other) => {
                self.channel
                    .note_failure(&format!("不认识的 receive 判定：{other}"));
                ReceiveDecision::Record
            }
            // 出错按「接收」兜底：宁可多记一条，也不误丢消息。
            None => ReceiveDecision::Record,
        }
    }
}

struct DylibClockProvider {
    channel: Arc<DecisionChannel>,
}

impl ClockProvider for DylibClockProvider {
    fn is_due(&self, options: &ClockPolicy, context: &ClockContext<'_>) -> bool {
        let wire = WireClockRequest {
            session: context.session.to_owned(),
            room: context.room.to_string(),
            now: context.now,
            last_clock_at: context.last_clock_at,
            last_wake_at: context.last_wake_at,
            last_record_at: context.last_record_at,
        };
        let message = CoreMessage::Decide {
            id: self.channel.next(),
            point: "clock".into(),
            options: clock_options(options),
            request: serde_json::to_value(wire).unwrap_or(Value::Null),
        };
        match plugin_decision(&self.channel, message).as_deref() {
            Some("due") => true,
            Some("skip") => false,
            Some(other) => {
                self.channel
                    .note_failure(&format!("不认识的 clock 判定：{other}"));
                false
            }
            // 出错按「不触发」兜底：时钟只是例行检查，不该因为插件坏了刷屏。
            None => false,
        }
    }
}

fn access_options(policy: &AccessPolicy) -> Value {
    json!({
        "provider": policy.provider.to_string(),
        "rooms": policy.rooms.iter().map(|room| room.to_string()).collect::<Vec<_>>(),
        "extra": policy.options,
    })
}

fn delivery_options(policy: &DeliveryPolicy) -> Value {
    json!({
        "provider": policy.provider.to_string(),
        "extra": policy.options,
    })
}

fn turn_error_options(policy: &TurnErrorPolicy) -> Value {
    json!({
        "provider": policy.provider.to_string(),
        "extra": policy.options,
    })
}

fn receive_options(policy: &ReceivePolicy) -> Value {
    json!({
        "provider": policy.provider.to_string(),
        "extra": policy.options,
    })
}

fn clock_options(policy: &ClockPolicy) -> Value {
    json!({
        "provider": policy.provider.to_string(),
        "interval_secs": policy.interval_secs,
        "idle_secs": policy.idle_secs,
        "extra": policy.options,
    })
}

struct HostContext {
    events: EventQueue,
    stop: Arc<AtomicBool>,
    connection: String,
}

struct AdapterHandle {
    ptr: *mut c_void,
    alive: bool,
}

unsafe impl Send for AdapterHandle {}

struct DylibOutbound {
    _library: Arc<Library>,
    /// 插件可能随时回调 HostV1；这份 Arc 保证 ctx 指针一直有效。
    _context: Arc<HostContext>,
    table: AdapterV1,
    handle: Arc<Mutex<AdapterHandle>>,
    connection: String,
    catalog: Arc<[MethodSpec]>,
    seq: u64,
}

impl Outbound for DylibOutbound {
    fn send(&mut self, room: &RoomId, elements: &[Element]) -> Result<Sent, OutboundError> {
        if room.connection != self.connection {
            return Err(OutboundError::Unavailable(format!(
                "这条连接叫 {}，收到 {}",
                self.connection, room.connection
            )));
        }

        self.seq += 1;
        let reply = invoke_with_timeout(
            &self.table,
            &self.handle,
            &CoreMessage::Send {
                id: self.seq,
                room: room.clone(),
                elements: elements.to_vec(),
            },
            CALL_TIMEOUT,
        )
        .map_err(OutboundError::Unavailable)?;

        match reply {
            AdapterMessage::Reply {
                result: WireResult::Sent { platform_id, at },
                ..
            } => Ok(Sent { platform_id, at }),
            AdapterMessage::Reply {
                result: WireResult::Error { error },
                ..
            } => Err(send_error(error)),
            other => Err(OutboundError::Unavailable(format!(
                "动态适配器回执不是发送结果：{other:?}"
            ))),
        }
    }

    fn catalog(&self, _connection: &str) -> Arc<[MethodSpec]> {
        Arc::clone(&self.catalog)
    }

    fn call(
        &mut self,
        room: &RoomId,
        method: &str,
        params: &serde_json::Value,
    ) -> Result<CallOutcome, CallError> {
        if room.connection != self.connection {
            return Err(CallError::Forbidden(format!(
                "这条连接叫 {}，收到 {} 的调用",
                self.connection, room.connection
            )));
        }

        self.seq += 1;
        let reply = invoke_with_timeout(
            &self.table,
            &self.handle,
            &CoreMessage::Call {
                id: self.seq,
                room: room.clone(),
                method: method.to_owned(),
                params: params.clone(),
            },
            CALL_TIMEOUT,
        )
        .map_err(CallError::Unavailable)?;

        match reply {
            AdapterMessage::Reply {
                result: WireResult::Call { value, record },
                ..
            } => {
                let record = record
                    .map(OutgoingRecord::try_from)
                    .transpose()
                    .map_err(CallError::Unavailable)?;
                Ok(CallOutcome { value, record })
            }
            AdapterMessage::Reply {
                result: WireResult::Error { error },
                ..
            } => Err(call_error(error)),
            other => Err(CallError::Unavailable(format!(
                "动态适配器回执不是调用结果：{other:?}"
            ))),
        }
    }
}

fn invoke(
    table: &AdapterV1,
    handle: &Arc<Mutex<AdapterHandle>>,
    message: &CoreMessage,
) -> Result<AdapterMessage, String> {
    let request = CString::new(
        encode_line(message).map_err(|error| format!("请求序列化失败：{error}"))?,
    )
    .map_err(|_| "请求里含有 NUL".to_owned())?;

    let guard = handle
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if !guard.alive {
        return Err("适配器已经销毁".into());
    }
    let invoke_fn = table.invoke.ok_or("没有 invoke 函数")?;
    let pointer = unsafe { invoke_fn(guard.ptr, request.as_ptr()) };
    // 锁在调用期间不放：插件的 handle 不是线程安全的，destroy 必须排队。
    drop(guard);

    if pointer.is_null() {
        return Err("适配器没有返回结果".into());
    }
    let json = take_owned_string(pointer, table)?;
    decode_line::<AdapterMessage>(&json)
        .map_err(|error| format!("回执不是合法 JSON：{error}"))
}

fn invoke_with_timeout(
    table: &AdapterV1,
    handle: &Arc<Mutex<AdapterHandle>>,
    message: &CoreMessage,
    timeout: Duration,
) -> Result<AdapterMessage, String> {
    let table = *table;
    let handle = Arc::clone(handle);
    let message = message.clone();
    let (tx, rx) = mpsc::channel();
    // 插件的 handle 不是线程安全的：整个 invoke 都在工作线程里，调用方只等结果。
    thread::spawn(move || {
        let _ = tx.send(invoke(&table, &handle, &message));
    });
    match rx.recv_timeout(timeout) {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Timeout) => Err(format!(
            "动态适配器调用超时（{} ms）；插件可能卡住了，需要重启桥",
            timeout.as_millis()
        )),
        Err(mpsc::RecvTimeoutError::Disconnected) => Err("动态适配器调用线程没了".into()),
    }
}

fn take_owned_string(pointer: *mut c_char, table: &AdapterV1) -> Result<String, String> {
    if pointer.is_null() {
        return Err("适配器返回了空字符串".into());
    }
    let text = unsafe { CStr::from_ptr(pointer).to_string_lossy().into_owned() };
    if let Some(free) = table.free_string {
        unsafe { free(pointer) };
    }
    Ok(text)
}

fn destroy_handle(table: &AdapterV1, handle: &Arc<Mutex<AdapterHandle>>) {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match handle.try_lock() {
            Ok(mut guard) => {
                destroy_locked(table, &mut guard);
                return;
            }
            Err(std::sync::TryLockError::Poisoned(poisoned)) => {
                let mut guard = poisoned.into_inner();
                destroy_locked(table, &mut guard);
                return;
            }
            Err(std::sync::TryLockError::WouldBlock) => {
                if Instant::now() >= deadline {
                    eprintln!("动态适配器有调用卡住，跳过 destroy（等重启桥）");
                    return;
                }
                thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

fn destroy_locked(table: &AdapterV1, guard: &mut AdapterHandle) {
    if !guard.alive {
        return;
    }
    if let Some(destroy) = table.destroy {
        unsafe { destroy(guard.ptr) };
    }
    guard.alive = false;
    guard.ptr = std::ptr::null_mut();
}

fn monitor(
    host_shutdown: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    table: AdapterV1,
    handle: Arc<Mutex<AdapterHandle>>,
    _context: Arc<HostContext>,
    connection: String,
) {
    while !host_shutdown.load(Ordering::Relaxed) {
        thread::sleep(Duration::from_millis(100));
    }
    stop.store(true, Ordering::Relaxed);

    // 先让插件自己收尾（Shutdown 可能返回空，忽略），再销毁 handle。
    let _ = invoke(&table, &handle, &CoreMessage::Shutdown);
    destroy_handle(&table, &handle);
    eprintln!("适配器 {connection}（dynamic）：已收尾");
}

unsafe extern "C" fn submit_event(ctx: *mut c_void, message_json: *const c_char) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if ctx.is_null() || message_json.is_null() {
            return;
        }
        let context = unsafe { &*(ctx as *const HostContext) };
        let text = unsafe { CStr::from_ptr(message_json) }
            .to_string_lossy()
            .into_owned();
        match decode_line::<AdapterMessage>(&text) {
            Ok(AdapterMessage::Event { record }) => {
                if !context.events.submit(record.into()) {
                    eprintln!(
                        "适配器 {}（dynamic）：入站队列已关闭，丢掉一条记录",
                        context.connection
                    );
                }
            }
            Ok(other) => eprintln!(
                "适配器 {}（dynamic）：submit_event 只接受 event，收到 {:?}",
                context.connection, other
            ),
            Err(error) => eprintln!(
                "适配器 {}（dynamic）：submit_event 收到读不懂的 JSON：{error}",
                context.connection
            ),
        }
    }));
}

unsafe extern "C" fn should_stop(ctx: *mut c_void) -> i32 {
    if ctx.is_null() {
        return 1;
    }
    let context = unsafe { &*(ctx as *const HostContext) };
    i32::from(context.stop.load(Ordering::Relaxed))
}

unsafe extern "C" fn log(ctx: *mut c_void, level: *const c_char, message: *const c_char) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        let connection = if ctx.is_null() {
            "?".to_owned()
        } else {
            unsafe { &*(ctx as *const HostContext) }.connection.clone()
        };
        let level = if level.is_null() {
            "log".into()
        } else {
            unsafe { CStr::from_ptr(level) }
                .to_string_lossy()
                .into_owned()
        };
        let message = if message.is_null() {
            String::new()
        } else {
            unsafe { CStr::from_ptr(message) }
                .to_string_lossy()
                .into_owned()
        };
        eprintln!("适配器 {connection}（dynamic {level}）：{message}");
    }));
}
