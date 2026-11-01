//! 外置进程平台适配器（ADR-0027 Phase 3）。
//!
//! 桥按连接配置里的 `options.command` 启动一个子进程，双方在 stdin/stdout 上
//! 一行一条 JSON（见 `acb-adapter-api`）。适配器实现可以用任何语言写：
//!
//! ```text
//! core ──Send/Call/Shutdown──▶ 子进程
//! core ◀──Hello/Event/Reply── 子进程
//! ```
//!
//! 这条连接对 core 来说和其它适配器完全一样：一个 `Outbound` + 一个 `launch`
//! 闭包。进程崩溃不会带倒桥；之后的发送/调用会明确返回「通道不可用」。
//!
//! 配置形状：
//!
//! ```toml
//! [[connections]]
//! id = "demo"
//! platform = "demo"
//! kind = "process"
//! transport = "process"
//! account = "bot"
//!
//! [connections.options]
//! command = ["/path/to/adapter", "--stdio"]
//! # 可选：env = { KEY = "value" }，cwd = "/path"，startup_timeout_secs = 10
//! ```

use std::borrow::Cow;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use acb_adapter_api::{
    AdapterMessage, CoreMessage, WireAdapterInfo, WireMethodSpec, WireResult, decode_line,
    encode_line,
};
use acb_kernel::wire::{call_error, convert_catalog, send_error};
use acb_kernel::{
    AdapterCapability, AdapterError, AdapterFactory, AdapterHost, AdapterInstance,
    AdapterManifest, CallError, CallOutcome, Connection, Element, EventQueue, MethodSpec,
    Outbound, OutboundError, OutgoingRecord, PluginEffect, PluginMetadata, RoomId, Sent,
    ADAPTER_API_VERSION,
};

/// 等子进程回一条动作结果的上限。
pub const REPLY_TIMEOUT: Duration = Duration::from_secs(10);
/// 子进程启动后必须在这个时间内说 hello。
pub const DEFAULT_STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
/// 子进程退出后的重启退避：从 500ms 起翻倍，最多 10 秒。
pub const RESTART_DELAY: Duration = Duration::from_millis(500);
pub const MAX_RESTART_DELAY: Duration = Duration::from_secs(10);
/// 退出时给子进程收尾的时间，过了就 kill。
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

