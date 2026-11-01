//! LuaManager / LuaLoader（ADR-0026 Phase 3b）。
//!
//! 一个 `.lua` 文件 = 一个 provider，文件所在目录由 `ACB_LUA_DIR` 指定。脚本按扩展点
//! 返回不同形状的 table：
//!
//! ```lua
//! -- wake
//! return {
//!   extension = "wake",
//!   decide = function(options, context) return "wake" end, -- wake/stay
//! }
//!
//! -- access
//! return {
//!   extension = "access",
//!   decide = function(options, request) return "allow" end, -- allow/deny
//! }
//!
//! -- delivery：返回本轮要投的编号；nil = 用内置默认选批
//! return {
//!   extension = "delivery",
//!   compose = function(options, request)
//!     return { keep = { request.records[#request.records].id } }
//!   end,
//! }
//!
//! -- send：before 可改写元素数组，after 只观察回执
//! return {
//!   extension = "send",
//!   before = function(context, elements)
//!     elements[#elements + 1] = { type = "text", text = " [签名]" }
//!     return elements
//!   end,
//!   after = function(context, sent) end,
//! }
//!
//! -- turn_error：返回要执行的动作列表
//! return {
//!   extension = "turn_error",
//!   decide = function(request)
//!     return { { kind = "log" }, { kind = "send", room = "qq:group/ops" } }
//!   end,
//! }
//!
//! -- clock：到点返回 true
//! return {
//!   extension = "clock",
//!   is_due = function(options, context)
//!     return context.now - context.last_clock_at >= 3600000
//!   end,
//! }
//!
//! -- receive：决定这条入站记录收不收
//! return {
//!   extension = "receive",
//!   decide = function(request)
//!     if request.sender.id == "bot" then return "drop" end
//!     return "record"
//!   end,
//! }
//! ```
//!
//! 文件名就是 provider 名，最终 id 是 `lua.<文件名>`。`options` / `context` / `request`
//! 是普通 table；脚本拿不到 Store、Bridge 或配置写入能力。每次调用最多 50ms，死循环会被
//! 指令钩子打断：wake/access/clock/receive 按安全侧兜底（receive 出错按接收），
//! delivery/turn_error 回退内置实现，send 的 before 报错则这次发送失败。
//! 失败数累计在 manager health 里。
//!
//! LuaManager 本身是 native 实现：先有它（以及 mlua 运行时），LuaLoader 才能加载脚本。
//! 这是 ADR-0026 的 bootstrap 约束——manager 不能由自己管理的格式来加载。

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use acb_kernel::{
    AccessDecision, AccessPolicy, AccessProvider, AccessRequest, Capability, ClockContext,
    ClockPolicy, ClockProvider, DefaultTurnErrorProvider, DeliveryComposer, DeliveryPlan,
    DeliveryPolicy, DeliveryRequest, Element, ExtensionPoint, Health, LoaderKind, LoaderManifest,
    ManagerManifest, ManagerPlugin, Omitted, PluginError, PluginLoader, PluginManager,
    PluginMetadata, PluginSource, ProviderDescriptor, ProviderId, ProviderInstance, ProviderOptions, ReceiveDecision,
    ReceivePolicy, ReceiveProvider, ReceiveRequest, Record, RecordKind, ReloadScope, RoomId,
    SendContext, SendHook, SenderRole, Sent, TurnErrorAction, TurnErrorProvider, TurnErrorRequest,
    WakeContext,
    WakeDecision, WakePolicy, WakeProvider, WakeVerdict, default_delivery_plan, PLUGIN_API_VERSION,
};
use acb_proto::element::flatten_text;
use mlua::{Function, HookTriggers, Lua, LuaOptions, RegistryKey, StdLib, Table, Value, VmState};

/// Lua manager 的 namespace。脚本不能自己挑 namespace，避免和别的 manager 抢。
pub const LUA_NAMESPACE: &str = "lua";

/// 脚本目录的环境变量。没设就不装 LuaLoader（默认行为不变）。
pub const LUA_DIR_ENV: &str = "ACB_LUA_DIR";

/// 单次 Lua 调用最多跑多久。超时按安全侧兜底：wake 不醒、access 拒绝。
const LUA_CALL_TIMEOUT: Duration = Duration::from_millis(50);

/// 每执行多少条 Lua 指令检查一次超时。
const TIMEOUT_HOOK_STEP: u32 = 10_000;

/// 一次调用拿到的 Lua 函数与它所属的状态。
struct LuaCaller {
    lua: Arc<Mutex<Lua>>,
    function: RegistryKey,
    timeout: Duration,
    failures: Arc<AtomicU64>,
    label: String,
}

impl LuaCaller {
    fn call(
        &self,
        run: impl FnOnce(&Lua, &Function) -> mlua::Result<Value>,
    ) -> mlua::Result<Value> {
        let lua = self.lua.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        // 上一次调用如果异常退出，钩子可能还留着；先清掉再装本次的。
        lua.remove_hook();
        let deadline = Instant::now() + self.timeout;
        lua.set_hook(
            HookTriggers::new().every_nth_instruction(TIMEOUT_HOOK_STEP),
            move |_, _| {
                if Instant::now() >= deadline {
                    Err(mlua::Error::runtime("Lua provider 调用超时"))
                } else {
                    Ok(VmState::Continue)
                }
            },
        )?;
        let result = lua
            .registry_value::<Function>(&self.function)
            .and_then(|function| run(&lua, &function));
        lua.remove_hook();
        result
    }

