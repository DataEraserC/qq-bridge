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
    AdapterMessage, CoreMessage, WireMethodSpec, WireResult, decode_line, encode_line,
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
        let (outbound, launch) = spawn_process(connection, events)
            .map_err(|detail| AdapterError::Create(connection.id.clone(), detail))?;
        Ok(AdapterInstance { outbound, launch })
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
}

fn spawn_process(
    connection: &Connection,
    events: EventQueue,
) -> Result<(Box<dyn Outbound>, Box<dyn FnOnce(AdapterHost) + Send>), String> {
    let config = parse_options(connection)?;
    let connection_id = connection.id.clone();

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

    let stdin = Arc::new(Mutex::new(stdin));
    let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
    let alive = Arc::new(AtomicBool::new(true));
    let internal_stop = Arc::new(AtomicBool::new(false));

    spawn_stderr(connection_id.clone(), stderr);
    let (hello_tx, hello_rx) = mpsc::channel();
    spawn_reader(
        connection_id.clone(),
        stdout,
        events,
        Arc::clone(&pending),
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
    eprintln!(
        "适配器 {connection_id}（process）：{} 已启动（{} 个平台方法）",
        hello.name,
        hello.catalog.len()
    );

    let catalog = convert_catalog(hello.catalog).map_err(|detail| {
        stop_child(&mut child);
        format!("适配器的平台方法目录不合法：{detail}")
    })?;

    let outbound = ProcessOutbound {
        stdin: Arc::clone(&stdin),
        pending: Arc::clone(&pending),
        connection: connection_id.clone(),
        catalog,
        alive,
        seq: 0,
        timeout: REPLY_TIMEOUT,
    };

    let launch: Box<dyn FnOnce(AdapterHost) + Send> = Box::new(move |host| {
        monitor_process(
            host.shutdown,
            internal_stop,
            stdin,
            child,
            connection_id,
        );
    });

    let outbound: Box<dyn Outbound> = Box::new(outbound);
    Ok((outbound, launch))
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
                    AdapterMessage::Hello { name, catalog } => {
                        hello_seen = true;
                        if let Some(sender) = hello_tx.take() {
                            let _ = sender.send(Ok(HelloData { name, catalog }));
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

fn monitor_process(
    host_shutdown: Arc<AtomicBool>,
    internal_stop: Arc<AtomicBool>,
    stdin: Arc<Mutex<ChildStdin>>,
    mut child: Child,
    connection: String,
) {
    loop {
        if host_shutdown.load(Ordering::Relaxed) || internal_stop.load(Ordering::Relaxed) {
            break;
        }
        match child.try_wait() {
            Ok(Some(_)) => {
                internal_stop.store(true, Ordering::Relaxed);
                return;
            }
            Ok(None) => {}
            Err(_) => return,
        }
        thread::sleep(Duration::from_millis(100));
    }

    let _ = write_line(&stdin, &CoreMessage::Shutdown);
    let deadline = Instant::now() + SHUTDOWN_GRACE;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(50));
            }
            Ok(None) | Err(_) => {
                eprintln!("适配器 {connection}（process）：收尾超时，强制结束");
                let _ = child.kill();
                let _ = child.wait();
                return;
            }
        }
    }
}

fn write_line(stdin: &Mutex<ChildStdin>, message: &CoreMessage) -> Result<(), String> {
    let line = encode_line(message).map_err(|error| error.to_string())?;
    let mut stdin = stdin
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    stdin
        .write_all(format!("{line}\n").as_bytes())
        .and_then(|_| stdin.flush())
        .map_err(|error| format!("写不进适配器进程：{error}"))
}

struct ProcessOutbound {
    stdin: Arc<Mutex<ChildStdin>>,
    pending: Pending,
    connection: String,
    catalog: Arc<[MethodSpec]>,
    alive: Arc<AtomicBool>,
    seq: u64,
    timeout: Duration,
}

impl ProcessOutbound {
    fn request(&mut self, message: CoreMessage, id: u64) -> Result<WireResult, String> {
        if !self.alive.load(Ordering::Relaxed) {
            return Err("适配器进程没在跑".into());
        }

        let (reply_tx, reply_rx) = mpsc::channel();
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(id, reply_tx);

        if let Err(detail) = write_line(&self.stdin, &message) {
            self.pending
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(&id);
            self.alive.store(false, Ordering::Relaxed);
            return Err(detail);
        }

        match reply_rx.recv_timeout(self.timeout) {
            Ok(result) => result,
            Err(RecvTimeoutError::Timeout) => {
                self.pending
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