static TRANSPORTS: &[Cow<'static, str>] = &[Cow::Borrowed("process")];
static CAPABILITIES: &[AdapterCapability] = &[
    AdapterCapability::ReceiveEvents,
    AdapterCapability::Send,
    AdapterCapability::Catalog,
];

/// 这条适配器的静态声明。平台名由连接配置里的 `platform` 决定，
/// 这里只声明「外置进程」这个载体类型。
pub static MANIFEST: AdapterManifest = AdapterManifest {
    id: Cow::Borrowed("acb.adapter.process"),
    platform: Cow::Borrowed("external"),
    kind: Cow::Borrowed("process"),
    transports: Cow::Borrowed(TRANSPORTS),
    api_version: ADAPTER_API_VERSION,
    capabilities: Cow::Borrowed(CAPABILITIES),
    // 子进程自己决定发什么事件、打什么标签；载体只负责把 at_self / reply_to_self 原样带进来。
    events: Cow::Borrowed(&[]),
    event_tags: Cow::Borrowed(&[]),
    supports_at: true,
    supports_reply: true,
    metadata: PluginMetadata {
        title: Some(Cow::Borrowed("外置进程适配器")),
        summary: Some(Cow::Borrowed("按连接的 options 启动子进程，stdio JSON 行协议")),
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

/// 外置进程适配器工厂。
#[derive(Debug, Default)]
pub struct ProcessAdapterFactory;

impl AdapterFactory for ProcessAdapterFactory {
    fn manifest(&self) -> AdapterManifest {
        MANIFEST.clone()
    }

    fn create(
        &self,
        connection: &Connection,
        events: EventQueue,
    ) -> Result<AdapterInstance, AdapterError> {
        let (outbound, launch, manifest) = spawn_process(connection, events)
            .map_err(|detail| AdapterError::Create(connection.id.clone(), detail))?;
        Ok(AdapterInstance {
            outbound,
            launch,
            manifest,
        })
    }
}

struct ProcessConfig {
    program: String,
    args: Vec<String>,
    env: Vec<(String, String)>,
    cwd: Option<String>,
    startup_timeout: Duration,
}

fn parse_options(connection: &Connection) -> Result<ProcessConfig, String> {
    let options = connection
        .options
        .as_object()
        .ok_or("connections.options 要给一个对象（map）")?;

    let command = options
        .get("command")
        .ok_or("options.command 要给启动命令（字符串或 [程序, 参数...]）")?;
    let (program, mut args) = match command {
        serde_json::Value::String(program) => {
            let program = program.trim();
            if program.is_empty() {
                return Err("options.command 不能为空".into());
            }
            (program.to_owned(), Vec::new())
        }
        serde_json::Value::Array(items) => {
            let mut parts = items.iter().map(|item| {
                item.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| "options.command 数组里只能放字符串".to_owned())
            });
            let program = parts
                .next()
                .transpose()?
                .ok_or("options.command 数组不能为空")?;
            let rest = parts.collect::<Result<Vec<_>, _>>()?;
            if program.trim().is_empty() {
                return Err("options.command 的第一个元素不能为空".into());
            }
            (program, rest)
        }
        _ => return Err("options.command 要给字符串或字符串数组".into()),
    };

    if let Some(extra) = options.get("args") {
        let items = extra
            .as_array()
            .ok_or("options.args 要给字符串数组")?;
        for item in items {
            args.push(
                item.as_str()
                    .ok_or("options.args 里只能放字符串")?
                    .to_owned(),
            );
        }
    }

    let mut env = Vec::new();
    if let Some(table) = options.get("env") {
        let table = table.as_object().ok_or("options.env 要给对象（map）")?;
        for (key, value) in table {
            env.push((
                key.clone(),
                value
                    .as_str()
                    .ok_or_else(|| format!("options.env.{key} 要是字符串"))?
                    .to_owned(),
            ));
        }
    }

    let cwd = match options.get("cwd") {
        Some(value) => Some(
            value
                .as_str()
                .ok_or("options.cwd 要给字符串")?
                .to_owned(),
        ),
        None => None,
    };

    let startup_timeout = options
        .get("startup_timeout_secs")
        .and_then(serde_json::Value::as_u64)
        .filter(|seconds| *seconds > 0)
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_STARTUP_TIMEOUT);

    Ok(ProcessConfig {
        program,
        args,
        env,
        cwd,
        startup_timeout,
    })
}

struct HelloData {
    name: String,
    catalog: Vec<WireMethodSpec>,
    info: WireAdapterInfo,
}

/// 一条连接的共享进程状态。重启时换 stdin、清 pending，但 outbound 句柄不变。
struct SharedProcess {
    stdin: Mutex<Option<ChildStdin>>,
    pending: Pending,
    alive: Arc<AtomicBool>,
    catalog: Mutex<Arc<[MethodSpec]>>,
    /// 子进程 hello 里声明的入站能力；重启时跟着新 hello 更新。
    info: Mutex<WireAdapterInfo>,
}

fn spawn_process(
    connection: &Connection,
    events: EventQueue,
) -> Result<
    (
        Box<dyn Outbound>,
        Box<dyn FnOnce(AdapterHost) + Send>,
        AdapterManifest,
    ),
    String,
> {
    let config = parse_options(connection)?;
    let connection_id = connection.id.clone();
    let restart_events = events.clone();

    let shared = Arc::new(SharedProcess {
        stdin: Mutex::new(None),
        pending: Arc::new(Mutex::new(HashMap::new())),
        alive: Arc::new(AtomicBool::new(false)),
        catalog: Mutex::new(Arc::from(Vec::<MethodSpec>::new())),
        info: Mutex::new(WireAdapterInfo::default()),
    });

    let (child, stdin, catalog, info) = start_child(
        &config,
        &connection_id,
        events,
        Arc::clone(&shared.pending),
        Arc::clone(&shared.alive),
    )?;
    *shared
        .stdin
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(stdin);
    *shared
        .catalog
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = catalog;
    *shared
        .info
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = info;
    shared.alive.store(true, Ordering::Relaxed);

    // 在把 shared 交给 launch 闭包之前算好运行时报文。
    let manifest = runtime_manifest(&connection.platform, &shared);

    let outbound = ProcessOutbound {
        shared: Arc::clone(&shared),
        connection: connection_id.clone(),
        seq: 0,
        timeout: REPLY_TIMEOUT,
    };

    let launch: Box<dyn FnOnce(AdapterHost) + Send> = Box::new(move |host| {
        supervise(
            host.shutdown,
            config,
            connection_id,
            shared,
            restart_events,
            child,
        );
    });

    let outbound: Box<dyn Outbound> = Box::new(outbound);
    Ok((outbound, launch, manifest))
}

/// 静态 process manifest + 子进程 hello 里声明的入站能力。
fn runtime_manifest(platform: &str, shared: &SharedProcess) -> AdapterManifest {
    let info = shared
        .info
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    let events: Vec<Cow<'static, str>> = info.events.into_iter().map(Cow::Owned).collect();
    let event_tags: Vec<Cow<'static, str>> =
        info.event_tags.into_iter().map(Cow::Owned).collect();
    AdapterManifest {
        platform: Cow::Owned(platform.to_owned()),
        events: events.into(),
        event_tags: event_tags.into(),
        supports_at: info.supports_at,
        supports_reply: info.supports_reply,
        ..MANIFEST.clone()
    }
}

/// 启动一个子进程并等到 hello。失败时把已经起的进程收掉，不留孤儿。
fn start_child(
    config: &ProcessConfig,
    connection: &str,
    events: EventQueue,
    pending: Pending,
    alive: Arc<AtomicBool>,
) -> Result<(Child, ChildStdin, Arc<[MethodSpec]>, WireAdapterInfo), String> {
    let mut command = Command::new(&config.program);
    command
        .args(&config.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in &config.env {
        command.env(key, value);
    }
    if let Some(cwd) = &config.cwd {
        command.current_dir(cwd);
    }

    let mut child = command
        .spawn()
        .map_err(|error| format!("启动 {} 失败：{error}", config.program))?;
    let stdin = child
        .stdin
        .take()
        .ok_or("子进程没有 stdin（启动方式不对）")?;
    let stdout = child
        .stdout
        .take()
        .ok_or("子进程没有 stdout（启动方式不对）")?;
    let stderr = child
        .stderr
        .take()
        .ok_or("子进程没有 stderr（启动方式不对）")?;

    spawn_stderr(connection.to_owned(), stderr);
    let (hello_tx, hello_rx) = mpsc::channel();
    spawn_reader(
        connection.to_owned(),
        stdout,
        events,
        pending,
        Arc::clone(&alive),
        hello_tx,
    );

    let hello = match hello_rx.recv_timeout(config.startup_timeout) {
        Ok(Ok(hello)) => hello,
        Ok(Err(detail)) => {
            stop_child(&mut child);
            return Err(detail);
        }
        Err(RecvTimeoutError::Timeout) => {
            stop_child(&mut child);
            return Err(format!(
                "等子进程说 hello 超时（{} 秒）",
                config.startup_timeout.as_secs()
            ));
        }
        Err(RecvTimeoutError::Disconnected) => {
            stop_child(&mut child);
            return Err("子进程没说 hello 就退出了".into());
        }
    };
    // hello 刚说完就退出的进程不能算启动成功，否则第一次请求会写进死管道。
    if let Ok(Some(status)) = child.try_wait() {
        stop_child(&mut child);
        return Err(format!("子进程说完 hello 就退出了（{status}）"));
    }
    eprintln!(
        "适配器 {connection}（process）：{} 已启动（{} 个平台方法）",
        hello.name,
        hello.catalog.len()
    );

    let catalog = convert_catalog(hello.catalog).map_err(|detail| {
        stop_child(&mut child);
        format!("适配器的平台方法目录不合法：{detail}")
    })?;

    Ok((child, stdin, catalog, hello.info))
}

/// 守着子进程：退出就按退避重启，桥要停才收尾。
fn supervise(
    host_shutdown: Arc<AtomicBool>,
    config: ProcessConfig,
    connection: String,
    shared: Arc<SharedProcess>,
    events: EventQueue,
    mut child: Child,
) {
    let mut delay = RESTART_DELAY;
    loop {
        let shutdown = wait_for_exit(&host_shutdown, &shared.stdin, &mut child, &connection);
        shared.alive.store(false, Ordering::Relaxed);
        *shared
            .stdin
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        let waiting = std::mem::take(
            &mut *shared
                .pending
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        );
        for (_, reply) in waiting {
            let _ = reply.send(Err("适配器进程退出了".into()));
        }
        if shutdown {
            return;
        }
        let _ = child.wait();

        eprintln!(
            "适配器 {connection}（process）：子进程退出，{} 毫秒后重启",
            delay.as_millis()
        );
        let wake = Instant::now() + delay;
        while Instant::now() < wake {
            if host_shutdown.load(Ordering::Relaxed) {
                return;
            }
            thread::sleep(Duration::from_millis(100));
        }

        match start_child(
            &config,
            &connection,
            events.clone(),
            Arc::clone(&shared.pending),
            Arc::clone(&shared.alive),
        ) {
            Ok((new_child, stdin, catalog, info)) => {
                *shared
                    .stdin
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(stdin);
                *shared
                    .catalog
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = catalog;
                *shared
                    .info
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = info;
                shared.alive.store(true, Ordering::Relaxed);
                child = new_child;
                delay = RESTART_DELAY;
            }
            Err(detail) => {
                eprintln!("适配器 {connection}（process）：重启失败：{detail}");
                delay = (delay * 2).min(MAX_RESTART_DELAY);
            }
        }
    }
}

type Pending = Arc<Mutex<HashMap<u64, Sender<Result<WireResult, String>>>>>;

fn spawn_stderr(connection: String, stderr: std::process::ChildStderr) {
    thread::spawn(move || {
        let reader = BufReader::new(stderr);
        for line in reader.lines().map_while(Result::ok) {
            eprintln!("适配器 {connection}（process stderr）：{line}");
        }
    });
}

fn spawn_reader(
    connection: String,
    stdout: std::process::ChildStdout,
    events: EventQueue,
    pending: Pending,
    alive: Arc<AtomicBool>,
    hello_tx: Sender<Result<HelloData, String>>,
) {
    thread::spawn(move || {
        let reader = BufReader::new(stdout);
        let mut hello_tx = Some(hello_tx);
        let mut hello_seen = false;

        for line in reader.lines() {
            let line = match line {
                Ok(line) => line,
                Err(error) => {
                    eprintln!("适配器 {connection}：读子进程输出失败：{error}");
                    break;
                }
            };
            if line.trim().is_empty() {
                continue;
            }

            let message = match decode_line::<AdapterMessage>(&line) {
                Ok(message) => message,
                Err(error) => {
                    eprintln!("适配器 {connection}：收到读不懂的 wire 消息：{error}");
                    continue;
                }
            };

            if !hello_seen {
                match message {
                    AdapterMessage::Hello {
                        name,
                        catalog,
                        info,
                    } => {
                        hello_seen = true;
                        if let Some(sender) = hello_tx.take() {
                            let _ = sender.send(Ok(HelloData {
                                name,
                                catalog,
                                info,
                            }));
                        }
                    }
                    _ => {
                        if let Some(sender) = hello_tx.take() {
                            let _ = sender.send(Err(
                                "子进程的第一行必须是 hello".to_owned(),
                            ));
                        }
                        return;
                    }
                }
                continue;
            }

            match message {
                AdapterMessage::Hello { .. } => {
                    eprintln!("适配器 {connection}：重复的 hello，忽略");
                }
                AdapterMessage::Event { record } => {
                    if !events.submit(record.into()) {
                        eprintln!("适配器 {connection}：入站队列已关闭，丢掉一条记录");
                    }
                }
                AdapterMessage::Reply { id, result } => {
                    let reply = pending
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .remove(&id);
                    if let Some(reply) = reply {
                        let _ = reply.send(Ok(result));
                    }
                }
                AdapterMessage::Log { level, message } => {
                    eprintln!("适配器 {connection}（{level}）：{message}");
                }
            }
        }

        // 子进程没了：所有等回话的请求都明确失败，不让 core 无限挂着。
        alive.store(false, Ordering::Relaxed);
        let waiting = std::mem::take(
            &mut *pending
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        );
        for (_, reply) in waiting {
            let _ = reply.send(Err("适配器进程退出了".into()));
        }
        if !hello_seen {
            if let Some(sender) = hello_tx.take() {
                let _ = sender.send(Err("子进程没说 hello 就退出了".into()));
            }
        }
    });
}

fn stop_child(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// 等子进程退出；桥要停时先发 shutdown，超时就 kill。返回 true 表示是桥要停。
fn wait_for_exit(
    host_shutdown: &AtomicBool,
    stdin: &Mutex<Option<ChildStdin>>,
    child: &mut Child,
    connection: &str,
) -> bool {
    loop {
        if host_shutdown.load(Ordering::Relaxed) {
            let _ = write_line(stdin, &CoreMessage::Shutdown);
            let deadline = Instant::now() + SHUTDOWN_GRACE;
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => return true,
                    Ok(None) if Instant::now() < deadline => {
                        thread::sleep(Duration::from_millis(50));
                    }
                    Ok(None) | Err(_) => {
                        eprintln!("适配器 {connection}（process）：收尾超时，强制结束");
                        let _ = child.kill();
                        let _ = child.wait();
                        return true;
                    }
                }
            }
        }
        match child.try_wait() {
            Ok(Some(_)) => return false,
            Ok(None) => {}
            Err(_) => return false,
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn write_line(stdin: &Mutex<Option<ChildStdin>>, message: &CoreMessage) -> Result<(), String> {
    let line = encode_line(message).map_err(|error| error.to_string())?;
    let mut stdin = stdin
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let stdin = stdin
        .as_mut()
        .ok_or_else(|| "适配器进程没在跑".to_owned())?;
    stdin
        .write_all(format!("{line}\n").as_bytes())
        .and_then(|_| stdin.flush())
        .map_err(|error| format!("写不进适配器进程：{error}"))
}

struct ProcessOutbound {
    shared: Arc<SharedProcess>,
    connection: String,
    seq: u64,
    timeout: Duration,
}

impl ProcessOutbound {
    fn request(&mut self, message: CoreMessage, id: u64) -> Result<WireResult, String> {
        if !self.shared.alive.load(Ordering::Relaxed) {
            return Err("适配器进程没在跑".into());
        }

        let (reply_tx, reply_rx) = mpsc::channel();
        self.shared
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(id, reply_tx);

        if let Err(detail) = write_line(&self.shared.stdin, &message) {
            self.shared
                .pending
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(&id);
            self.shared.alive.store(false, Ordering::Relaxed);
            return Err(detail);
        }

        match reply_rx.recv_timeout(self.timeout) {
            Ok(result) => result,
            Err(RecvTimeoutError::Timeout) => {
                self.shared
                    .pending
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .remove(&id);
                Err("等适配器回话超时".into())
            }
            Err(RecvTimeoutError::Disconnected) => Err("适配器进程退出了".into()),
        }
    }
}

impl Outbound for ProcessOutbound {
    fn send(&mut self, room: &RoomId, elements: &[Element]) -> Result<Sent, OutboundError> {
        if room.connection != self.connection {
            return Err(OutboundError::Unavailable(format!(
                "这条连接叫 {}，收到 {}",
                self.connection, room.connection
            )));
        }

        self.seq += 1;
        let id = self.seq;
        let result = self
            .request(
                CoreMessage::Send {
                    id,
                    room: room.clone(),
                    elements: elements.to_vec(),
                },
                id,
            )
            .map_err(OutboundError::Unavailable)?;

        match result {
            WireResult::Sent { platform_id, at } => Ok(Sent { platform_id, at }),
            WireResult::Error { error } => Err(send_error(error)),
            _ => Err(OutboundError::Unavailable(
                "适配器回执不是发送结果".into(),
            )),
        }
    }

    fn catalog(&self, _connection: &str) -> Arc<[MethodSpec]> {
        Arc::clone(
            &self
                .shared
                .catalog
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        )
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
        let id = self.seq;
        let result = self
            .request(
                CoreMessage::Call {
                    id,
                    room: room.clone(),
                    method: method.to_owned(),
                    params: params.clone(),
                },
                id,
            )
            .map_err(CallError::Unavailable)?;

        match result {
            WireResult::Call { value, record } => {
                let record = record
                    .map(OutgoingRecord::try_from)
                    .transpose()
                    .map_err(CallError::Unavailable)?;
                Ok(CallOutcome { value, record })
            }
            WireResult::Error { error } => Err(call_error(error)),
            _ => Err(CallError::Unavailable(
                "适配器回执不是调用结果".into(),
            )),
        }
    }
}