    fn record_failure(&self, error: &mlua::Error) {
        self.failures.fetch_add(1, Ordering::Relaxed);
        eprintln!("Lua provider {} 调用失败：{error}", self.label);
    }
}

struct LuaWakeProvider {
    caller: LuaCaller,
}

impl WakeProvider for LuaWakeProvider {
    fn decide(&self, options: &WakePolicy, context: &WakeContext<'_>) -> WakeDecision {
        self.decide_with_reason(options, context).decision
    }

    fn decide_with_reason(
        &self,
        options: &WakePolicy,
        context: &WakeContext<'_>,
    ) -> WakeVerdict {
        let result = self.caller.call(|lua, function| {
            let options = wake_options_table(lua, options)?;
            let context = wake_context_table(lua, context)?;
            function.call::<Value>((options, context))
        });
        match result {
            Ok(value) => match interpret_wake(value) {
                Ok(verdict) => verdict,
                Err(error) => {
                    self.caller.record_failure(&error);
                    WakeVerdict::stay()
                }
            },
            Err(error) => {
                self.caller.record_failure(&error);
                WakeVerdict::stay()
            }
        }
    }
}

struct LuaAccessProvider {
    caller: LuaCaller,
}

impl AccessProvider for LuaAccessProvider {
    fn check(&self, options: &AccessPolicy, request: &AccessRequest<'_>) -> AccessDecision {
        let result = self.caller.call(|lua, function| {
            let options = access_options_table(lua, options)?;
            let request = access_request_table(lua, request)?;
            function.call::<Value>((options, request))
        });
        match result {
            Ok(value) => match interpret_access(value) {
                Ok(decision) => decision,
                Err(error) => {
                    self.caller.record_failure(&error);
                    AccessDecision::Deny
                }
            },
            Err(error) => {
                self.caller.record_failure(&error);
                AccessDecision::Deny
            }
        }
    }
}

struct LuaDeliveryComposer {
    caller: LuaCaller,
}

impl DeliveryComposer for LuaDeliveryComposer {
    fn compose(&self, options: &DeliveryPolicy, request: &DeliveryRequest<'_>) -> DeliveryPlan {
        let result = self.caller.call(|lua, function| {
            let options = delivery_options_table(lua, options)?;
            let request = delivery_request_table(lua, request)?;
            function.call::<Value>((options, request))
        });
        match result {
            Ok(Value::Nil) => default_delivery_plan(request),
            Ok(value) => match interpret_delivery_plan(value) {
                Ok(plan) => plan,
                Err(error) => {
                    self.caller.record_failure(&error);
                    default_delivery_plan(request)
                }
            },
            Err(error) => {
                self.caller.record_failure(&error);
                default_delivery_plan(request)
            }
        }
    }
}

struct LuaSendHook {
    before: Option<LuaCaller>,
    after: Option<LuaCaller>,
}

impl SendHook for LuaSendHook {
    fn before_send(
        &self,
        context: &SendContext<'_>,
        elements: &mut Vec<Element>,
    ) -> Result<(), PluginError> {
        let Some(caller) = &self.before else {
            return Ok(());
        };
        let result = caller.call(|lua, function| {
            let context = send_context_table(lua, context)?;
            let elements = elements_to_table(lua, elements)?;
            function.call::<Value>((context, elements))
        });
        let replacement = match result {
            Ok(Value::Nil) => return Ok(()),
            Ok(Value::Table(table)) => elements_from_table(&table),
            Ok(other) => Err(mlua::Error::runtime(format!(
                "send.before 要返回元素数组或 nil，收到 {other:?}"
            ))),
            Err(error) => Err(error),
        };
        match replacement {
            Ok(new_elements) => {
                *elements = new_elements;
                Ok(())
            }
            Err(error) => {
                caller.record_failure(&error);
                Err(provider_error(caller, &error))
            }
        }
    }

    fn after_send(&self, context: &SendContext<'_>, sent: &Sent) {
        let Some(caller) = &self.after else {
            return;
        };
        let result = caller.call(|lua, function| {
            let context = send_context_table(lua, context)?;
            let sent = sent_table(lua, sent)?;
            function.call::<Value>((context, sent))
        });
        if let Err(error) = result {
            caller.record_failure(&error);
        }
    }
}

struct LuaTurnErrorProvider {
    caller: LuaCaller,
}

impl TurnErrorProvider for LuaTurnErrorProvider {
    fn decide(&self, request: &TurnErrorRequest<'_>) -> Vec<TurnErrorAction> {
        let result = self.caller.call(|lua, function| {
            let request = turn_error_request_table(lua, request)?;
            function.call::<Value>(request)
        });
        match result {
            Ok(value) => match interpret_turn_error_actions(value) {
                Ok(actions) => actions,
                Err(error) => {
                    self.caller.record_failure(&error);
                    DefaultTurnErrorProvider.decide(request)
                }
            },
            Err(error) => {
                self.caller.record_failure(&error);
                DefaultTurnErrorProvider.decide(request)
            }
        }
    }
}

struct LuaClockProvider {
    caller: LuaCaller,
}

