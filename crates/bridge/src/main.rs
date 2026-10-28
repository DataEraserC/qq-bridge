//! 桥的入口：读配置、接平台、等 harness。
//!
//! 启动顺序：
//! 1. 校验配置（不对就在启动时死掉，不带病运行）；
//! 2. 打开状态库（流水、水位、笔记、投递计数都在里面，重启接着用）；
//! 3. 给每个平台配一个适配器，拿到「发送端口」；
//! 4. 建桥（配置 + 存储 + 一组发送端口）；
//! 5. 每个适配器起一个守护线程，连不上就等一会儿再连；
//! 6. 起接入点，等 harness 连进来。

use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use qqab_adapter_onebot11::{LinkSlot, OneBotOutbound, supervise};
use qqab_kernel::{Bridge, Config, Outbounds, SqliteStore, serve};

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

    // 每条平台连接先占一个发送口，桥建好之后再起守护线程去连。
    let mut outbounds = Outbounds::new();
    let mut links = Vec::new();
    for adapter in &config.adapters {
        let slot = LinkSlot::new();
        outbounds = outbounds.with(
            adapter.platform.clone(),
            Box::new(OneBotOutbound::new(slot.clone(), adapter.platform.clone())),
        );
        links.push((adapter.clone(), slot));
    }

    let bridge = Arc::new(Mutex::new(Bridge::new(
        config.clone(),
        store,
        Box::new(outbounds),
    )));

    for (adapter, slot) in links {
        let bridge = Arc::clone(&bridge);
        let shutdown = Arc::clone(&shutdown);
        std::thread::spawn(move || {
            supervise(
                slot,
                adapter.url,
                adapter.token,
                adapter.platform,
                bridge,
                shutdown,
            );
        });
    }

    match &config.endpoint {
        Some(endpoint) => {
            let listener = std::net::TcpListener::bind(&endpoint.listen)?;
            let bridge = Arc::clone(&bridge);
            let shutdown = Arc::clone(&shutdown);
            std::thread::spawn(move || {
                if let Err(error) = serve(listener, bridge, shutdown) {
                    eprintln!("接入点停了：{error}");
                }
            });
            println!("接入点已监听 {}", endpoint.listen);
        }
        None => println!("没有配 [endpoint]，harness 连不进来"),
    }

    println!("桥在跑。按 Ctrl-C 停。");
    while !shutdown.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_secs(3600));
    }
    Ok(())
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
                    println!("用法：qqab-bridge [--config 路径] [--check]");
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
    for device in &config.devices {
        println!(
            "  设备 {}：{} 条订阅，人格 {}，记忆 {}",
            device.id,
            device.subscriptions.len(),
            device.persona,
            device.memory
        );
        for subscription in &device.subscriptions {
            println!(
                "    - {} 用 {} 发",
                subscription.room, subscription.send_account
            );
        }
    }

    if config.adapters.is_empty() {
        println!("  适配器：一个都没配——只能收 harness 的话，发不出去");
    } else {
        for adapter in &config.adapters {
            println!(
                "  适配器 {}：{} 连 {}",
                adapter.platform, adapter.kind, adapter.url
            );
        }
    }

    // 订阅了某个平台却没给它配适配器：能记流水、收不到也发不出，说清楚免得白等。
    for device in &config.devices {
        for subscription in &device.subscriptions {
            if config.adapter(&subscription.room.platform).is_none() {
                eprintln!(
                    "注意：设备 {} 订阅了 {}，但平台 {} 没有配适配器——这个房间发不出去",
                    device.id, subscription.room, subscription.room.platform
                );
            }
        }
    }
}
