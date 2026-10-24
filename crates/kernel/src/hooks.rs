//! 钩子宿主：每设备一个内嵌 Lua 5.4 环境。
//!
//! 落地 `docs/architecture/06-hooks.md` 的三个挂载点——`preReceive`、`postReceive`、
//! `shouldWake`。三条约束在这里实现：
//!
//! - **脚本是配置，不是插件。** 只从 `config.toml` 引用的文件读，不接受别的来源；读不出来
//!   或语法错即启动失败，不留到运行期。
//! - **失败偏向保留。** `preReceive` 抛错时事件照常进归档；`shouldWake` 抛错时按规则判定
//!   继续。脚本 bug 的代价应该是噪音，不是失联（[不变量八]）。
//! - **环境里没有副作用。** 不加载 `io`、`package`、`debug`；`os` 只留 `time`/`date`；
//!   `load`/`loadfile`/`dofile`/`require` 全部去掉。加上指令预算与墙钟超时，意外死循环不会
//!   拖住主循环。
//!
//! [不变量八]: ../../docs/architecture/07-invariants.md

use std::cell::{Cell, RefCell};
use std::fmt;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use mlua::debug::Debug;
use mlua::{Function, HookTriggers, Lua, LuaOptions, StdLib, Table, Value, VmState};

use crate::archive::Message;
use crate::config::Config;

/// VM 指令钩子的触发间隔。预算按「每这么多条指令算一次」计数。
const HOOK_STRIDE: u64 = 1_000;

/// 钩子执行的资源上限。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HookLimits {
    /// 单次调用的墙钟上限。到点即抛错，按该位置的失败方向处理。
    pub timeout: Duration,
    /// 单次调用的 VM 指令预算。纯 `while true do end` 的兜底。
    pub instructions: u64,
    /// 单个 Lua 环境的内存上限。
    pub memory_bytes: usize,
}

impl Default for HookLimits {
    fn default() -> Self {
        // 文档写的默认超时是 100ms。指令预算与内存上限是同一件事的另一面：正常判定逻辑
        // 是几十行，两万条指令已经宽到不会误伤。
        Self {
            timeout: Duration::from_millis(100),
            instructions: 20_000,
            memory_bytes: 4 << 20,
        }
    }
}

impl HookLimits {
    /// 指令预算折算成的钩子调用次数。
    fn calls(&self) -> u64 {
        (self.instructions / HOOK_STRIDE).max(1)
    }
}

/// 传给钩子的事件视图。字段与文档里的 `ev.sender_id` 等一一对应。
pub struct HookEvent<'a> {
    pub sender_id: &'a str,
    pub group_id: &'a str,
    pub text: &'a str,
    /// Unix 毫秒。
    pub time_ms: i64,
    /// 发送者属于设备声明的账号集合。**只读**：钩子改不动它。
    pub is_self: bool,
    /// 本批归档的条数。新消息是 1，重复到达是 0，拍一拍没有可归档的内容也是 0。
    pub batch: usize,
    pub oldest_ms: i64,
    pub newest_ms: i64,
}

impl<'a> HookEvent<'a> {
    /// 一条已归档的消息。
    pub fn of_message(message: &'a Message, fresh: bool) -> Self {
        Self {
            sender_id: &message.sender_id,
            group_id: &message.group,
            text: &message.text,
            time_ms: message.at,
            is_self: message.own,
            batch: usize::from(fresh),
            oldest_ms: message.at,
            newest_ms: message.at,
        }
    }

    /// 一次拍一拍。它没有正文也没有归档增量。
    pub fn of_poke(group: &'a str, sender_id: &'a str, time_ms: i64) -> Self {
        Self {
            sender_id,
            group_id: group,
            text: "",
            time_ms,
            is_self: false,
            batch: 0,
            oldest_ms: time_ms,
            newest_ms: time_ms,
        }
    }
}

/// 事件视图的**自有**副本。
///
/// 管线在归档前后各要调一次钩子，而归档借用了消息。拿一份快照就能避开借用冲突，
/// 并且让「归档后的事件」与「归档前的事件」是同一份数据，只有 `batch` 不同。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub sender_id: String,
    pub group_id: String,
    pub text: String,
    pub time_ms: i64,
    pub is_self: bool,
    pub batch: usize,
    pub oldest_ms: i64,
    pub newest_ms: i64,
}

impl Snapshot {
    pub fn of_message(message: &Message, fresh: bool) -> Self {
        Self {
            sender_id: message.sender_id.clone(),
            group_id: message.group.clone(),
            text: message.text.clone(),
            time_ms: message.at,
            is_self: message.own,
            batch: usize::from(fresh),
            oldest_ms: message.at,
            newest_ms: message.at,
        }
    }

    pub fn of_poke(group: &str, sender_id: &str, time_ms: i64) -> Self {
        Self {
            sender_id: sender_id.to_owned(),
            group_id: group.to_owned(),
            text: String::new(),
            time_ms,
            is_self: false,
            batch: 0,
            oldest_ms: time_ms,
            newest_ms: time_ms,
        }
    }

    /// 借成调用期需要的那份视图。
    pub fn view(&self) -> HookEvent<'_> {
        HookEvent {
            sender_id: &self.sender_id,
            group_id: &self.group_id,
            text: &self.text,
            time_ms: self.time_ms,
            is_self: self.is_self,
            batch: self.batch,
            oldest_ms: self.oldest_ms,
            newest_ms: self.newest_ms,
        }
    }
}