impl ClockProvider for LuaClockProvider {
    fn is_due(&self, options: &ClockPolicy, context: &ClockContext<'_>) -> bool {
        let result = self.caller.call(|lua, function| {
            let options = clock_options_table(lua, options)?;
            let context = clock_context_table(lua, context)?;
            function.call::<Value>((options, context))
        });
        match result {
            Ok(Value::Boolean(due)) => due,
            Ok(Value::String(text)) => match text.to_string_lossy().as_str() {
                "due" | "wake" => true,
                "skip" | "stay" => false,
                other => {
                    self.caller.record_failure(&mlua::Error::runtime(format!(
                        "clock.is_due 返回了不认识的字符串 {other:?}"
                    )));
                    false
                }
            },
            Ok(other) => {
                self.caller.record_failure(&mlua::Error::runtime(format!(
                    "clock.is_due 要返回布尔值或 due/skip，收到 {other:?}"
                )));
                false
            }
            Err(error) => {
                self.caller.record_failure(&error);
                false
            }
        }
    }
}

struct LuaReceiveProvider {
    caller: LuaCaller,
}

impl ReceiveProvider for LuaReceiveProvider {
    fn decide(&self, _options: &ReceivePolicy, request: &ReceiveRequest<'_>) -> ReceiveDecision {
        let result = self.caller.call(|lua, function| {
            let request = receive_request_table(lua, request)?;
            function.call::<Value>(request)
        });
        match result {
            Ok(Value::Boolean(record)) => {
                if record {
                    ReceiveDecision::Record
                } else {
                    ReceiveDecision::Drop
                }
            }
            Ok(Value::String(text)) => match text.to_string_lossy().as_str() {
                "record" => ReceiveDecision::Record,
                "drop" => ReceiveDecision::Drop,
                other => {
                    self.caller.record_failure(&mlua::Error::runtime(format!(
                        "receive.decide 返回了不认识的字符串 {other:?}"
                    )));
                    ReceiveDecision::Record
                }
            },
            Ok(other) => {
                self.caller.record_failure(&mlua::Error::runtime(format!(
                    "receive.decide 要返回 record/drop 或布尔值，收到 {other:?}"
                )));
                ReceiveDecision::Record
            }
            Err(error) => {
                self.caller.record_failure(&error);
                ReceiveDecision::Record
            }
        }
    }
}

fn provider_error(caller: &LuaCaller, error: &mlua::Error) -> PluginError {
    PluginError::Provider {
        id: caller.label.clone(),
        detail: error.to_string(),
    }
}

/// 一个已经加载好的 provider：descriptor 信息和可克隆的实例。
struct LoadedProvider {
    id: ProviderId,
    extension: ExtensionPoint,
    instance: LoadedInstance,
}

enum LoadedInstance {
    Wake(Arc<dyn WakeProvider>),
    Access(Arc<dyn AccessProvider>),
    Delivery(Arc<dyn DeliveryComposer>),
    Send(Arc<dyn SendHook>),
    TurnError(Arc<dyn TurnErrorProvider>),
    Clock(Arc<dyn ClockProvider>),
    Receive(Arc<dyn ReceiveProvider>),
}

impl LoadedProvider {
    fn provider_instance(&self) -> ProviderInstance {
        match &self.instance {
            LoadedInstance::Wake(provider) => ProviderInstance::Wake(provider.clone()),
            LoadedInstance::Access(provider) => ProviderInstance::Access(provider.clone()),
            LoadedInstance::Delivery(composer) => ProviderInstance::Delivery(composer.clone()),
            LoadedInstance::Send(hook) => ProviderInstance::Send(hook.clone()),
            LoadedInstance::TurnError(provider) => ProviderInstance::TurnError(provider.clone()),
            LoadedInstance::Clock(provider) => ProviderInstance::Clock(provider.clone()),
            LoadedInstance::Receive(provider) => ProviderInstance::Receive(provider.clone()),
        }
    }
}

/// Lua 子树的管理者。provider 清单来自脚本目录，文件增删需要重启（reload 只换实现，
/// 不允许 descriptor 集合变化）。
pub struct LuaManager {
    dir: PathBuf,
    namespace: String,
    providers: RwLock<HashMap<ProviderId, LoadedProvider>>,
    descriptors: Vec<ProviderDescriptor>,
    failures: Arc<AtomicU64>,
}

impl LuaManager {
    fn load(dir: &Path, namespace: &str) -> Result<Self, PluginError> {
        let failures = Arc::new(AtomicU64::new(0));
        let (descriptors, providers) = load_scripts(dir, namespace, &failures)?;
        Ok(Self {
            dir: dir.to_owned(),
            namespace: namespace.to_owned(),
            providers: RwLock::new(providers),
            descriptors,
            failures,
        })
    }
}

impl ManagerPlugin for LuaManager {
    fn manifest(&self) -> ManagerManifest {
        ManagerManifest::new(
            self.namespace.clone(),
            PLUGIN_API_VERSION,
            vec![
                Capability::WakeDecision,
                Capability::AccessDecision,
                Capability::DeliverPlan,
                Capability::SendHook,
                Capability::TurnErrorDecision,
                Capability::ClockDecision,
                Capability::ReceiveDecision,
            ],
        )
        .with_metadata(PluginMetadata {
            title: Some("Lua 脚本插件".into()),
            summary: Some("ACB_LUA_DIR 下的 *.lua 脚本；每个脚本注册一个 provider".into()),
            description: None,
            tags: Vec::new().into(),
            effects: Vec::new().into(),
            docs_url: None,
            options_schema: None,
        })
    }

    fn descriptors(&self) -> Vec<ProviderDescriptor> {
        self.descriptors.clone()
    }

