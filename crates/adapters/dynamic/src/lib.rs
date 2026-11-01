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
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use acb_adapter_api::abi::{ABI_VERSION, AdapterV1, ENTRY_SYMBOL, EntryFn, HostV1};
use acb_adapter_api::{
    AdapterMessage, CoreMessage, WireConnection, WireInstanceConfig, WireManifest, WireResult,
    WireWakeRequest, decode_line, encode_line,
};
use acb_kernel::wire::{call_error, convert_catalog, send_error};
use acb_kernel::{
    AdapterError, AdapterFactory, AdapterHost, AdapterInstance, AdapterManifest, CallError,
    CallOutcome, Capability, Connection, Element, EventQueue, ExtensionPoint, Health, LoaderKind,
    LoaderManifest, ManagerManifest, ManagerPlugin, MethodSpec, Outbound, OutboundError,
    OutgoingRecord, PluginConfig, PluginError, PluginLoader, PluginManager, ProviderDescriptor,
    ProviderId, ProviderInstance, ProviderOptions, RoomId, Sent, WakeContext, WakeDecision,
    WakePolicy, WakeProvider, PLUGIN_API_VERSION, validate_manifest,
};
use libloading::Library;
use serde_json::{Value, json};
use std::sync::atomic::AtomicU64;

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
            vec![ExtensionPoint::PlatformAdapter, ExtensionPoint::Wake],
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
                    .register_adapter(Arc::new(factory))
                    .map_err(|error| PluginError::Loader(error.to_string()))?,
                LoadedPlugin::Wake(wake) => manager
                    .install_manager(Arc::new(wake))
                    .map_err(|error| PluginError::Loader(error.to_string()))?,
            }
        }
        Ok(())
    }
}

enum LoadedPlugin {
    Adapter(DylibAdapterFactory),
    Wake(DylibWakeManager),
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
                Ok(Self::Wake(DylibWakeManager::new(
                    Arc::new(library),
                    table,
                    provider,
                    wire.platform,
                    wire.id,
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

        Ok(AdapterInstance { outbound, launch })
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
    cached: Mutex<Option<Arc<DylibWakeProvider>>>,
}

impl DylibWakeManager {
    fn new(
        library: Arc<Library>,
        table: AdapterV1,
        provider: ProviderId,
        platform: String,
        plugin_id: String,
    ) -> Self {
        Self {
            library,
            table,
            provider,
            platform,
            plugin_id,
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
    }

    fn descriptors(&self) -> Vec<ProviderDescriptor> {
        vec![ProviderDescriptor::new(
            self.provider.clone(),
            ExtensionPoint::Wake,
            PLUGIN_API_VERSION,
            vec![Capability::WakeDecision],
        )]
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
                result: WireResult::Decision { decision, .. },
                ..
            }) => match decision.as_str() {
                "wake" => WakeDecision::Wake,
                "stay" => WakeDecision::Stay,
                other => {
                    self.note_failure(&format!("不认识的 wake 判定：{other}"));
                    WakeDecision::Stay
                }
            },
            Ok(other) => {
                self.note_failure(&format!("回执不是 wake 判定：{other:?}"));
                WakeDecision::Stay
            }
            Err(detail) => {
                self.note_failure(&detail);
                WakeDecision::Stay
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
        "poke": policy.poke,
        "events": policy.events,
        "keywords": policy.keywords,
        "ignore_speakers": policy.ignore_speakers,
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
        let reply = invoke(
            &self.table,
            &self.handle,
            &CoreMessage::Send {
                id: self.seq,
                room: room.clone(),
                elements: elements.to_vec(),
            },
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
        let reply = invoke(
            &self.table,
            &self.handle,
            &CoreMessage::Call {
                id: self.seq,
                room: room.clone(),
                method: method.to_owned(),
                params: params.clone(),
            },
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
    let mut guard = handle
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
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
