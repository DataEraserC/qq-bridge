//! qq-agent-bridge 内核可执行入口。
//!
//! 进程里有四类线程，全部是阻塞的（见 [ADR-0006]）：
//!
//! - **QQ 连接**：每账号一条，由 [`qqab_kernel::transport::spawn`] 起，事件经 `mpsc` 回主循环。
//! - **适配器接入**：每条连接一条，由 [`qqab_kernel::adapter::Server`] 起。
//! - **主循环**：本文件。收所有来源的输入、驱动 [`qqab_kernel::runtime::Kernel`]、执行出站。
//! - **时钟**：每 200 ms 往主循环投一个 `Tick`，让超时回合、在途动作与重投都能推进。
//!
//! 主循环不是 async，也不持锁：`Kernel` 只在它自己手里，并发只发生在 worker 与它之间的
//! `mpsc` 上。
//!
//! [ADR-0006]: ../../docs/adr/0006-blocking-websocket-per-thread.md

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::thread;
use std::time::Duration;

use time::OffsetDateTime;

use qqab_kernel::adapter::Server;
use qqab_kernel::archive::Limits;
use qqab_kernel::config::{Account, Config};
use qqab_kernel::conn::Pool;
use qqab_kernel::hooks::Hooks;
use qqab_kernel::persona::{Personas, base_dir};
use qqab_kernel::pipeline::Pipeline;
use qqab_kernel::runtime::{Inbound, Kernel, Outbound};
use qqab_kernel::transport::{ActionRequest, ConnectError, Connection, Handle, spawn};

/// 时钟节拍。与传输层的读超时同量级：再快只是空转。
const TICK: Duration = Duration::from_millis(200);

/// 首连失败的账号多久后重试一次。
const RECONNECT_AFTER: Duration = Duration::from_secs(5);

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--help" | "-h" | "help") => {
            println!("{}", usage());
            ExitCode::SUCCESS
        }
        _ => match run(&args) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("qqab：{error}");
                ExitCode::FAILURE
            }
        },
    }
}

fn usage() -> &'static str {
    "qq-agent-bridge 内核\n\
     \n\
     用法：qqab [config.toml]\n\
     \n\
     省略路径时依次尝试 ./config.toml 与 /etc/qqab/config.toml。\n\
     人格文件按配置文件所在目录解析。"
}

fn run(args: &[String]) -> Result<(), String> {
    let config_path = resolve_config(args.first().map(PathBuf::from))?;
    let text = std::fs::read_to_string(&config_path)
        .map_err(|error| format!("读不到配置 {}（{error}）", config_path.display()))?;

    let config = Config::parse(&text).map_err(|error| format!("配置不合法：{error}"))?;
    let base = base_dir(&config_path);
    let personas =
        Personas::load(&config, &base).map_err(|error| format!("人格加载失败：{error}"))?;

    // 钩子在启动时一次性加载：语法错当场失败，不留到运行期（见 06-hooks.md#加载时机）。
    let hooks: BTreeMap<String, Hooks> = Hooks::load_all(&config, &base)
        .map_err(|error| format!("钩子加载失败：{error}"))?
        .into_iter()
        .collect();

    let pipeline = Pipeline::new(
        Limits::default(),
        config.notes.max_entries,
        time::Duration::seconds(config.adapter.turn_timeout_sec as i64),
    )
    .with_note_budget(config.notes.digest_budget_tokens)
    .with_hooks(hooks);

    let pool = Pool::new(&config.accounts);
    let mut kernel = Kernel::new(config.clone(), personas, pipeline, pool);

    // 输入总线：所有 worker 往这里送，主循环独占读端。
    let (bus_tx, bus_rx) = channel::<Inbound>();

    // 适配器接入：内核监听，适配器连上来。
    let server = Server::bind(&config.adapter.endpoint).map_err(|error| error.to_string())?;
    let bus_for_adapter = bus_tx.clone();
    let (adapter_tx, adapter_rx) = channel::<qqab_kernel::adapter::AdapterEvent>();
    Server::spawn(server, adapter_tx);
    thread::spawn(move || {
        while let Ok(event) = adapter_rx.recv() {
            if bus_for_adapter.send(Inbound::Adapter(event)).is_err() {
                return;
            }
        }
    });

    // QQ 连接。
    let mut links: BTreeMap<String, Handle> = BTreeMap::new();
    for account in &config.accounts {
        match Connection::open(account) {
            Ok(connection) => {
                let handle = start_link(&bus_tx, account, connection);
                let _ = bus_tx.send(Inbound::LinkUp {
                    account: account.id.clone(),
                    actions: handle.actions.clone(),
                });
                links.insert(account.id.clone(), handle);
            }
            Err(error) => eprintln!(
                "账号 {}：连接失败（{}），{RECONNECT_AFTER:?} 后重试",
                account.id,
                describe_connect(&error)
            ),
        }
    }

    // 时钟。
    let tick_tx = bus_tx.clone();
    thread::spawn(move || {
        loop {
            thread::sleep(TICK);
            if tick_tx.send(Inbound::Tick).is_err() {
                return;
            }
        }
    });

    eprintln!(
        "qqab：已启动（适配器接入点 {}）",
        config.adapter.endpoint.describe()
    );
    main_loop(&mut kernel, &bus_rx, &config, &bus_tx, &mut links);
    Ok(())
}