    fn instantiate(
        &self,
        id: &ProviderId,
        _options: ProviderOptions<'_>,
    ) -> Result<ProviderInstance, PluginError> {
        let providers = self
            .providers
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        providers
            .get(id)
            .map(LoadedProvider::provider_instance)
            .ok_or_else(|| PluginError::UnknownProvider(id.to_string()))
    }

    fn reload(&self, _scope: &ReloadScope) -> Result<(), PluginError> {
        let (descriptors, providers) = load_scripts(&self.dir, &self.namespace, &self.failures)?;
        let same_set = descriptors.len() == self.descriptors.len()
            && descriptors.iter().all(|descriptor| {
                self.descriptors.iter().any(|old| {
                    old.id == descriptor.id && old.extension_point == descriptor.extension_point
                })
            });
        if !same_set {
            return Err(PluginError::Manager {
                namespace: self.namespace.clone(),
                detail: "脚本增删了 provider；descriptor 集合变化需要重启桥".to_owned(),
            });
        }
        *self
            .providers
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = providers;
        Ok(())
    }

    fn health(&self) -> Health {
        let failures = self.failures.load(Ordering::Relaxed);
        if failures == 0 {
            Health::Healthy
        } else {
            Health::Degraded(format!("{failures} 次 Lua 调用失败"))
        }
    }
}

/// 脚本载体。目录来自 `LuaLoader::new`，`from_env` 读 `ACB_LUA_DIR`。
pub struct LuaLoader {
    dir: PathBuf,
}

impl LuaLoader {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// 没设 `ACB_LUA_DIR`（或设成空串）就返回 `None`，调用方不必装这个 loader。
    pub fn from_env() -> Option<Self> {
        let dir = std::env::var_os(LUA_DIR_ENV)?;
        if dir.is_empty() {
            return None;
        }
        Some(Self::new(dir))
    }
}

impl PluginLoader for LuaLoader {
    fn manifest(&self) -> LoaderManifest {
        LoaderManifest::new(
            LoaderKind::Script,
            PLUGIN_API_VERSION,
            vec![
                ExtensionPoint::Wake,
                ExtensionPoint::Access,
                ExtensionPoint::Delivery,
                ExtensionPoint::Send,
                ExtensionPoint::TurnError,
                ExtensionPoint::Clock,
                ExtensionPoint::Receive,
            ],
        )
    }

    fn install(&self, manager: &mut PluginManager) -> Result<(), PluginError> {
        manager.install_manager(Arc::new(LuaManager::load(&self.dir, LUA_NAMESPACE)?))
    }
}

fn load_scripts(
    dir: &Path,
    namespace: &str,
    failures: &Arc<AtomicU64>,
) -> Result<(Vec<ProviderDescriptor>, HashMap<ProviderId, LoadedProvider>), PluginError> {
    let entries = fs::read_dir(dir).map_err(|error| {
        PluginError::Loader(format!(
            "读不了 Lua 插件目录 {}：{error}",
            dir.display()
        ))
    })?;
    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| {
            PluginError::Loader(format!("读 Lua 插件目录 {} 失败：{error}", dir.display()))
        })?;
        let path = entry.path();
        let is_lua = path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("lua"));
        if is_lua && path.is_file() {
            paths.push(path);
        }
    }
    paths.sort();

    let mut providers = HashMap::new();
    let mut descriptors = Vec::new();
    for path in paths {
        let loaded = load_script(&path, namespace, failures)?;
        if providers.contains_key(&loaded.id) {
            return Err(PluginError::Loader(format!(
                "{} 的 provider id {} 重复",
                path.display(),
                loaded.id
            )));
        }
        let capability = match loaded.extension {
            ExtensionPoint::Wake => Capability::WakeDecision,
            ExtensionPoint::Access => Capability::AccessDecision,
            ExtensionPoint::Delivery => Capability::DeliverPlan,
            ExtensionPoint::Send => Capability::SendHook,
            ExtensionPoint::TurnError => Capability::TurnErrorDecision,
            ExtensionPoint::Clock => Capability::ClockDecision,
            ExtensionPoint::Receive => Capability::ReceiveDecision,
            point => {
                return Err(PluginError::UnsupportedExtensionPoint {
                    id: loaded.id.to_string(),
                    point,
                });
            }
        };
        descriptors.push(
            ProviderDescriptor::new(
                loaded.id.clone(),
                loaded.extension,
                PLUGIN_API_VERSION,
                vec![capability],
            )
            .with_source(PluginSource::Lua),
        );
        providers.insert(loaded.id.clone(), loaded);
    }
    Ok((descriptors, providers))
}

