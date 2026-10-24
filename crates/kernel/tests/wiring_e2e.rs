//! 接线测试：真 Unix socket、真 WebSocket，中间是完整的 kernel 状态机。
//!
//! 与 `runtime_e2e.rs` 的区别是那里用通道模拟两端，这里两端都是真的：适配器一侧是
//! [`Server`] 绑出来的 socket，QQ 一侧是 `transport::Connection` 连到一个回环 WebSocket
//! 服务端。只缺 `main` 自己的主循环，其余接线全是生产代码。

use std::io::Write;
use std::net::TcpListener;
use std::sync::mpsc::{Receiver, channel};
use std::thread;
use std::time::Duration;

use time::OffsetDateTime;

use qqab_kernel::adapter::AdapterEvent;
use qqab_kernel::archive::Limits;
use qqab_kernel::config::Config;
use qqab_kernel::conn::Pool;
use qqab_kernel::persona::Personas;
use qqab_kernel::pipeline::Pipeline;
use qqab_kernel::runtime::{Inbound, Kernel, Outbound};
use qqab_kernel::transport::{Connection, Incoming};
use qqab_proto::{Frame, decode};

use tungstenite::Message as WsMessage;

const CONFIG: &str = r#"
[[accounts]]
id = "main"
self_id = "10001"
ws_url = "WS_URL_PLACEHOLDER"

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

  [devices.wake]
  at_mention = true
"#;

/// `@` 必须在 `message` 段里出现：`raw_message` 里的 CQ 码不参与解码。
const AT_ME: &str = r#"{"post_type":"message","message_type":"group","self_id":10001,
"group_id":20001,"message_id":"7451","user_id":30002,"sender":{"nickname":"小明"},
"time":1792561540,"message":[{"type":"at","data":{"qq":"10001"}},
{"type":"text","data":{"text":" 在吗"}}]}"#;

fn now() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_792_561_540).expect("合法时间戳")
}