/// 主循环：收输入、问内核、执行出站、按需重连。
fn main_loop(
    kernel: &mut Kernel,
    bus: &Receiver<Inbound>,
    config: &Config,
    bus_tx: &Sender<Inbound>,
    links: &mut BTreeMap<String, Handle>,
) {
    let mut due: BTreeMap<String, std::time::Instant> = BTreeMap::new();

    loop {
        let inbound = match bus.recv_timeout(Duration::from_millis(500)) {
            Ok(inbound) => inbound,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => {
                eprintln!("qqab：输入总线关闭，退出");
                return;
            }
        };

        let out = kernel.handle(inbound, OffsetDateTime::now_utc());
        for outbound in kernel.apply(out) {
            match outbound {
                Outbound::Act { account, request } => dispatch(links, &account, request),
                Outbound::Log { level, message } => eprintln!("qqab[{level}]：{message}"),
                // `apply` 已把它们消化掉。
                Outbound::Frame { .. } | Outbound::CloseAdapter { .. } => {}
            }
        }

        // 退避到点的账号重新建连。`retry_due` 只看状态，真正的建连在这里做。
        for account_id in kernel.pool().retry_due(OffsetDateTime::now_utc()) {
            let ready_at = due
                .entry(account_id.to_owned())
                .or_insert_with(std::time::Instant::now);
            if ready_at.elapsed() < RECONNECT_AFTER {
                continue;
            }
            due.remove(account_id);

            let Some(account) = config
                .accounts
                .iter()
                .find(|candidate| candidate.id == account_id)
            else {
                continue;
            };
            match Connection::open(account) {
                Ok(connection) => {
                    let handle = start_link(bus_tx, account, connection);
                    let _ = bus_tx.send(Inbound::LinkUp {
                        account: account.id.clone(),
                        actions: handle.actions.clone(),
                    });
                    links.insert(account.id.clone(), handle);
                }
                Err(error) => eprintln!(
                    "账号 {account_id}：重连失败（{}）",
                    describe_connect(&error)
                ),
            }
        }
    }
}

/// 把一个动作交给对应账号的连接线程。
///
/// 找不到连接就把失败写回回执通道：调用方在等一个结果，静默丢弃会让模型一直悬着。
fn dispatch(links: &BTreeMap<String, Handle>, account: &str, request: ActionRequest) {
    let Some(handle) = links.get(account) else {
        let _ = request
            .reply
            .send(Err(write_refused(format!("账号 {account} 没有可用连接"))));
        return;
    };
    if let Err(error) = handle.actions.send(request) {
        let _ = error.0.reply.send(Err(write_refused(format!(
            "账号 {account} 的连接线程已退出"
        ))));
    }
}

/// 主循环自己造的写失败：与传输层的 `WriteError` 同一个类型。
fn write_refused(reason: String) -> qqab_kernel::transport::WriteError {
    qqab_kernel::transport::WriteError { reason }
}

/// 起一条 QQ 连接线程，把它的入站事件接到总线上。
fn start_link(bus: &Sender<Inbound>, account: &Account, connection: Connection) -> Handle {
    let forward = bus.clone();
    let forward_id = account.id.clone();
    spawn(account.clone(), connection, move |incoming| {
        let _ = forward.send(Inbound::Qq {
            account: forward_id.clone(),
            incoming,
        });
    })
}

/// 找到配置文件。显式路径优先，否则按约定位置依次尝试。
fn resolve_config(explicit: Option<PathBuf>) -> Result<PathBuf, String> {
    if let Some(path) = explicit {
        return Ok(path);
    }
    for candidate in ["config.toml", "/etc/qqab/config.toml"] {
        let path = PathBuf::from(candidate);
        if path.is_file() {
            return Ok(path);
        }
    }
    Err("找不到配置文件：给一个路径，或把 config.toml 放在当前目录".to_owned())
}

fn describe_connect(error: &ConnectError) -> String {
    match error {
        ConnectError::BadUrl { url, .. } => format!("地址无效：{url}"),
        ConnectError::TokenEnv(name) => format!("环境变量 {name} 未设置"),
        ConnectError::BadToken => "令牌无法放进 HTTP 头".to_owned(),
        ConnectError::Handshake { url, detail } => format!("握手失败：{url}（{detail}）"),
    }
}