fn load_script(
    path: &Path,
    namespace: &str,
    failures: &Arc<AtomicU64>,
) -> Result<LoadedProvider, PluginError> {
    let stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .ok_or_else(|| {
            PluginError::Loader(format!("{}：文件名不是合法 UTF-8", path.display()))
        })?;
    let id = ProviderId::parse(&format!("{namespace}.{stem}")).map_err(|error| {
        PluginError::Loader(format!(
            "{}：文件名要能当 provider 名（字母/数字/下划线/短横线）：{error}",
            path.display()
        ))
    })?;
    let source = fs::read_to_string(path).map_err(|error| {
        PluginError::Loader(format!("读不了 {}：{error}", path.display()))
    })?;
    let lua = Lua::new_with(StdLib::ALL_SAFE, LuaOptions::default()).map_err(|error| {
        PluginError::Loader(format!("{}：创建 Lua 状态失败：{error}", path.display()))
    })?;
    let table: Table = lua
        .load(&source)
        .set_name(path.display().to_string())
        .eval()
        .map_err(|error| {
            PluginError::Loader(format!("{}：脚本执行失败：{error}", path.display()))
        })?;

    let extension: String = table.get("extension").map_err(|error| {
        PluginError::Loader(format!(
            "{}：缺少 extension（wake / access / delivery / send）：{error}",
            path.display()
        ))
    })?;
    let extension = match extension.as_str() {
        "wake" => ExtensionPoint::Wake,
        "access" => ExtensionPoint::Access,
        "delivery" => ExtensionPoint::Delivery,
        "send" => ExtensionPoint::Send,
        "turn_error" => ExtensionPoint::TurnError,
        "clock" => ExtensionPoint::Clock,
        "receive" => ExtensionPoint::Receive,
        other => {
            return Err(PluginError::Loader(format!(
                "{}：不支持的 extension {other:?}（只支持 wake / access / delivery / send / turn_error / clock / receive）",
                path.display()
            )));
        }
    };
    let instance = match extension {
        ExtensionPoint::Wake => {
            let function = required_function(&table, "decide", path)?;
            let key = lua.create_registry_value(function).map_err(|error| {
                PluginError::Loader(format!("{}：保存 decide 失败：{error}", path.display()))
            })?;
            LoadedInstance::Wake(Arc::new(LuaWakeProvider {
                caller: LuaCaller {
                    lua: Arc::new(Mutex::new(lua)),
                    function: key,
                    timeout: LUA_CALL_TIMEOUT,
                    failures: failures.clone(),
                    label: id.to_string(),
                },
            }))
        }
        ExtensionPoint::Access => {
            let function = required_function(&table, "decide", path)?;
            let key = lua.create_registry_value(function).map_err(|error| {
                PluginError::Loader(format!("{}：保存 decide 失败：{error}", path.display()))
            })?;
            LoadedInstance::Access(Arc::new(LuaAccessProvider {
                caller: LuaCaller {
                    lua: Arc::new(Mutex::new(lua)),
                    function: key,
                    timeout: LUA_CALL_TIMEOUT,
                    failures: failures.clone(),
                    label: id.to_string(),
                },
            }))
        }
        ExtensionPoint::Delivery => {
            let function = required_function(&table, "compose", path)?;
            let key = lua.create_registry_value(function).map_err(|error| {
                PluginError::Loader(format!("{}：保存 compose 失败：{error}", path.display()))
            })?;
            LoadedInstance::Delivery(Arc::new(LuaDeliveryComposer {
                caller: LuaCaller {
                    lua: Arc::new(Mutex::new(lua)),
                    function: key,
                    timeout: LUA_CALL_TIMEOUT,
                    failures: failures.clone(),
                    label: id.to_string(),
                },
            }))
        }
        ExtensionPoint::Send => {
            let before = optional_function(&table, "before", path)?;
            let after = optional_function(&table, "after", path)?;
            if before.is_none() && after.is_none() {
                return Err(PluginError::Loader(format!(
                    "{}：send 脚本至少要有一个 before 或 after",
                    path.display()
                )));
            }
            let before_key = before
                .map(|function| lua.create_registry_value(function))
                .transpose()
                .map_err(|error| {
                    PluginError::Loader(format!(
                        "{}：保存 before 失败：{error}",
                        path.display()
                    ))
                })?;
            let after_key = after
                .map(|function| lua.create_registry_value(function))
                .transpose()
                .map_err(|error| {
                    PluginError::Loader(format!("{}：保存 after 失败：{error}", path.display()))
                })?;
            let lua = Arc::new(Mutex::new(lua));
            let before = before_key.map(|key| LuaCaller {
                lua: lua.clone(),
                function: key,
                timeout: LUA_CALL_TIMEOUT,
                failures: failures.clone(),
                label: format!("{id}.before"),
            });
            let after = after_key.map(|key| LuaCaller {
                lua,
                function: key,
                timeout: LUA_CALL_TIMEOUT,
                failures: failures.clone(),
                label: format!("{id}.after"),
            });
            LoadedInstance::Send(Arc::new(LuaSendHook { before, after }))
        }
        ExtensionPoint::TurnError => {
            let function = required_function(&table, "decide", path)?;
            let key = lua.create_registry_value(function).map_err(|error| {
                PluginError::Loader(format!("{}：保存 decide 失败：{error}", path.display()))
            })?;
            LoadedInstance::TurnError(Arc::new(LuaTurnErrorProvider {
                caller: LuaCaller {
                    lua: Arc::new(Mutex::new(lua)),
                    function: key,
                    timeout: LUA_CALL_TIMEOUT,
                    failures: failures.clone(),
                    label: id.to_string(),
                },
            }))
        }
        ExtensionPoint::Clock => {
            let function = required_function(&table, "is_due", path)?;
            let key = lua.create_registry_value(function).map_err(|error| {
                PluginError::Loader(format!("{}：保存 is_due 失败：{error}", path.display()))
            })?;
            LoadedInstance::Clock(Arc::new(LuaClockProvider {
                caller: LuaCaller {
                    lua: Arc::new(Mutex::new(lua)),
                    function: key,
                    timeout: LUA_CALL_TIMEOUT,
                    failures: failures.clone(),
                    label: id.to_string(),
                },
            }))
        }
        ExtensionPoint::Receive => {
            let function = required_function(&table, "decide", path)?;
            let key = lua.create_registry_value(function).map_err(|error| {
                PluginError::Loader(format!("{}：保存 decide 失败：{error}", path.display()))
            })?;
            LoadedInstance::Receive(Arc::new(LuaReceiveProvider {
                caller: LuaCaller {
                    lua: Arc::new(Mutex::new(lua)),
                    function: key,
                    timeout: LUA_CALL_TIMEOUT,
                    failures: failures.clone(),
                    label: id.to_string(),
                },
            }))
        }
        _ => unreachable!("extension 已在上面筛过"),
    };
    Ok(LoadedProvider {
        id,
        extension,
        instance,
    })
}

