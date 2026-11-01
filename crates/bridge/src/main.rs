//! 桥的入口：读配置、接平台、等 harness。
//!
//! 启动顺序：
//! 1. 校验配置（不对就在启动时死掉，不带病运行）；可容忍的冲突只警告，停三秒再继续；
//! 2. 打开状态库（流水、水位、笔记、投递计数都在里面，重启接着用）；
//! 3. 给每条连接配一个适配器，拿到「发送端口」；
//! 4. 建桥（配置 + 存储 + 一组发送端口）；
//! 5. 每条连接起一个守护线程，连不上就等一会儿再连；
//! 6. 起一个清理线程，定时削旧流水（只削读过的）；
//! 7. 起接入点，等 harness 连进来。

use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use acb_adapter_onebot11::OneBotAdapterFactory;
use acb_kernel::{
    AdapterRegistry, Bridge, Config, ControlRegistry, EventQueue, FileRelay, Outbounds,
    PluginManager, SqliteStore, drain_events, serve,
};
use acb_plugin_lua::LuaLoader;
use acb_plugin_trusted::TrustedLoader;

mod config_patch;
mod console;
use console::Console;

/// 清理线程隔多久扫一次。来源：实现默认——桥不是数据库，5 分钟一次足够，
/// 真需要更勤/更懒再做成配置项。
const PRUNE_EVERY: Duration = Duration::from_secs(300);

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("起不来：{error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let options = Options::parse(std::env::args().skip(1))?;

    let text = std::fs::read_to_string(&options.config)?;
    let config = Config::parse(&text)?;
    print_summary(&config, &options.config);

    // 有些配置是用户故意这么写的（两个会话订同一个房间）。不拦，但要说清楚它意味着什么，
    // 并且停三秒——刚启动时人还在看屏幕。
    let warnings = config.warnings();
    if !warnings.is_empty() {
        for warning in &warnings {
            eprintln!("注意：{warning}");
        }
        eprintln!("这些是你配的，就按这样跑。三秒后继续（Ctrl-C 可以停）。");
        std::thread::sleep(Duration::from_secs(3));
    }

    if options.check_only {
        println!("配置没问题（只检查，没有启动）");
        return Ok(());
    }

    // 先开库：库打不开就别往下走。宁可起不来，也别用内存悄悄跑起来——
    // 那样它看着一切正常，直到你重启才发现什么都没留下。
    let store = SqliteStore::open(&config.storage.path)?;
    let counts = store.counts()?;
    println!(
        "状态库：{}（已有 {} 条记录、{} 个房间、{} 条笔记）",
        config.storage.path, counts.records, counts.rooms, counts.notes
    );

    let shutdown = Arc::new(AtomicBool::new(false));

    // 每条连接先占一个发送口，桥建好之后再起守护线程去连。
    // 适配器由注册表按 kind 实例化；main 不再知道 onebot11 的构造细节。
    let mut adapters = AdapterRegistry::<SqliteStore>::new();
    adapters.register(std::sync::Arc::new(OneBotAdapterFactory))?;

    // 入站队列：适配器只往里丢记录，core 的 drain 线程统一 ingest。
    let (events, event_rx) = EventQueue::channel();

    let mut outbounds = Outbounds::new();
    let mut launches = Vec::new();
    for connection in &config.connections {
        let instance = adapters.create(connection, events.clone())?;
        outbounds = outbounds.with(connection.id.clone(), instance.outbound);
        launches.push(instance.launch);
    }

    // 扩展宿主：先装内置 provider，再装树外 manager（ADR-0026 Phase 3）。
    // 树外 manager 只是把 provider 注册进 Root；配置里引用 `trusted.*` 才生效。
    let mut plugins = PluginManager::with_native_builtins();
    plugins.install_loader(&TrustedLoader)?;
    // Lua 脚本插件是可选能力：设了 ACB_LUA_DIR 才装，目录里的 *.lua 变成 lua.* provider。
    if let Some(lua) = LuaLoader::from_env() {
        plugins.install_loader(&lua)?;
    }

    let bridge = Arc::new(Mutex::new(Bridge::with_plugins(
        config.clone(),
        store,
        Box::new(outbounds),
        plugins,
    )));

    // provider id 写错要在启动时死，不能等第一轮才报。
    {
        let guard = bridge
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.validate_plugins()?;
    }

    // core 的入站 drain 线程：所有适配器的事件都从这里进流水，顺序由队列保证。
    {
        let bridge = Arc::clone(&bridge);
        let shutdown = Arc::clone(&shutdown);
        std::thread::spawn(move || drain_events(bridge, event_rx, shutdown));
    }

    for launch in launches {
        let bridge = Arc::clone(&bridge);
        let shutdown = Arc::clone(&shutdown);
        std::thread::spawn(move || launch(bridge, shutdown));
    }

    // 控制指令注册表：控制台靠它把 control 帧推给管理某会话的 harness 连接。
    let controls = ControlRegistry::new();

    match &config.endpoint {
        Some(endpoint) => {
            let listener = std::net::TcpListener::bind(&endpoint.listen)?;
            let bridge = Arc::clone(&bridge);
            let shutdown = Arc::clone(&shutdown);
            let controls = controls.clone();
            std::thread::spawn(move || {
                if let Err(error) = serve(listener, bridge, shutdown, controls) {
                    eprintln!("接入点停了：{error}");
                }
            });
            println!("接入点已监听 {}", endpoint.listen);
        }
        None => println!("没有配 [endpoint]，harness 连不进来"),
    }

    // 控制台：可远程访问的管理 HTTP 服务（ADR-0018）。
    match &config.console {
        Some(console) => {
            let listener = std::net::TcpListener::bind(&console.listen)?;
            let console_service = Arc::new(Console::new(
                Arc::clone(&bridge),
                controls,
                console.token.clone(),
                options.config.clone(),
            ));
            let shutdown = Arc::clone(&shutdown);
            std::thread::spawn(move || {
                Console::serve(listener, console_service, shutdown);
            });
            println!("控制台已监听 {}（token 鉴权）", console.listen);
        }
        None => println!("没有配 [console]，控制台不起"),
    }

    // 文件中继：平台在别的文件系统命名空间里时（典型：平台在容器、agent 在宿主机），
    // 桥替 agent 把本机文件收下来换成平台取得到的 URL（ADR-0025）。
    // 发布（写文件）在 Bridge 里做，这里只起取文件的 HTTP 服务——
    // 两边各持一个 FileRelay 实例，共享同一个目录，token 目录就是事实来源。
    match &config.file_relay {
        Some(relay_config) => {
            let relay = Arc::new(FileRelay::new(&relay_config.dir, &relay_config.public_base));
            relay.sweep_expired();
            let listener = std::net::TcpListener::bind(&relay_config.listen)?;
            let shutdown = Arc::clone(&shutdown);
            std::thread::spawn(move || FileRelay::serve(listener, relay, shutdown));
            println!(
                "文件中继已监听 {}，平台从 {} 取文件（存 {}）",
                relay_config.listen, relay_config.public_base, relay_config.dir
            );
        }
        None => println!("没有配 [file_relay]，本机文件路径原样透传"),
    }

    {
        let bridge = Arc::clone(&bridge);
        let shutdown = Arc::clone(&shutdown);
        std::thread::spawn(move || {
            // 起来先扫一次：上次跑完留下的旧账不用等到下个周期才削。
            prune_once(&bridge);
            while !shutdown.load(Ordering::Relaxed) {
                std::thread::sleep(PRUNE_EVERY);
                prune_once(&bridge);
            }
        });
    }

    // 时钟：每秒评估一次订阅的时钟策略（ADR-0011）。默认全关，不配就不产生轮次。
    {
        let bridge = Arc::clone(&bridge);
        let shutdown = Arc::clone(&shutdown);
        std::thread::spawn(move || {
            while !shutdown.load(Ordering::Relaxed) {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|elapsed| elapsed.as_millis() as i64)
                    .unwrap_or(0);
                let queued = {
                    let mut guard = bridge
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    guard.tick_clock(now)
                };
                if queued > 0 {
                    println!("时钟：排了 {queued} 轮例行检查");
                }
                std::thread::sleep(Duration::from_secs(1));
            }
        });
    }

    println!("桥在跑。按 Ctrl-C 停。");
    while !shutdown.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_secs(3600));
    }
    Ok(())
}