/// `preReceive` 的结论。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Ruling {
    /// 丢弃：不归档、不判定、不唤醒。
    pub dropped: bool,
    /// 改写后的正文。`None` 表示脚本没动它。
    pub text: Option<String>,
    /// 脚本附加的标签，只在本次事件的后续判定里可见。
    pub tags: Vec<String>,
}

impl Ruling {
    /// 原样放行。
    pub fn keep() -> Self {
        Self::default()
    }
}

/// 一条钩子日志。`device` 由宿主补上，脚本只给级别与正文。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookLog {
    pub device: String,
    pub level: String,
    pub message: String,
}

/// 钩子运行时。
///
/// 内核只认识这三个挂载点与它们的输入输出；**怎么执行**是运行时的事。
///
/// - 解释器内（现在的 [`Hooks`]）：Lua，隔离靠不加载 IO 库，超时靠 VM 指令钩子。
/// - 子进程：任何语言，隔离靠进程边界，超时靠 kill。测得常驻子进程逐行 IPC 约
///   0.031 ms，每次重起进程约 12.4 ms——所以子进程实现必须常驻，不能一调用一起。
/// - WASM：隔离靠虚拟机，超时靠燃料计量。
///
/// 这三种的隔离模型完全不同，所以抽象只放**接口**，不放实现策略。见
/// [ADR-0010](../../../docs/adr/0010-hook-runtime-is-an-interface.md)。
///
/// **不要求 `Send`。** 钩子全部在主循环线程上同步执行——[ADR-0006] 的阻塞模型决定了
/// 没有第二个线程会碰它。要求 `Send` 会立刻把 `mlua::Lua` 排除在外（它不是 `Send`），
/// 而那是个假的约束：真要有线程要碰钩子，问题就不在 trait 上。
///
/// [ADR-0006]: ../../docs/adr/0006-blocking-websocket-per-thread.md
pub trait HookRuntime {
    /// 设备 id。日志与诊断要用。
    fn device(&self) -> &str;