fn required_function(table: &Table, name: &str, path: &Path) -> Result<Function, PluginError> {
    table.get::<Function>(name).map_err(|error| {
        PluginError::Loader(format!(
            "{}：缺少 {name} 函数：{error}",
            path.display()
        ))
    })
}

fn optional_function(
    table: &Table,
    name: &str,
    path: &Path,
) -> Result<Option<Function>, PluginError> {
    table.get::<Option<Function>>(name).map_err(|error| {
        PluginError::Loader(format!(
            "{}：{name} 不是函数：{error}",
            path.display()
        ))
    })
}

fn wake_options_table(lua: &Lua, options: &WakePolicy) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    table.set("provider", options.provider.to_string())?;
    table.set("at_mention", options.at_mention)?;
    table.set("reply", options.reply)?;
    table.set("name_mention", options.name_mention)?;
    table.set("names", string_list(lua, &options.names)?)?;
    table.set("accumulate", options.accumulate)?;
    table.set("event_tags", options.event_tags.clone())?;
    table.set("events", string_list(lua, &options.events)?)?;
    table.set("keywords", string_list(lua, &options.keywords)?)?;
    table.set("ignore_speakers", string_list(lua, &options.ignore_speakers)?)?;
    Ok(table)
}

fn wake_context_table(lua: &Lua, context: &WakeContext<'_>) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    table.set("session", context.session)?;
    table.set("room", context.room.to_string())?;
    table.set("connection", context.connection)?;
    table.set("platform", context.platform)?;
    table.set("sender_id", context.sender_id)?;
    table.set("text", context.text)?;
    table.set("at_self", context.at_self)?;
    table.set("reply_to_self", context.reply_to_self)?;
    table.set("event", context.event)?;
    table.set("tags", string_list(lua, context.tags)?)?;
    table.set("unread", context.unread)?;
    table.set("now", context.now)?;
    Ok(table)
}

fn access_options_table(lua: &Lua, options: &AccessPolicy) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    table.set("provider", options.provider.to_string())?;
    let rooms = lua.create_table()?;
    for (index, room) in options.rooms.iter().enumerate() {
        rooms.set(index + 1, room.to_string())?;
    }
    table.set("rooms", rooms)?;
    Ok(table)
}

fn access_request_table(lua: &Lua, request: &AccessRequest<'_>) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    table.set("session", request.session)?;
    table.set("connection", request.connection)?;
    table.set("room", request.room.map(|room| room.to_string()))?;
    table.set("action", request.action.as_str())?;
    let subscriptions = lua.create_table()?;
    for (index, room) in request.subscriptions.iter().enumerate() {
        subscriptions.set(index + 1, room.to_string())?;
    }
    table.set("subscriptions", subscriptions)?;
    Ok(table)
}

fn string_list(lua: &Lua, items: &[impl AsRef<str>]) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    for (index, item) in items.iter().enumerate() {
        table.set(index + 1, item.as_ref())?;
    }
    Ok(table)
}

fn delivery_options_table(lua: &Lua, options: &DeliveryPolicy) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    table.set("provider", options.provider.to_string())?;
    Ok(table)
}

fn delivery_request_table(lua: &Lua, request: &DeliveryRequest<'_>) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    table.set("session", request.session)?;
    table.set("room", request.room.to_string())?;
    table.set("limit", request.limit)?;
    let records = lua.create_table()?;
    for (index, record) in request.records.iter().enumerate() {
        records.set(index + 1, record_table(lua, record)?)?;
    }
    table.set("records", records)?;
    Ok(table)
}

fn record_table(lua: &Lua, record: &Record) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    table.set("id", record.id)?;
    table.set("room", record.room.to_string())?;
    table.set("at", record.at)?;
    table.set("at_self", record.at_self)?;
    table.set("reply_to_self", record.reply_to_self)?;
    table.set("redelivery", record.redelivery)?;
    table.set("note", record.note.clone())?;
    table.set("platform_id", record.platform_id.clone())?;
    table.set("text", flatten_text(&record.elements))?;

    let sender = lua.create_table()?;
    sender.set("id", record.sender.id.clone())?;
    sender.set("name", record.sender.name.clone())?;
    sender.set("role", record.sender.role.as_ref().map(role_name))?;
    table.set("sender", sender)?;

    match &record.kind {
        RecordKind::Message => {
            table.set("kind", "message")?;
            table.set("event", Option::<String>::None)?;
        }
        RecordKind::Event { event, .. } => {
            table.set("kind", "event")?;
            table.set("event", event.clone())?;
        }
    }

    table.set("elements", elements_to_table(lua, &record.elements)?)?;
    match &record.quote_ref {
        Some(quote) => {
            let quote_table = lua.create_table()?;
            quote_table.set("platform_id", quote.platform_id.clone())?;
            quote_table.set("id", quote.id)?;
            table.set("quote_ref", quote_table)?;
        }
        None => table.set("quote_ref", Value::Nil)?,
    }
    Ok(table)
}

