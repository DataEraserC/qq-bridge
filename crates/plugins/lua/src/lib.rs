//! LuaManager / LuaLoader（ADR-0026 Phase 3b）。
//!
//! 一个 `.lua` 文件 = 一个 provider，文件所在目录由 `ACB_LUA_DIR` 指定。脚本返回：
//!
//! ```lua
//! return {
//!   extension = "wake",            -- wake | access
//!   decide = function(options, context)
//!     return "wake"                -- wake/stay（也可返回 true/false）
//!   end,
//! }
//! ```
//!
//! 文件名就是 provider 名，最终 id 是 `lua.<文件名>`。`options` / `context` 是只读的
//! 普通 table；脚本拿不到 Store、Bridge 或配置写入能力。每次调用最多 50ms，死循环会被
//! 指令钩子打断，按「不唤醒 / 拒绝访问」处理并记一次失败（manager 的 health 会变成
//! Degraded）。
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
    AccessDecision, AccessPolicy, AccessProvider, AccessRequest, Capability, ExtensionPoint, Health,
    LoaderKind, LoaderManifest, ManagerManifest, ManagerPlugin, PluginError, PluginLoader,
    PluginManager, ProviderDescriptor, ProviderId, ProviderInstance, ProviderOptions, ReloadScope,
    WakeContext, WakeDecision, WakePolicy, WakeProvider, PLUGIN_API_VERSION,
};
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
        let result = self.caller.call(|lua, function| {
            let options = wake_options_table(lua, options)?;
            let context = wake_context_table(lua, context)?;
            function.call::<Value>((options, context))
        });
        match result {
            Ok(value) => match interpret_wake(value) {
                Ok(decision) => decision,
                Err(error) => {
                    self.caller.record_failure(&error);
                    WakeDecision::Stay
                }
            },
            Err(error) => {
                self.caller.record_failure(&error);
                WakeDecision::Stay
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

/// 一个已经加载好的 provider：descriptor 信息和可克隆的实例。
struct LoadedProvider {
    id: ProviderId,
    extension: ExtensionPoint,
    instance: LoadedInstance,
}

enum LoadedInstance {
    Wake(Arc<dyn WakeProvider>),
    Access(Arc<dyn AccessProvider>),
}

impl LoadedProvider {
    fn provider_instance(&self) -> ProviderInstance {
        match &self.instance {
            LoadedInstance::Wake(provider) => ProviderInstance::Wake(provider.clone()),
            LoadedInstance::Access(provider) => ProviderInstance::Access(provider.clone()),
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
            vec![Capability::WakeDecision, Capability::AccessDecision],
        )
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
            vec![ExtensionPoint::Wake, ExtensionPoint::Access],
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
            point => {
                return Err(PluginError::UnsupportedExtensionPoint {
                    id: loaded.id.to_string(),
                    point,
                });
            }
        };
        descriptors.push(ProviderDescriptor::new(
            loaded.id.clone(),
            loaded.extension,
            PLUGIN_API_VERSION,
            vec![capability],
        ));
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
            "{}：缺少 extension（要写 \"wake\" 或 \"access\"）：{error}",
            path.display()
        ))
    })?;
    let extension = match extension.as_str() {
        "wake" => ExtensionPoint::Wake,
        "access" => ExtensionPoint::Access,
        other => {
            return Err(PluginError::Loader(format!(
                "{}：不支持的 extension {other:?}（只支持 \"wake\" / \"access\"）",
                path.display()
            )));
        }
    };
    let decide: Function = table.get("decide").map_err(|error| {
        PluginError::Loader(format!(
            "{}：缺少 decide 函数：{error}",
            path.display()
        ))
    })?;
    let function = lua.create_registry_value(decide).map_err(|error| {
        PluginError::Loader(format!("{}：保存 decide 失败：{error}", path.display()))
    })?;

    let caller = LuaCaller {
        lua: Arc::new(Mutex::new(lua)),
        function,
        timeout: LUA_CALL_TIMEOUT,
        failures: failures.clone(),
        label: id.to_string(),
    };
    let instance = match extension {
        ExtensionPoint::Wake => {
            LoadedInstance::Wake(Arc::new(LuaWakeProvider { caller }))
        }
        ExtensionPoint::Access => {
            LoadedInstance::Access(Arc::new(LuaAccessProvider { caller }))
        }
        _ => unreachable!("extension 已在上面筛过"),
    };
    Ok(LoadedProvider {
        id,
        extension,
        instance,
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
    table.set("poke", options.poke)?;
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

fn interpret_wake(value: Value) -> mlua::Result<WakeDecision> {
    match value {
        Value::Boolean(true) => Ok(WakeDecision::Wake),
        Value::Boolean(false) => Ok(WakeDecision::Stay),
        Value::String(text) => match text.to_string_lossy().as_str() {
            "wake" => Ok(WakeDecision::Wake),
            "stay" => Ok(WakeDecision::Stay),
            other => Err(mlua::Error::runtime(format!(
                "wake provider 要返回 \"wake\"/\"stay\"，收到 {other:?}"
            ))),
        },
        other => Err(mlua::Error::runtime(format!(
            "wake provider 要返回 \"wake\"/\"stay\" 或布尔值，收到 {other:?}"
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