    /// `preReceive`：身份过滤之后、归档之前。
    fn pre_receive(&mut self, event: &HookEvent<'_>) -> Ruling;

    /// `postReceive`：归档之后、判定之前。返回值被忽略。
    fn post_receive(&mut self, event: &HookEvent<'_>);

    /// `shouldWake`：判定的最终决定。
    ///
    /// `Ok(None)` 是「否决」，`Ok(Some(reason))` 是「唤醒并给出原因」，`Err` 是脚本失败——
    /// 调用方据此按「失败偏向保留」的方向退回规则结论（不变量八）。
    fn should_wake(
        &mut self,
        event: &HookEvent<'_>,
        tags: &[String],
        reason: Option<&str>,
    ) -> Result<Option<String>, HookError>;

    /// 取走攒下的日志。
    fn drain_logs(&self) -> Vec<HookLog>;
}

/// 一个设备的钩子环境。
pub struct Hooks {
    lua: Lua,
    device: String,
    limits: HookLimits,
    /// 本次调用剩余可触发的钩子次数。到 0 即抛错。
    budget: Arc<AtomicU64>,
    /// 本次调用的墙钟截止时刻（Unix 毫秒）。0 表示不设。
    deadline: Arc<AtomicI64>,
    logs: Rc<RefCell<Vec<HookLog>>>,
}

impl fmt::Debug for Hooks {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // `Lua` 不是 `Debug`；打印身份与上限足够定位。
        f.debug_struct("Hooks")
            .field("device", &self.device)
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

impl Hooks {
    /// 读脚本并建环境。
    pub fn load(device: &str, path: &Path, limits: HookLimits) -> Result<Self, HookError> {
        let source = std::fs::read_to_string(path).map_err(|source| HookError::Read {
            device: device.to_owned(),
            path: path.display().to_string(),
            detail: source.to_string(),
        })?;
        Self::from_source(device, &source, &path.display().to_string(), limits)
    }

    /// 建环境并执行脚本正文。语法错在这里暴露，不留到运行期。
    pub fn from_source(
        device: &str,
        source: &str,
        name: &str,
        limits: HookLimits,
    ) -> Result<Self, HookError> {
        Self::build(device, source, name, limits, None)
    }

    /// 建环境并执行脚本正文，外带只读配置快照。
    pub fn from_source_with_config(
        device: &str,
        source: &str,
        name: &str,
        limits: HookLimits,
        config: &Config,
    ) -> Result<Self, HookError> {
        Self::build(device, source, name, limits, Some(config))
    }

    fn build(
        device: &str,
        source: &str,
        name: &str,
        limits: HookLimits,
        config: Option<&Config>,
    ) -> Result<Self, HookError> {
        let lua = Lua::new_with(
            // `_G` 由 mlua 无条件打开；这里只加无副作用的那几个。
            StdLib::COROUTINE
                | StdLib::TABLE
                | StdLib::STRING
                | StdLib::UTF8
                | StdLib::MATH
                | StdLib::OS,
            LuaOptions::default(),
        )
        .map_err(|error| env_error(device, error))?;

        lua.set_memory_limit(limits.memory_bytes)
            .map_err(|error| env_error(device, error))?;

        let logs: Rc<RefCell<Vec<HookLog>>> = Rc::new(RefCell::new(Vec::new()));
        // 随机源只被宿主函数的闭包持有：`Lua` 的注册表保它活着。
        let rng = Rc::new(Cell::new(seed_for(device)));

        seal(&lua, device)?;
        install_host_functions(&lua, device, &logs, &rng)?;
        if let Some(config) = config {
            install_config(&lua, config, device)?;
        }

        let budget = Arc::new(AtomicU64::new(limits.calls()));
        let deadline = Arc::new(AtomicI64::new(0));
        lua.set_global_hook(
            HookTriggers::new().every_nth_instruction(HOOK_STRIDE as u32),
            watch(Arc::clone(&budget), Arc::clone(&deadline)),
        )
        .map_err(|error| env_error(device, error))?;

        lua.load(source)
            .set_name(name)
            .exec()
            .map_err(|error| HookError::Syntax {
                device: device.to_owned(),
                path: name.to_owned(),
                detail: error.to_string(),
            })?;

        Ok(Self {
            lua,
            device: device.to_owned(),
            limits,
            budget,
            deadline,
            logs,
        })
    }

    /// 读脚本并建环境，外带配置快照。
    pub fn load_with_config(
        device: &str,
        path: &Path,
        limits: HookLimits,
        config: &Config,
    ) -> Result<Self, HookError> {
        let source = std::fs::read_to_string(path).map_err(|source| HookError::Read {
            device: device.to_owned(),
            path: path.display().to_string(),
            detail: source.to_string(),
        })?;
        Self::from_source_with_config(device, &source, &path.display().to_string(), limits, config)
    }

    /// 按配置加载全部设备的脚本。没有声明 `hooks` 的设备不出现在结果里。
    pub fn load_all(config: &Config, base: &Path) -> Result<Vec<(String, Hooks)>, HookError> {
        let mut out = Vec::new();
        for device in &config.devices {
            let Some(relative) = &device.hooks else {
                continue;
            };
            let path = base.join(relative);
            out.push((
                device.id.clone(),
                Hooks::load_with_config(&device.id, &path, HookLimits::default(), config)?,
            ));
        }
        Ok(out)
    }

    pub fn device(&self) -> &str {
        &self.device
    }

    pub fn limits(&self) -> HookLimits {
        self.limits
    }

    /// 取走攒下的日志。宿主每次处理后调用一次，交给日志出口。
    pub fn drain_logs(&self) -> Vec<HookLog> {
        std::mem::take(&mut self.logs.borrow_mut())
    }

    /// `preReceive`：身份过滤之后、归档之前。
    ///
    /// 脚本抛错时按**保留**处理——返回 [`Ruling::keep`] 并记一条日志。丢消息的代价远比多处理
    /// 一次高。
    pub fn pre_receive(&mut self, event: &HookEvent<'_>) -> Ruling {
        let Some(function) = self.function("preReceive") else {
            return Ruling::keep();
        };

        let ev = match self.event_table(event) {
            Ok(ev) => ev,
            Err(error) => return self.salvage_with("preReceive", error),
        };

        self.arm();
        let result = function.call::<Option<Table>>(ev);
        self.disarm();
        let reply = match result {
            Ok(Some(reply)) => reply,
            // 返回 nil：丢弃。文档只给了这一种「丢弃」写法。
            Ok(None) => {
                return Ruling {
                    dropped: true,
                    ..Ruling::keep()
                };
            }
            Err(error) => {
                self.record("warn", format!("preReceive 抛错，事件按保留处理：{error}"));
                return Ruling::keep();
            }
        };

        let text = self.read_string(&reply, "text");
        let tags = self.read_tags(&reply);
        match (text, tags) {
            (Ok(text), Ok(tags)) => Ruling {
                dropped: false,
                // 脚本没改正文时 `text` 与原文相同，不该被当成一次改写。
                text: text.filter(|value| value != event.text),
                tags,
            },
            (Err(error), _) | (_, Err(error)) => self.salvage_with("preReceive", error),
        }
    }

    /// `postReceive`：归档之后、判定之前。返回值被忽略，只让它维护跨事件状态。
    pub fn post_receive(&mut self, event: &HookEvent<'_>) {
        let Some(function) = self.function("postReceive") else {
            return;
        };
        let (ev, batch) = match (self.event_table(event), self.batch_table(event)) {
            (Ok(ev), Ok(batch)) => (ev, batch),
            (Err(error), _) | (_, Err(error)) => {
                self.record("warn", format!("postReceive 参数构造失败：{error}"));
                return;
            }
        };

        self.arm();
        let result = function.call::<()>((ev, batch));
        self.disarm();
        if let Err(error) = result {
            self.record("warn", format!("postReceive 抛错，忽略：{error}"));
        }
    }

    /// `shouldWake`：唤醒判定的最终决定。
    ///
    /// `reason` 是规则算出的原因标签，可能为空。返回 `None` 表示**不唤醒**——既能否决规则，
    /// 也能在规则没命中时自行判定为唤醒。
    ///
    /// 抛错时不否决：返回 `Err`，由调用方按规则的原结论继续。
    pub fn should_wake(
        &mut self,
        event: &HookEvent<'_>,
        tags: &[String],
        reason: Option<&str>,
    ) -> Result<Option<String>, HookError> {
        let Some(function) = self.function("shouldWake") else {
            return Ok(reason.map(str::to_owned));
        };

        let ev = self.event_table(event)?;
        if !tags.is_empty() {
            let table = self
                .lua
                .create_sequence_from(tags.to_vec())
                .map_err(|error| self.runtime(error))?;
            ev.set("tags", table).map_err(|error| self.runtime(error))?;
        }
        let reason = match reason {
            Some(label) => Value::String(
                self.lua
                    .create_string(label)
                    .map_err(|error| self.runtime(error))?,
            ),
            None => Value::Nil,
        };

        self.arm();
        let result = function.call::<Value>((ev, reason));
        self.disarm();
        let value = result.map_err(|error| self.runtime(error))?;

        match value {
            Value::Nil => Ok(None),
            Value::String(text) => Ok(Some(
                text.to_str()
                    .map_err(|error| self.runtime(error))?
                    .to_string(),
            )),
            other => Err(self.runtime(mlua::Error::runtime(format!(
                "shouldWake 必须返回字符串或 nil，得到 {}",
                other.type_name()
            )))),
        }
    }

    /// 脚本是否定义了该钩子。没定义不是错误——不是每个脚本都要三个。
    fn function(&self, name: &str) -> Option<Function> {
        self.lua
            .globals()
            .get::<Option<Function>>(name)
            .ok()
            .flatten()
    }

    fn event_table(&self, event: &HookEvent<'_>) -> Result<Table, HookError> {
        let table = self.lua.create_table().map_err(|e| self.runtime(e))?;
        table
            .set(
                "sender_id",
                self.lua
                    .create_string(event.sender_id)
                    .map_err(|e| self.runtime(e))?,
            )
            .map_err(|e| self.runtime(e))?;
        table
            .set(
                "group_id",
                self.lua
                    .create_string(event.group_id)
                    .map_err(|e| self.runtime(e))?,
            )
            .map_err(|e| self.runtime(e))?;
        table
            .set(
                "text",
                self.lua
                    .create_string(event.text)
                    .map_err(|e| self.runtime(e))?,
            )
            .map_err(|e| self.runtime(e))?;
        table
            .set("time", event.time_ms)
            .map_err(|e| self.runtime(e))?;
        table
            .set("is_self", event.is_self)
            .map_err(|e| self.runtime(e))?;
        Ok(table)
    }

    fn batch_table(&self, event: &HookEvent<'_>) -> Result<Table, HookError> {
        let table = self.lua.create_table().map_err(|e| self.runtime(e))?;
        table
            .set("count", event.batch as u64)
            .map_err(|e| self.runtime(e))?;
        table
            .set("oldest_at", event.oldest_ms)
            .map_err(|e| self.runtime(e))?;
        table
            .set("newest_at", event.newest_ms)
            .map_err(|e| self.runtime(e))?;
        Ok(table)
    }

    fn read_string(&self, table: &Table, key: &str) -> Result<Option<String>, HookError> {
        table
            .get::<Option<String>>(key)
            .map_err(|error| self.runtime(error))
    }

    fn read_tags(&self, table: &Table) -> Result<Vec<String>, HookError> {
        let Some(inner) = table
            .get::<Option<Table>>("tags")
            .map_err(|error| self.runtime(error))?
        else {
            return Ok(Vec::new());
        };

        let mut out = Vec::new();
        for value in inner.sequence_values::<Value>() {
            let value = value.map_err(|error| self.runtime(error))?;
            if let Value::String(text) = value {
                out.push(
                    text.to_str()
                        .map_err(|error| self.runtime(error))?
                        .to_string(),
                );
            }
        }
        Ok(out)
    }

    /// 设置本次调用的预算与截止时刻。
    fn arm(&self) {
        self.budget.store(self.limits.calls(), Ordering::SeqCst);
        let deadline = now_ms().saturating_add(self.limits.timeout.as_millis() as i64);
        self.deadline.store(deadline, Ordering::SeqCst);
    }

    fn disarm(&self) {
        self.deadline.store(0, Ordering::SeqCst);
    }

    fn salvage_with(&mut self, hook: &str, error: HookError) -> Ruling {
        self.record("warn", format!("{hook} 失败，事件按保留处理：{error}"));
        Ruling::keep()
    }

    fn record(&self, level: &str, message: String) {
        self.logs.borrow_mut().push(HookLog {
            device: self.device.clone(),
            level: level.to_owned(),
            message,
        });
    }

    fn runtime(&self, source: mlua::Error) -> HookError {
        HookError::Runtime {
            device: self.device.clone(),
            detail: source.to_string(),
        }
    }
}

impl HookRuntime for Hooks {
    fn device(&self) -> &str {
        &self.device
    }

    fn pre_receive(&mut self, event: &HookEvent<'_>) -> Ruling {
        Hooks::pre_receive(self, event)
    }

    fn post_receive(&mut self, event: &HookEvent<'_>) {
        Hooks::post_receive(self, event)
    }

    fn should_wake(
        &mut self,
        event: &HookEvent<'_>,
        tags: &[String],
        reason: Option<&str>,
    ) -> Result<Option<String>, HookError> {
        Hooks::should_wake(self, event, tags, reason)
    }

    fn drain_logs(&self) -> Vec<HookLog> {
        Hooks::drain_logs(self)
    }
}

/// 只留纯函数：去掉一切能做 IO 或重新编译源码的入口。
fn seal(lua: &Lua, device: &str) -> Result<(), HookError> {
    let globals = lua.globals();
    for name in ["load", "loadfile", "dofile", "require", "collectgarbage"] {
        globals
            .set(name, Value::Nil)
            .map_err(|error| env_error(device, error))?;
    }

    // `os` 只留时间：`execute`/`remove`/`rename`/`exit`/`getenv` 都是副作用或信息泄漏。
    if let Ok(os) = globals.get::<Table>("os") {
        for name in [
            "execute",
            "remove",
            "rename",
            "exit",
            "getenv",
            "tmpname",
            "setlocale",
        ] {
            os.set(name, Value::Nil)
                .map_err(|error| env_error(device, error))?;
        }
    }

    Ok(())
}

/// 注入三个宿主函数：`log`、`now`、`random`。
fn install_host_functions(
    lua: &Lua,
    device: &str,
    logs: &Rc<RefCell<Vec<HookLog>>>,
    rng: &Rc<Cell<u64>>,
) -> Result<(), HookError> {
    let sink = Rc::clone(logs);
    let owner = device.to_owned();
    lua.globals()
        .set(
            "log",
            lua.create_function(move |_, (level, message): (String, String)| {
                sink.borrow_mut().push(HookLog {
                    device: owner.clone(),
                    level,
                    message,
                });
                Ok(())
            })
            .map_err(|error| env_error(device, error))?,
        )
        .map_err(|error| env_error(device, error))?;

    lua.globals()
        .set(
            "now",
            lua.create_function(|_, ()| Ok(now_ms()))
                .map_err(|error| env_error(device, error))?,
        )
        .map_err(|error| env_error(device, error))?;

    // 确定性随机源：跨进程可重放比真随机重要——排查「它为什么说话」时能复现。
    let source = Rc::clone(rng);
    lua.globals()
        .set(
            "random",
            lua.create_function(move |_, ()| {
                let mut state = source.get();
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                source.set(state);
                Ok((state >> 11) as f64 / (1u64 << 53) as f64)
            })
            .map_err(|error| env_error(device, error))?,
        )
        .map_err(|error| env_error(device, error))?;

    Ok(())
}

/// VM 指令钩子：预算耗尽或超过截止时刻即抛错。
///
/// 抛错会顺着 Lua 调用栈回到 `Function::call`，变成一次普通的脚本失败，按该位置的失败
/// 方向处理。这样死循环只是一个「慢了一点的失败」，不是主循环卡死。
fn watch(
    budget: Arc<AtomicU64>,
    deadline: Arc<AtomicI64>,
) -> impl Fn(&Lua, &Debug) -> mlua::Result<VmState> + 'static {
    move |_lua, _debug| {
        let limit = deadline.load(Ordering::Relaxed);
        if limit != 0 && now_ms() >= limit {
            return Err(mlua::Error::runtime("钩子执行超时"));
        }
        let remaining = budget.load(Ordering::Relaxed);
        if remaining == 0 {
            return Err(mlua::Error::runtime("钩子执行超时"));
        }
        budget.store(remaining - 1, Ordering::Relaxed);
        Ok(VmState::Continue)
    }
}

fn env_error(device: &str, source: mlua::Error) -> HookError {
    HookError::Environment {
        device: device.to_owned(),
        detail: source.to_string(),
    }
}

/// 注入只读配置快照。
///
/// 文档说环境里有「该设备的只读配置快照」，脚本据此按群/账号分岔。表是深冻结的：
/// `table.freeze` 在 Lua 5.4 存在，改它当场抛错，而不是悄悄改掉一份只在本次调用有效的副本。
fn install_config(lua: &Lua, config: &Config, device_id: &str) -> Result<(), HookError> {
    // 读只读包装器的实现：它是一段固定 Lua 代码，装一次。
    lua.globals()
        .set(
            "__qqab_readonly",
            lua.load(READONLY_WRAPPER)
                .set_name("readonly.lua")
                .eval::<Function>()
                .map_err(|e| env_error(device_id, e))?,
        )
        .map_err(|e| env_error(device_id, e))?;

    let Some(device) = config.devices.iter().find(|device| device.id == device_id) else {
        // 配置里没有这台设备：脚本拿不到快照也不该崩，那是调用方的配置错误。
        return Ok(());
    };

    let snapshot = lua.create_table().map_err(|e| env_error(device_id, e))?;
    let put = |key: &str, value: Value| snapshot.set(key, value);

    put(
        "id",
        Value::String(
            lua.create_string(&device.id)
                .map_err(|e| env_error(device_id, e))?,
        ),
    )
    .map_err(|e| env_error(device_id, e))?;
    put(
        "memory",
        Value::String(
            lua.create_string(&device.memory)
                .map_err(|e| env_error(device_id, e))?,
        ),
    )
    .map_err(|e| env_error(device_id, e))?;
    put(
        "persona",
        Value::String(
            lua.create_string(&device.persona)
                .map_err(|e| env_error(device_id, e))?,
        ),
    )
    .map_err(|e| env_error(device_id, e))?;
    put(
        "accounts",
        Value::Table(
            lua.create_sequence_from(device.accounts.clone())
                .map_err(|e| env_error(device_id, e))?,
        ),
    )
    .map_err(|e| env_error(device_id, e))?;

    let scopes = lua.create_table().map_err(|e| env_error(device_id, e))?;
    for (index, scope) in device.scopes.iter().enumerate() {
        let entry = lua.create_table().map_err(|e| env_error(device_id, e))?;
        entry
            .set(
                "group",
                lua.create_string(&scope.group)
                    .map_err(|e| env_error(device_id, e))?,
            )
            .map_err(|e| env_error(device_id, e))?;
        entry
            .set(
                "receive",
                lua.create_sequence_from(scope.receive.clone())
                    .map_err(|e| env_error(device_id, e))?,
            )
            .map_err(|e| env_error(device_id, e))?;
        entry
            .set(
                "send",
                lua.create_sequence_from(scope.send.clone())
                    .map_err(|e| env_error(device_id, e))?,
            )
            .map_err(|e| env_error(device_id, e))?;
        scopes
            .set(index + 1, entry)
            .map_err(|e| env_error(device_id, e))?;
    }
    put("scopes", Value::Table(scopes)).map_err(|e| env_error(device_id, e))?;

    // 包成只读视图：`guard` 是 Lua 侧实现的深拷贝 + 代理，嵌套表一起处理。
    let guard: Function = lua
        .globals()
        .get::<Function>("__qqab_readonly")
        .map_err(|e| env_error(device_id, e))?;
    let guarded = guard
        .call::<Table>(snapshot)
        .map_err(|e| env_error(device_id, e))?;
    lua.globals()
        .set("config", guarded)
        .map_err(|e| env_error(device_id, e))?;
    Ok(())
}

/// 只读视图的 Lua 实现。
///
/// Lua 5.4 **没有** `table.freeze`（那是 Luau 的扩展）。用代理表拿到同样的效果：
/// `__index` 转发读、`__newindex` 拒绝写、`__pairs` 转发遍历。`rawset` 能绕过它，但脚本
/// 作者是部署者本人，目标是防止误改而不是防恶意——见 ADR-0003 的威胁模型。
const READONLY_WRAPPER: &str = r#"
-- 每一层都必须是代理：只在顶层拦截时，`config.scopes[1].group = "9"` 改的是影子副本。
-- 影子表本身不保护任何东西；protected 的是外面这层代理。
local function readonly(t)
  local shadow = {}
  for k, v in pairs(t) do
    if type(v) == "table" then
      shadow[k] = readonly(v)
    else
      shadow[k] = v
    end
  end
  return setmetatable({}, {
    __index = shadow,
    __newindex = function() error("配置快照是只读的", 2) end,
    -- pairs 在 5.4 认 __pairs；不设的话遍历到的是空代理表本身。
    __pairs = function() return next, shadow, nil end,
    __metatable = "read-only",
  })
end
return readonly
"#;

fn seed_for(device: &str) -> u64 {
    // 固定基 + 设备名散列：同一份配置每次启动得到同一串随机数。
    let mut hash = 0xCBF2_9CE4_8422_2325u64;
    for byte in device.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
    }
    hash | 1
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as i64)
}