/// 扫一次旧流水。删了什么就打一行，没删就不吭声——安静的日志才好读。
fn prune_once(bridge: &Mutex<Bridge<SqliteStore>>) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0);

    let report = {
        // 锁被毒死也接着用：桥的状态没有「半条」这种中间态（同 endpoint::lock）。
        let mut bridge = bridge
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        bridge.prune(now)
    };

    match report {
        Ok(pruned) => {
            for item in pruned {
                println!(
                    "清理：{} 删掉 {} 条已读记录，留 {} 条",
                    item.room, item.deleted, item.kept
                );
            }
        }
        Err(error) => eprintln!("清理失败（不影响收发，下次再试）：{error}"),
    }
}

/// 命令行参数。
struct Options {
    config: String,
    check_only: bool,
}

impl Options {
    fn parse(args: impl Iterator<Item = String>) -> Result<Self, Box<dyn std::error::Error>> {
        let mut config = "config.toml".to_owned();
        let mut check_only = false;
        let mut args = args.peekable();

        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--config" => {
                    config = args.next().ok_or("--config 后面要跟配置文件路径")?;
                }
                "--check" => check_only = true,
                "--help" | "-h" => {
                    println!("用法：acb-bridge [--config 路径] [--check]");
                    std::process::exit(0);
                }
                other => return Err(format!("不认识的参数：{other}（用 --help 看用法）").into()),
            }
        }

        Ok(Self { config, check_only })
    }
}