fn role_name(role: &SenderRole) -> &'static str {
    match role {
        SenderRole::SystemAdmin => "system_admin",
        SenderRole::Owner => "owner",
        SenderRole::Admin => "admin",
    }
}

fn elements_to_table(lua: &Lua, elements: &[Element]) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    for (index, element) in elements.iter().enumerate() {
        table.set(index + 1, element_to_table(lua, element)?)?;
    }
    Ok(table)
}

fn element_to_table(lua: &Lua, element: &Element) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    match element {
        Element::Text { text } => {
            table.set("type", "text")?;
            table.set("text", text.clone())?;
        }
        Element::At { target, all } => {
            table.set("type", "at")?;
            table.set("target", target.clone())?;
            table.set("all", *all)?;
        }
        Element::Quote { message_id } => {
            table.set("type", "quote")?;
            table.set("message_id", message_id.clone())?;
        }
        Element::Image { url, alt } => {
            table.set("type", "image")?;
            table.set("url", url.clone())?;
            table.set("alt", alt.clone())?;
        }
        Element::Audio { url } => {
            table.set("type", "audio")?;
            table.set("url", url.clone())?;
        }
        Element::Video { url } => {
            table.set("type", "video")?;
            table.set("url", url.clone())?;
        }
        Element::File { url, name } => {
            table.set("type", "file")?;
            table.set("url", url.clone())?;
            table.set("name", name.clone())?;
        }
        Element::Face { platform, id } => {
            table.set("type", "face")?;
            table.set("platform", platform.clone())?;
            table.set("id", id.clone())?;
        }
        Element::Opaque { platform, payload } => {
            table.set("type", "opaque")?;
            table.set("platform", platform.clone())?;
            table.set(
                "payload_json",
                serde_json::to_string(payload).map_err(|error| {
                    mlua::Error::runtime(format!("opaque 载荷序列化失败：{error}"))
                })?,
            )?;
        }
    }
    Ok(table)
}

fn elements_from_table(table: &Table) -> mlua::Result<Vec<Element>> {
    let mut elements = Vec::new();
    for value in table.sequence_values::<Table>() {
        elements.push(element_from_table(&value?)?);
    }
    Ok(elements)
}

fn element_from_table(table: &Table) -> mlua::Result<Element> {
    let kind: String = table.get("type")?;
    Ok(match kind.as_str() {
        "text" => Element::Text {
            text: table.get::<String>("text")?,
        },
        "at" => Element::At {
            target: table.get::<Option<String>>("target")?.unwrap_or_default(),
            all: table.get::<Option<bool>>("all")?.unwrap_or(false),
        },
        "quote" => Element::Quote {
            message_id: table.get::<String>("message_id")?,
        },
        "image" => Element::Image {
            url: table.get::<String>("url")?,
            alt: table.get::<Option<String>>("alt")?,
        },
        "audio" => Element::Audio {
            url: table.get::<String>("url")?,
        },
        "video" => Element::Video {
            url: table.get::<String>("url")?,
        },
        "file" => Element::File {
            url: table.get::<String>("url")?,
            name: table.get::<String>("name")?,
        },
        "face" => Element::Face {
            platform: table.get::<String>("platform")?,
            id: table.get::<String>("id")?,
        },
        "opaque" => {
            let payload_json: String = table.get("payload_json")?;
            let payload = serde_json::from_str(&payload_json).map_err(|error| {
                mlua::Error::runtime(format!("opaque 载荷不是合法 JSON：{error}"))
            })?;
            Element::Opaque {
                platform: table.get::<String>("platform")?,
                payload,
            }
        }
        other => {
            return Err(mlua::Error::runtime(format!(
                "不认识的元素类型 {other:?}；合法值：text/at/quote/image/audio/video/file/face/opaque"
            )));
        }
    })
}

fn send_context_table(lua: &Lua, context: &SendContext<'_>) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    table.set("session", context.session)?;
    table.set("room", context.room.to_string())?;
    table.set("origin", context.origin.as_str())?;
    Ok(table)
}

fn sent_table(lua: &Lua, sent: &Sent) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    table.set("platform_id", sent.platform_id.clone())?;
    table.set("at", sent.at)?;
    Ok(table)
}

fn interpret_delivery_plan(value: Value) -> mlua::Result<DeliveryPlan> {
    let Value::Table(table) = value else {
        return Err(mlua::Error::runtime(format!(
            "delivery composer 要返回 table 或 nil，收到 {value:?}"
        )));
    };
    let keep = match table.get::<Option<Table>>("keep")? {
        Some(keep) => {
            let mut ids = Vec::new();
            for value in keep.sequence_values::<u64>() {
                ids.push(value?);
            }
            ids
        }
        None => Vec::new(),
    };
    let omitted = match table.get::<Option<Table>>("omitted")? {
        Some(omitted) => Some(Omitted {
            from: omitted.get("from")?,
            to: omitted.get("to")?,
            count: omitted.get("count")?,
        }),
        None => None,
    };
    Ok(DeliveryPlan { keep, omitted })
}

fn turn_error_request_table(
    lua: &Lua,
    request: &TurnErrorRequest<'_>,
) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    table.set("session", request.session)?;
    table.set("room", request.room.to_string())?;
    table.set("error", request.error)?;
    table.set("actions", turn_error_actions_table(lua, request.actions)?)?;
    table.set(
        "default_actions",
        turn_error_actions_table(lua, request.default_actions)?,
    )?;
    Ok(table)
}