/// 钩子加载或执行失败。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookError {
    /// 脚本文件读不出来。
    Read {
        device: String,
        path: String,
        detail: String,
    },
    /// 语法错。启动失败，不在运行期暴露。
    Syntax {
        device: String,
        path: String,
        detail: String,
    },
    /// 环境建不起来（内存上限、宿主函数注入）。
    Environment { device: String, detail: String },
    /// 运行期失败。已被各钩子位置按失败方向处理，只在需要上报的地方出现。
    Runtime { device: String, detail: String },
}

impl fmt::Display for HookError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read {
                device,
                path,
                detail,
            } => write!(f, "设备 {device} 的钩子文件读不出来：{path}（{detail}）"),
            Self::Syntax {
                device,
                path,
                detail,
            } => write!(f, "设备 {device} 的钩子有语法错：{path}（{detail}）"),
            Self::Environment { device, detail } => {
                write!(f, "设备 {device} 的钩子环境建不起来：{detail}")
            }
            Self::Runtime { device, detail } => {
                write!(f, "设备 {device} 的钩子执行失败：{detail}")
            }
        }
    }
}

impl std::error::Error for HookError {}

/// 配置里声明的脚本路径。给诊断与测试用。
pub fn script_path(device: &crate::config::Device, base: &Path) -> Option<PathBuf> {
    device.hooks.as_ref().map(|relative| base.join(relative))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event<'a>(text: &'a str, sender: &'a str) -> HookEvent<'a> {
        HookEvent {
            sender_id: sender,
            group_id: "20001",
            text,
            time_ms: 1_792_561_540_000,
            is_self: false,
            batch: 1,
            oldest_ms: 1_792_561_540_000,
            newest_ms: 1_792_561_540_000,
        }
    }

    fn hooks(source: &str) -> Hooks {
        Hooks::from_source("alice-phone", source, "test.lua", HookLimits::default())
            .expect("脚本应能加载")
    }

    #[test]
    fn missing_script_is_a_load_error_not_a_silent_noop() {
        let error = Hooks::load(
            "alice-phone",
            Path::new("/nonexistent-dir/hooks.lua"),
            HookLimits::default(),
        )
        .unwrap_err();
        assert!(matches!(error, HookError::Read { .. }), "得到 {error:?}");
    }

    #[test]
    fn syntax_error_fails_at_load_time() {
        let error = Hooks::from_source(
            "alice-phone",
            "function preReceive(ev",
            "bad.lua",
            HookLimits::default(),
        )
        .unwrap_err();
        assert!(matches!(error, HookError::Syntax { .. }), "得到 {error:?}");
    }

    #[test]
    fn absent_hooks_leave_the_event_alone() {
        let mut hooks = hooks("local x = 1");
        assert_eq!(hooks.pre_receive(&event("在吗", "30002")), Ruling::keep());
        assert_eq!(
            hooks.should_wake(&event("在吗", "30002"), &[], Some("at_mention")),
            Ok(Some("at_mention".to_owned()))
        );
    }

    #[test]
    fn pre_receive_can_drop_an_event() {
        let mut hooks = hooks("function preReceive(ev) return nil end");
        assert!(hooks.pre_receive(&event("", "30002")).dropped);
    }

    #[test]
    fn pre_receive_can_rewrite_text() {
        let mut hooks = hooks(
            r#"function preReceive(ev)
                 ev.text = ev.text .. "!"
                 return ev
               end"#,
        );
        let ruling = hooks.pre_receive(&event("在吗", "30002"));
        assert_eq!(ruling.text.as_deref(), Some("在吗!"));
        assert!(!ruling.dropped);
    }

    #[test]
    fn unchanged_text_is_not_reported_as_a_rewrite() {
        let mut hooks = hooks("function preReceive(ev) return ev end");
        assert_eq!(hooks.pre_receive(&event("在吗", "30002")).text, None);
    }

    #[test]
    fn tags_from_pre_receive_reach_should_wake() {
        let mut hooks = hooks(
            r#"function preReceive(ev)
                 ev.tags = { "noisy" }
                 return ev
               end
               function shouldWake(ev, reason)
                 if ev.tags and ev.tags[1] == "noisy" then return nil end
                 return reason
               end"#,
        );
        let ruling = hooks.pre_receive(&event("在吗", "30002"));
        assert_eq!(ruling.tags, vec!["noisy".to_owned()]);

        let decided = hooks.should_wake(&event("在吗", "30002"), &ruling.tags, Some("at_mention"));
        assert_eq!(decided, Ok(None), "标签应能在 shouldWake 里否决");
    }

    #[test]
    fn should_wake_can_veto_a_rule_hit() {
        let mut hooks = hooks("function shouldWake(ev, reason) return nil end");
        assert_eq!(
            hooks.should_wake(&event("在吗", "30002"), &[], Some("any_message")),
            Ok(None)
        );
    }

    #[test]
    fn should_wake_can_wake_when_rules_stayed() {
        let mut hooks = hooks(
            r#"function shouldWake(ev, reason)
                 if reason == nil and ev.text:find("上线") then return "keyword:上线" end
                 return reason
               end"#,
        );
        assert_eq!(
            hooks.should_wake(&event("我上线了", "30002"), &[], None),
            Ok(Some("keyword:上线".to_owned()))
        );
    }

    #[test]
    fn should_wake_can_rewrite_the_reason() {
        let mut hooks = hooks(
            r#"function shouldWake(ev, reason)
                 if reason == "any_message" then return "custom:quiet" end
                 return reason
               end"#,
        );
        assert_eq!(
            hooks.should_wake(&event("嗯", "30002"), &[], Some("any_message")),
            Ok(Some("custom:quiet".to_owned()))
        );
    }

    #[test]
    fn should_wake_returning_a_number_is_an_error_not_a_wake() {
        let mut hooks = hooks("function shouldWake(ev, reason) return 42 end");
        let error = hooks
            .should_wake(&event("嗯", "30002"), &[], None)
            .unwrap_err();
        assert!(matches!(error, HookError::Runtime { .. }), "得到 {error:?}");
    }

    #[test]
    fn pre_receive_error_keeps_the_event() {
        let mut hooks = hooks("function preReceive(ev) error('boom') end");
        let ruling = hooks.pre_receive(&event("在吗", "30002"));
        assert!(!ruling.dropped, "脚本 bug 不该丢消息");
        assert!(
            hooks
                .drain_logs()
                .iter()
                .any(|log| log.message.contains("boom"))
        );
    }

    #[test]
    fn should_wake_error_does_not_veto() {
        let mut hooks = hooks("function shouldWake(ev, reason) error('boom') end");
        assert!(
            hooks
                .should_wake(&event("在吗", "30002"), &[], Some("at_mention"))
                .is_err(),
            "运行期失败应可上报，由调用方按规则结论继续"
        );
    }

    #[test]
    fn post_receive_error_is_only_a_log_entry() {
        let mut hooks = hooks("function postReceive(ev, batch) error('boom') end");
        hooks.post_receive(&event("在吗", "30002"));
        let logs = hooks.drain_logs();
        assert_eq!(logs.len(), 1);
        assert!(logs[0].message.contains("boom"));
    }

    #[test]
    fn state_persists_across_calls() {
        let mut hooks = hooks(
            r#"local seen = 0
               function postReceive(ev, batch) seen = seen + batch.count end
               function shouldWake(ev, reason) return tostring(seen) end"#,
        );
        hooks.post_receive(&event("一", "30002"));
        hooks.post_receive(&event("二", "30002"));
        assert_eq!(
            hooks.should_wake(&event("三", "30002"), &[], None),
            Ok(Some("2".to_owned()))
        );
    }

    #[test]
    fn sandbox_has_no_side_effecting_entry_points() {
        let mut hooks = hooks(
            r#"function shouldWake(ev, reason)
                 local missing = {}
                 for _, name in ipairs({"io", "require", "load", "loadfile", "dofile", "package", "debug", "collectgarbage"}) do
                   if _G[name] ~= nil then table.insert(missing, name) end
                 end
                 for _, name in ipairs({"execute", "remove", "rename", "exit", "getenv"}) do
                   if os[name] ~= nil then table.insert(missing, "os." .. name) end
                 end
                 if os.time == nil or os.date == nil then table.insert(missing, "os.time/date 缺失") end
                 return table.concat(missing, ",")
               end"#,
        );
        assert_eq!(
            hooks.should_wake(&event("嗯", "30002"), &[], None),
            Ok(Some(String::new())),
            "沙箱里不该有任何副作用入口"
        );
    }

    #[test]
    fn infinite_loop_times_out_instead_of_hanging() {
        let limits = HookLimits {
            timeout: Duration::from_millis(50),
            instructions: 2_000,
            ..HookLimits::default()
        };
        let mut hooks = Hooks::from_source(
            "alice-phone",
            "function shouldWake(ev, reason) while true do end end",
            "loop.lua",
            limits,
        )
        .expect("脚本应能加载");

        let started = std::time::Instant::now();
        assert!(
            hooks.should_wake(&event("嗯", "30002"), &[], None).is_err(),
            "死循环应抛错"
        );
        assert!(
            started.elapsed() < Duration::from_millis(2_000),
            "不该拖住主循环，实际 {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn host_log_is_collected_with_the_device_name() {
        let mut hooks = hooks(r#"function preReceive(ev) log("info", "看到一条") return ev end"#);
        let _ = hooks.pre_receive(&event("在吗", "30002"));
        assert_eq!(
            hooks.drain_logs(),
            vec![HookLog {
                device: "alice-phone".to_owned(),
                level: "info".to_owned(),
                message: "看到一条".to_owned(),
            }]
        );
    }

    #[test]
    fn now_reports_unix_milliseconds() {
        let mut hooks =
            hooks("function shouldWake(ev, reason) return tostring(now() > 1600000000000) end");
        assert_eq!(
            hooks.should_wake(&event("嗯", "30002"), &[], None),
            Ok(Some("true".to_owned()))
        );
    }

    #[test]
    fn config_snapshot_is_readable_and_frozen() {
        let config = crate::config::Config::parse(
            r#"
[[accounts]]
id = "main"
self_id = "10001"
ws_url = "ws://127.0.0.1:6700"

[[personas]]
id = "whale"
text = "你是……"

[[devices]]
id = "alice-phone"
persona = "whale"
memory = "alice"
accounts = ["main"]

  [[devices.scopes]]
  group = "20001"
  receive = ["main"]
  send = ["main"]
"#,
        )
        .expect("配置应能解析");

        let mut hooks = Hooks::from_source_with_config(
            "alice-phone",
            r#"function shouldWake(ev, reason)
                 return string.format("%s/%s/%s/%s",
                   config.memory, config.scopes[1].group, config.accounts[1], config.id)
               end"#,
            "config.lua",
            HookLimits::default(),
            &config,
        )
        .expect("脚本应能加载");

        assert_eq!(
            hooks.should_wake(&event("嗯", "30002"), &[], None),
            Ok(Some("alice/20001/main/alice-phone".to_owned()))
        );
    }

    #[test]
    fn config_snapshot_cannot_be_mutated() {
        let config = crate::config::Config::parse(
            r#"
[[accounts]]
id = "main"
self_id = "10001"
ws_url = "ws://127.0.0.1:6700"

[[personas]]
id = "whale"
text = "你是……"

[[devices]]
id = "alice-phone"
persona = "whale"
memory = "alice"
accounts = ["main"]

  [[devices.scopes]]
  group = "20001"
  receive = ["main"]
  send = ["main"]
"#,
        )
        .expect("配置应能解析");

        // 改顶层、改嵌套表、改数组元素都该抛错——否则「只读快照」只是口头承诺。
        for attempt in [
            r#"config.memory = "hacked""#,
            r#"config.scopes[1].group = "9""#,
            r#"config.accounts[1] = "9""#,
        ] {
            let source = format!("function shouldWake(ev, reason) {attempt} return reason end");
            let mut hooks = Hooks::from_source_with_config(
                "alice-phone",
                &source,
                "mutate.lua",
                HookLimits::default(),
                &config,
            )
            .expect("脚本语法应通过");
            assert!(
                hooks.should_wake(&event("嗯", "30002"), &[], None).is_err(),
                "改只读快照应抛错：{attempt}"
            );
        }
    }

    #[test]
    fn without_config_there_is_no_config_global() {
        let mut hooks = hooks("function shouldWake(ev, reason) return tostring(config == nil) end");
        assert_eq!(
            hooks.should_wake(&event("嗯", "30002"), &[], None),
            Ok(Some("true".to_owned()))
        );
    }

    #[test]
    fn random_is_deterministic_per_device_and_between_runs() {
        let source = "function shouldWake(ev, reason) return string.format('%.10f', random()) end";
        let mut first = hooks(source);
        let mut second = hooks(source);
        let a = first.should_wake(&event("嗯", "30002"), &[], None).unwrap();
        let b = second
            .should_wake(&event("嗯", "30002"), &[], None)
            .unwrap();
        assert_eq!(a, b, "同一份配置每次启动应得到同一串随机数");
    }

    #[test]
    fn random_stays_in_the_unit_interval() {
        let mut hooks = hooks(
            r#"function shouldWake(ev, reason)
                 local ok = true
                 for i = 1, 64 do
                   local value = random()
                   if value < 0 or value >= 1 then ok = false end
                 end
                 return tostring(ok)
               end"#,
        );
        assert_eq!(
            hooks.should_wake(&event("嗯", "30002"), &[], None),
            Ok(Some("true".to_owned()))
        );
    }
}