fn print_summary(config: &Config, path: &str) {
    println!("配置：{path}");

    println!("  连接：{} 条", config.connections.len());
    for connection in &config.connections {
        println!(
            "    - {}（{} / {}，{}）连 {}，登录账号 {}",
            connection.id,
            connection.platform,
            connection.kind,
            connection.transport.as_str(),
            connection.url,
            connection.account
        );
    }

    println!("  人格：{} 张", config.personas.len());
    for persona in &config.personas {
        println!("    - {}（{}）", persona.id, persona.name);
    }

    println!("  记忆：{} 份", config.memories.len());
    for memory in &config.memories {
        println!(
            "    - {}（{}，{} @ {}）",
            memory.id, memory.name, memory.kind, memory.location
        );
    }

    println!("  会话：{} 个", config.sessions.len());
    for session in &config.sessions {
        println!(
            "    - {}（harness {}，人格 {}，记忆 {:?}，{} 条订阅）",
            session.id,
            session.harness,
            session.persona,
            session.memories,
            session.subscriptions.len()
        );
        for subscription in &session.subscriptions {
            println!("      - {}", subscription.room);
        }
    }

    let retention = config.retention;
    if retention.enabled() {
        let by_count = if retention.max_records_per_room > 0 {
            format!("每房间留 {} 条", retention.max_records_per_room)
        } else {
            "不按条数清".to_owned()
        };
        let by_age = if retention.max_age_days > 0 {
            format!("最多留 {} 天", retention.max_age_days)
        } else {
            "不按天数清".to_owned()
        };
        println!("  流水保留：{by_count}，{by_age}（只削已读的，未读一条不动）");
    } else {
        println!("  流水保留：不清理——库只增不减");
    }
}