fn turn_error_actions_table(
    lua: &Lua,
    actions: &[TurnErrorAction],
) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    for (index, action) in actions.iter().enumerate() {
        table.set(index + 1, turn_error_action_table(lua, action)?)?;
    }
    Ok(table)
}

fn turn_error_action_table(lua: &Lua, action: &TurnErrorAction) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    match action {
        TurnErrorAction::Log => {
            table.set("kind", "log")?;
        }
        TurnErrorAction::Send { room } => {
            table.set("kind", "send")?;
            table.set("room", room.to_string())?;
        }
    }
    Ok(table)
}

fn interpret_turn_error_actions(value: Value) -> mlua::Result<Vec<TurnErrorAction>> {
    let Value::Table(table) = value else {
        return Err(mlua::Error::runtime(format!(
            "turn_error provider 要返回动作数组，收到 {value:?}"
        )));
    };
    let mut actions = Vec::new();
    for value in table.sequence_values::<Table>() {
        let action = value?;
        let kind: String = action.get("kind")?;
        match kind.as_str() {
            "log" => actions.push(TurnErrorAction::Log),
            "send" => {
                let room: String = action.get("room")?;
                let room = RoomId::parse(&room).map_err(|error| {
                    mlua::Error::runtime(format!("失败通知房间不合法：{room}（{error}）"))
                })?;
                actions.push(TurnErrorAction::Send { room });
            }
            other => {
                return Err(mlua::Error::runtime(format!(
                    "不认识的失败通知动作 {other:?}（只支持 log / send）"
                )));
            }
        }
    }
    Ok(actions)
}

fn clock_options_table(lua: &Lua, options: &ClockPolicy) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    table.set("provider", options.provider.to_string())?;
    table.set("interval_secs", options.interval_secs)?;
    table.set("idle_secs", options.idle_secs)?;
    Ok(table)
}

fn clock_context_table(lua: &Lua, context: &ClockContext<'_>) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    table.set("session", context.session)?;
    table.set("room", context.room.to_string())?;
    table.set("now", context.now)?;
    table.set("last_clock_at", context.last_clock_at)?;
    table.set("last_record_at", context.last_record_at)?;
    Ok(table)
}

fn receive_request_table(lua: &Lua, request: &ReceiveRequest<'_>) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    table.set("connection", request.connection)?;
    table.set("platform", request.platform)?;
    table.set("room", request.room.to_string())?;
    table.set("text", request.text)?;
    table.set("at_self", request.at_self)?;
    let sender = lua.create_table()?;
    sender.set("id", request.sender.id.clone())?;
    sender.set("name", request.sender.name.clone())?;
    sender.set("role", request.sender.role.as_ref().map(role_name))?;
    table.set("sender", sender)?;
    match request.kind {
        RecordKind::Message => {
            table.set("kind", "message")?;
            table.set("event", Option::<String>::None)?;
        }
        RecordKind::Event { event, .. } => {
            table.set("kind", "event")?;
            table.set("event", event.clone())?;
        }
    }
    table.set("elements", elements_to_table(lua, request.elements)?)?;
    Ok(table)
}

fn interpret_wake(value: Value) -> mlua::Result<WakeVerdict> {
    match value {
        Value::Boolean(true) => Ok(WakeVerdict {
            decision: WakeDecision::Wake,
            reason: None,
        }),
        Value::Boolean(false) => Ok(WakeVerdict::stay()),
        Value::String(text) => match text.to_string_lossy().as_str() {
            "wake" => Ok(WakeVerdict {
                decision: WakeDecision::Wake,
                reason: None,
            }),
            "stay" => Ok(WakeVerdict::stay()),
            other => Err(mlua::Error::runtime(format!(
                "wake provider 要返回 \"wake\"/\"stay\"，收到 {other:?}"
            ))),
        },
        Value::Table(table) => {
            let decision: String = table.get("decision")?;
            let reason: Option<String> = table.get("reason")?;
            match decision.as_str() {
                "wake" => Ok(WakeVerdict {
                    decision: WakeDecision::Wake,
                    reason,
                }),
                "stay" => Ok(WakeVerdict::stay()),
                other => Err(mlua::Error::runtime(format!(
                    "wake provider 的 decision 要是 \"wake\"/\"stay\"，收到 {other:?}"
                ))),
            }
        }
        other => Err(mlua::Error::runtime(format!(
            "wake provider 要返回 \"wake\"/\"stay\"、布尔值或 {{decision, reason}} 表，收到 {other:?}"
        ))),
    }
}

#[cfg(test)]
mod tests;

fn interpret_access(value: Value) -> mlua::Result<AccessDecision> {
    match value {
        Value::Boolean(true) => Ok(AccessDecision::Allow),
        Value::Boolean(false) => Ok(AccessDecision::Deny),
        Value::String(text) => match text.to_string_lossy().as_str() {
            "allow" => Ok(AccessDecision::Allow),
            "deny" => Ok(AccessDecision::Deny),
            other => Err(mlua::Error::runtime(format!(
                "access provider 要返回 \"allow\"/\"deny\"，收到 {other:?}"
            ))),
        },
        other => Err(mlua::Error::runtime(format!(
            "access provider 要返回 \"allow\"/\"deny\" 或布尔值，收到 {other:?}"
        ))),
    }
}