/// 一个假的 OneBot 实现：接受一条连接，发一条 @ 消息，然后收 send_group_msg 并回执。
fn onebot_server() -> (String, thread::JoinHandle<Vec<u8>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("应能绑定回环端口");
    let port = listener.local_addr().expect("应有本地地址").port();

    let handle = thread::spawn(move || {
        let (stream, _) = listener.accept().expect("应能接受一条连接");
        let mut socket = tungstenite::accept(stream).expect("应能完成服务端握手");

        // 上游推一条 @ 自己的群消息。
        socket
            .send(WsMessage::Text(AT_ME.into()))
            .expect("应能发出事件");

        // 等内核发来的动作；把整行原样交回去给测试断言。
        loop {
            match socket.read() {
                Ok(WsMessage::Text(text)) => {
                    let echo = serde_json::from_str::<serde_json::Value>(&text)
                        .expect("动作应是 JSON")
                        .get("echo")
                        .and_then(|value| value.as_str())
                        .unwrap_or_default()
                        .to_owned();
                    socket
                        .send(WsMessage::Text(
                            format!(r#"{{"status":"ok","retcode":0,"echo":"{echo}"}}"#).into(),
                        ))
                        .expect("应能回执");
                    return text.to_string().into_bytes();
                }
                Ok(_) => continue,
                Err(error) => panic!("读动作失败：{error}"),
            }
        }
    });

    (format!("ws://127.0.0.1:{port}"), handle)
}

fn temp_endpoint() -> qqab_kernel::endpoint::Endpoint {
    let unique = format!("qqab-wiring-{}", std::process::id());
    if cfg!(windows) {
        qqab_kernel::endpoint::Endpoint::Namespaced(unique)
    } else {
        qqab_kernel::endpoint::Endpoint::Path(std::env::temp_dir().join(unique))
    }
}

/// 测试当适配器：连到内核绑的接入点上。
fn connect(endpoint: &qqab_kernel::endpoint::Endpoint) -> qqab_kernel::adapter::Conn {
    use interprocess::local_socket::traits::Stream as _;
    use interprocess::local_socket::{
        GenericFilePath, GenericNamespaced, Stream, ToFsName, ToNsName,
    };

    let stream = match endpoint {
        qqab_kernel::endpoint::Endpoint::Path(path) => {
            let name = path
                .as_path()
                .to_fs_name::<GenericFilePath>()
                .expect("路径应可用");
            Stream::connect(name).expect("应能连上内核")
        }
        qqab_kernel::endpoint::Endpoint::Namespaced(name) => {
            let name = name
                .as_str()
                .to_ns_name::<GenericNamespaced>()
                .expect("名字应可用");
            Stream::connect(name).expect("应能连上内核")
        }
        other => panic!("接线测试不走 TCP 形态，实际 {other:?}"),
    };
    stream.set_nonblocking(true).expect("应能设非阻塞");
    qqab_kernel::adapter::Conn::Local(stream)
}

/// 非阻塞读到一个完整行为止。
fn read_line(client: &mut qqab_kernel::adapter::Conn) -> String {
    use std::io::Read as _;
    let mut got = Vec::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut chunk = [0u8; 4096];
    while std::time::Instant::now() < deadline {
        match client.read(&mut chunk) {
            Ok(0) => panic!("连接被关闭了"),
            Ok(count) => {
                got.extend_from_slice(&chunk[..count]);
                if got.contains(&b'\n') {
                    break;
                }
            }
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.kind() == std::io::ErrorKind::TimedOut =>
            {
                thread::sleep(Duration::from_millis(20));
            }
            Err(error) => panic!("读失败：{error}"),
        }
    }
    String::from_utf8_lossy(&got).trim_end().to_owned()
}

/// 等待适配器连上来。
fn await_connected(rx: &Receiver<AdapterEvent>) -> (u64, std::sync::mpsc::Sender<String>) {
    match rx
        .recv_timeout(Duration::from_secs(5))
        .expect("应有适配器事件")
    {
        AdapterEvent::Connected { conn, out, .. } => (conn, out),
        other => panic!("期望 Connected，得到 {other:?}"),
    }
}

fn frames(out: &[Outbound]) -> Vec<Frame> {
    out.iter()
        .filter_map(|item| match item {
            Outbound::Frame { line, .. } => Some(decode(line).expect("出站行应能解码")),
            _ => None,
        })
        .collect()
}

#[test]
fn full_loop_from_qq_message_to_delivered_frame_and_back() {
    let (ws_url, onebot) = onebot_server();
    let endpoint = temp_endpoint();

    let config =
        Config::parse(&CONFIG.replace("WS_URL_PLACEHOLDER", &ws_url)).expect("配置应能解析");
    let personas = Personas::load(&config, std::path::Path::new(".")).expect("应能加载人格");

    let mut kernel = Kernel::new(
        config.clone(),
        personas,
        Pipeline::new(Limits::default(), 100, time::Duration::minutes(10)),
        Pool::new(&config.accounts),
    );

    // 真 socket：内核监听，测试当适配器连上去。
    let server = qqab_kernel::adapter::Server::bind(&endpoint).expect("应能绑定");
    let (adapter_tx, adapter_rx) = channel();
    qqab_kernel::adapter::Server::spawn(server, adapter_tx);

    let mut client = connect(&endpoint);
    let (conn, out) = await_connected(&adapter_rx);
    // 先登记连接：不登记的话内核没有写出通道，回不了 hello_ok。
    kernel.handle(
        Inbound::Adapter(AdapterEvent::Connected {
            conn,
            out: out.clone(),
            peer: "test".to_owned(),
        }),
        now(),
    );

    // 握手。
    client
        .write_all(b"{\"type\":\"hello\",\"id\":\"h-1\",\"version\":1,\"adapter\":\"wiring\"}\n")
        .expect("应能发 hello");
    let event = adapter_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("应有帧事件");
    let AdapterEvent::Frame { frame, .. } = event else {
        panic!("期望一帧，得到 {event:?}");
    };
    let out_frames = kernel.handle(Inbound::Adapter(AdapterEvent::Frame { conn, frame }), now());
    kernel.apply(out_frames);
    let hello_ok = read_line(&mut client);
    assert!(
        hello_ok.contains("hello_ok"),
        "握手应被接受，实际 {hello_ok:?}"
    );
    // 登记 QQ 连接：动作要有地方送，发送账号也要是就绪状态。
    let (act_tx, act_rx) = channel::<qqab_kernel::transport::ActionRequest>();
    kernel.handle(
        Inbound::LinkUp {
            account: "main".to_owned(),
            actions: act_tx,
        },
        now(),
    );

    // 真 WebSocket：连上去拿事件。
    let mut connection = Connection::open(&config.accounts[0]).expect("应能连上假 OneBot");
    let incoming = connection.read_event();
    assert!(
        matches!(incoming, Incoming::Event(_)),
        "应读到一条真实事件，得到 {incoming:?}"
    );

    // 喂给内核：应产生一帧 deliver。
    let action_out = kernel.handle(
        Inbound::Qq {
            account: "main".to_owned(),
            incoming,
        },
        now(),
    );
    let leftover = kernel.apply(action_out);
    let deliver = frames(&leftover).into_iter().find_map(|frame| match frame {
        Frame::Deliver(deliver) => Some(deliver),
        _ => None,
    });
    let deliver = match deliver {
        Some(deliver) => deliver,
        None => {
            // 帧已经被 apply 写进 socket，从客户端读回来。
            let line = read_line(&mut client);
            match decode(&line).expect("应能解码") {
                Frame::Deliver(deliver) => deliver,
                other => panic!("期望 deliver，得到 {other:?}"),
            }
        }
    };
    assert_eq!(deliver.memory, "alice");
    assert_eq!(deliver.wake_reason, "at_mention");

    // 适配器：确认投递，然后要求发一条消息。
    client
        .write_all(format!("{{\"type\":\"deliver_ok\",\"id\":\"{}\"}}\n", deliver.id).as_bytes())
        .expect("应能发 deliver_ok");
    client
        .write_all(
            format!(
                "{{\"type\":\"tool_call\",\"id\":\"t-1\",\"ticket\":\"{}\",\"memory\":\"alice\",\
                 \"tool\":\"qq_send_message\",\"args\":{{\"text\":\"在\"}}}}\n",
                deliver.ticket
            )
            .as_bytes(),
        )
        .expect("应能发 tool_call");

    // 内核收这两帧。
    // 先喂 deliver_ok：水位应推进。
    let event = adapter_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("应有帧事件");
    let AdapterEvent::Frame { frame, .. } = event else {
        panic!("期望 deliver_ok，得到 {event:?}");
    };
    kernel.handle(Inbound::Adapter(AdapterEvent::Frame { conn, frame }), now());
    assert_eq!(
        kernel.pipeline().archive().watermark("alice", "20001"),
        1,
        "确认之后水位推进"
    );

    // 再喂 tool_call：应变成一次真动作，经 socket 发到假 OneBot。
    let event = adapter_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("应有帧事件");
    let AdapterEvent::Frame { frame, .. } = event else {
        panic!("期望 tool_call，得到 {event:?}");
    };
    let out = kernel.handle(Inbound::Adapter(AdapterEvent::Frame { conn, frame }), now());
    for item in kernel.apply(out) {
        let Outbound::Act { account, request } = item else {
            continue;
        };
        assert_eq!(account, "main");
        let (outcome, _) = connection.act(&request.action, &request.echo);
        let _ = request.reply.send(outcome);
    }
    // 动作应真的进了队列。
    let _ = act_rx;

    // 在途动作的回执：下一拍应把 tool_result 写回适配器。
    let tick = kernel.handle(Inbound::Tick, now() + time::Duration::milliseconds(300));
    kernel.apply(tick);
    let result_line = read_line(&mut client);
    let result = decode(&result_line).expect("应能解码");
    assert!(
        matches!(result, Frame::ToolResult(_)),
        "工具调用应拿到结果，实际 {result:?}"
    );

    // 假 OneBot 收到的那一行就是内核真正发出去的 QQ 动作。
    let sent = onebot.join().expect("服务端应正常结束");
    let value: serde_json::Value = serde_json::from_slice(&sent).expect("应是 JSON");
    assert_eq!(value["action"], "send_group_msg");
    assert_eq!(value["params"]["group_id"], "20001");
    assert_eq!(value["params"]["message"], "在");
}
