//! 接入点的端到端：真的开一个监听端口，真的连上去，走完握手 → 投递 → 工具 → 收工。
//!
//! 这里没有任何 mock 掉的部分：TCP 是真的，帧编码是真的，锁也是真的。唯一的假东西是
//! 出站通道（不往真平台发消息）和「平台那边把一个聊天消息喂进来」这一下。

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use acb_kernel::endpoint::{read_frame, reader_of};
use acb_kernel::{
    Bridge, Conn, Element, Frame, MemoryStore, Outbound, OutboundError, RecordDraft, RoomId,
    Sender, Sent, handshake, serve,
};
use acb_proto::frame::{PROTOCOL_VERSION, encode};
use serde_json::json;

const TOKEN: &str = "t-main";
const AT: i64 = 1_793_000_000_000;

#[derive(Default)]
struct Recorder {
    sent: Vec<(RoomId, Vec<Element>)>,
}

impl Outbound for Recorder {
    fn send(&mut self, room: &RoomId, elements: &[Element]) -> Result<Sent, OutboundError> {
        self.sent.push((room.clone(), elements.to_vec()));
        Ok(Sent {
            platform_id: format!("p-{}", self.sent.len()),
            at: AT,
        })
    }
}

fn config() -> &'static str {
    r#"
        [[connections]]
        id = "qq"
        platform = "qq"
        kind = "onebot11"
        url = "ws://127.0.0.1:3001"
        account = "bot"

        [endpoint]
        listen = "127.0.0.1:7700"

        [[personas]]
        id = "p"
        name = "人格"
        text = "你是桥。"

        [[sessions]]
        id = "main"
        harness = "dsh"
        persona = "p"
        token = "t-main"

        [[sessions.subscriptions]]
        connection = "qq"
        room = "group/123"
    "#
}

fn room(text: &str) -> RoomId {
    RoomId::parse(text).expect("房间合法")
}

fn message(text: &str) -> RecordDraft {
    RecordDraft::message(
        room("qq:group/123"),
        Sender {
            id: "10001".into(),
            name: "老王".into(),
        },
        AT,
        vec![Element::text(text)],
        true,
    )
}

fn write_frame(stream: &mut TcpStream, frame: &Frame) {
    let line = encode(frame).expect("可编码");
    stream.write_all(line.as_bytes()).expect("能写出");
    stream.flush().expect("能刷出");
}

// ---------------------------------------------------------------------------
// 握手：纯状态机，不开端口
// ---------------------------------------------------------------------------

fn bridge() -> Bridge<MemoryStore> {
    Bridge::new(
        acb_kernel::Config::parse(config()).expect("配置合法"),
        MemoryStore::new(),
        Box::new(Recorder::default()),
    )
}

#[test]
fn handshake_binds_the_connection_to_the_token_group() {
    let bridge = bridge();
    let sessions = handshake(
        &bridge,
        Frame::Hello {
            version: PROTOCOL_VERSION,
            harness: "dsh".into(),
            token: Some(TOKEN.into()),
        },
    )
    .expect("能握手");
    assert_eq!(sessions, vec!["main".to_owned()]);
}

#[test]
fn handshake_rejects_bad_token_version_and_wrong_first_frame() {
    let bridge = bridge();

    let error = handshake(
        &bridge,
        Frame::Hello {
            version: PROTOCOL_VERSION,
            harness: "dsh".into(),
            token: Some("乱写".into()),
        },
    )
    .expect_err("令牌不对");
    assert!(error.to_string().contains("令牌"));

    let error = handshake(
        &bridge,
        Frame::Hello {
            version: PROTOCOL_VERSION + 1,
            harness: "dsh".into(),
            token: Some(TOKEN.into()),
        },
    )
    .expect_err("版本不对");
    assert!(error.to_string().contains("版本"));

    let error = handshake(&bridge, Frame::Ping).expect_err("第一帧不是 hello");
    assert!(error.to_string().contains("hello"));

    let error = handshake(
        &bridge,
        Frame::Hello {
            version: PROTOCOL_VERSION,
            harness: "dsh".into(),
            token: None,
        },
    )
    .expect_err("没带令牌");
    assert!(error.to_string().contains("令牌"));
}

// ---------------------------------------------------------------------------
// 会话状态机
// ---------------------------------------------------------------------------

#[test]
fn session_refuses_a_ticket_it_did_not_receive() {
    let mut bridge = bridge();
    bridge.ingest(message("在吗")).expect("能记下来");
    let mut session = Conn::new(vec!["main".to_owned()]);

    let delivery = session
        .take_delivery(&mut bridge)
        .expect("能取活")
        .expect("有活");
    assert_eq!(session.ticket(), Some(&delivery.ticket));

    // 冒充别的票。
    let replies = session
        .handle(
            &mut bridge,
            Frame::ToolCall {
                id: "c-1".into(),
                ticket: acb_kernel::Ticket::new("main-99").expect("合法"),
                tool: "send_message".into(),
                args: json!({"elements": [{"type": "text", "text": "hi"}]}),
            },
        )
        .expect("不该崩");
    match &replies[0] {
        Frame::ToolResult {
            error: Some(error), ..
        } => {
            assert_eq!(error.code, acb_kernel::ErrorCode::Unauthorized);
        }
        other => panic!("应当是错误结果，实际是 {}", other.kind()),
    }
}

#[test]
fn session_answers_ping_and_ends_turns() {
    let mut bridge = bridge();
    bridge.ingest(message("在吗")).expect("能记下来");
    let mut session = Conn::new(vec!["main".to_owned()]);
    let delivery = session
        .take_delivery(&mut bridge)
        .expect("能取活")
        .expect("有活");

    let replies = session.handle(&mut bridge, Frame::Ping).expect("能处理");
    assert!(matches!(replies[0], Frame::Pong));

    session
        .handle(
            &mut bridge,
            Frame::TurnEnd {
                ticket: delivery.ticket.clone(),
                error: None,
            },
        )
        .expect("能收工");
    assert!(session.ticket().is_none());
    assert!(
        session
            .take_delivery(&mut bridge)
            .expect("能取活")
            .is_none(),
        "收工之后没活了"
    );
}

// ---------------------------------------------------------------------------
// 真连接
// ---------------------------------------------------------------------------

/// 一个真起来的接入点：真端口、真线程。
struct TestServer {
    address: std::net::SocketAddr,
    bridge: Arc<Mutex<Bridge<MemoryStore>>>,
    shutdown: Arc<AtomicBool>,
    handle: thread::JoinHandle<()>,
}

impl TestServer {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("能监听");
        let address = listener.local_addr().expect("有地址");
        let bridge = Arc::new(Mutex::new(bridge()));
        let shutdown = Arc::new(AtomicBool::new(false));

        let thread_bridge = Arc::clone(&bridge);
        let thread_shutdown = Arc::clone(&shutdown);
        let handle = thread::spawn(move || {
            serve(listener, thread_bridge, thread_shutdown).expect("服务能跑");
        });

        Self {
            address,
            bridge,
            shutdown,
            handle,
        }
    }

    fn stop(self) {
        self.shutdown.store(true, Ordering::SeqCst);
        self.handle.join().expect("服务线程能收工");
    }
}

#[test]
fn a_real_connection_carries_a_whole_turn() {
    let server = TestServer::start();
    let bridge = Arc::clone(&server.bridge);

    let mut stream = TcpStream::connect(server.address).expect("能连上");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("能设超时");
    let mut reader = BufReader::new(stream.try_clone().expect("能复制"));

    // 握手。
    write_frame(
        &mut stream,
        &Frame::Hello {
            version: PROTOCOL_VERSION,
            harness: "dsh".into(),
            token: Some(TOKEN.into()),
        },
    );
    match read_frame(&mut reader).expect("能读到 ready") {
        Frame::Ready { sessions } => assert_eq!(sessions, vec!["main".to_owned()]),
        other => panic!("应当是 ready，实际是 {}", other.kind()),
    }

    // 平台那边来了条消息（直接喂给桥，等价于适配器解码后调 ingest）。
    {
        let mut guard = bridge.lock().expect("没被毒死");
        let ingested = guard.ingest(message("在吗")).expect("能记下来");
        assert!(matches!(ingested, acb_kernel::Ingest::Recorded { .. }));
    }

    // 这一端应当把轮次推过来。
    let delivery = match read_frame(&mut reader).expect("能读到 turn_start") {
        Frame::TurnStart { delivery } => delivery,
        other => panic!("应当是 turn_start，实际是 {}", other.kind()),
    };
    assert_eq!(delivery.session, "main");
    assert_eq!(delivery.records.len(), 1);
    let ticket = delivery.ticket;

    // agent 回一句话。
    write_frame(
        &mut stream,
        &Frame::ToolCall {
            id: "c-1".into(),
            ticket: ticket.clone(),
            tool: "send_message".into(),
            args: json!({"elements": [{"type": "text", "text": "在"}]}),
        },
    );
    match read_frame(&mut reader).expect("能读到 tool_result") {
        Frame::ToolResult { id, result, error } => {
            assert_eq!(id, "c-1");
            assert!(error.is_none(), "不该有错");
            assert_eq!(result.expect("有结果")["id"], 2);
        }
        other => panic!("应当是 tool_result，实际是 {}", other.kind()),
    }

    // 确认 + 收工。
    write_frame(
        &mut stream,
        &Frame::ToolCall {
            id: "c-2".into(),
            ticket: ticket.clone(),
            tool: "ack_messages".into(),
            args: json!({"ids": [1]}),
        },
    );
    match read_frame(&mut reader).expect("能读到 tool_result") {
        Frame::ToolResult { result, .. } => {
            assert_eq!(result.expect("有结果")["watermark"], 2);
        }
        other => panic!("应当是 tool_result，实际是 {}", other.kind()),
    }
    write_frame(
        &mut stream,
        &Frame::TurnEnd {
            ticket: ticket.clone(),
            error: None,
        },
    );

    // 水位真的推了，设备真的空出来了。
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let busy = {
            let guard = bridge.lock().expect("没被毒死");
            guard.busy_sessions()
        };
        if busy.is_empty() {
            break;
        }
        assert!(Instant::now() < deadline, "设备一直没空出来");
        thread::sleep(Duration::from_millis(20));
    }

    drop(reader);
    drop(stream);
    server.stop();
}

#[test]
fn redelivery_hint_rides_on_note_not_on_content() {
    let server = TestServer::start();
    let bridge = Arc::clone(&server.bridge);

    let mut stream = TcpStream::connect(server.address).expect("能连上");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("能设超时");
    let mut reader = BufReader::new(stream.try_clone().expect("能复制"));

    write_frame(
        &mut stream,
        &Frame::Hello {
            version: PROTOCOL_VERSION,
            harness: "dsh".into(),
            token: Some(TOKEN.into()),
        },
    );
    assert!(matches!(
        read_frame(&mut reader).expect("能读到 ready"),
        Frame::Ready { .. }
    ));

    // 第一轮：投一条，不确认就收工。
    {
        let mut guard = bridge.lock().expect("没被毒死");
        guard.ingest(message("在吗")).expect("能记下来");
    }
    let first = match read_frame(&mut reader).expect("能读到 turn_start") {
        Frame::TurnStart { delivery } => delivery,
        other => panic!("应当是 turn_start，实际是 {}", other.kind()),
    };
    assert!(first.records[0].note.is_none(), "第一次投递不需要旁白");
    write_frame(
        &mut stream,
        &Frame::TurnEnd {
            ticket: first.ticket,
            error: None,
        },
    );

    // 第二轮：新消息叫醒，上一条重投。
    {
        let mut guard = bridge.lock().expect("没被毒死");
        guard.ingest(message("还在吗")).expect("能记下来");
    }
    let second = match read_frame(&mut reader).expect("能读到 turn_start") {
        Frame::TurnStart { delivery } => delivery,
        other => panic!("应当是 turn_start，实际是 {}", other.kind()),
    };
    assert_eq!(second.records.len(), 2);

    let redelivered = &second.records[0];
    assert_eq!(redelivered.redelivery, Some(2));
    let note = redelivered.note.as_deref().unwrap_or_default();
    assert!(note.contains("第 2 次投递"), "旁白要写明第几次：{note}");
    assert!(note.contains("#1"), "旁白要写明确认哪条：{note}");
    assert!(note.contains("ack_messages"), "旁白要写明怎么止投：{note}");
    assert_eq!(
        redelivered.elements.len(),
        1,
        "旁白不能混进内容元素：{:?}",
        redelivered.elements
    );
    assert_eq!(
        acb_proto::element::flatten_text(&redelivered.elements),
        "在吗",
        "内容要原样，一个字都不能被塞进去"
    );
    assert!(second.records[1].note.is_none());

    drop(reader);
    drop(stream);
    server.stop();
}

#[test]
fn a_bad_token_gets_no_ready() {
    let server = TestServer::start();

    let mut stream = TcpStream::connect(server.address).expect("能连上");
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .expect("能设超时");
    write_frame(
        &mut stream,
        &Frame::Hello {
            version: PROTOCOL_VERSION,
            harness: "dsh".into(),
            token: Some("偷来的".into()),
        },
    );

    let mut reader = reader_of(&stream);
    let mut line = String::new();
    let read = reader.read_line(&mut line).expect("读得到（或读到断开）");
    assert_eq!(read, 0, "令牌不对就该直接断开，不该回 ready：{line}");

    drop(stream);
    server.stop();
}

#[test]
fn dropping_the_connection_mid_turn_releases_the_device() {
    let server = TestServer::start();
    let bridge = Arc::clone(&server.bridge);

    let mut stream = TcpStream::connect(server.address).expect("能连上");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("能设超时");
    let mut reader = BufReader::new(stream.try_clone().expect("能复制"));

    write_frame(
        &mut stream,
        &Frame::Hello {
            version: PROTOCOL_VERSION,
            harness: "dsh".into(),
            token: Some(TOKEN.into()),
        },
    );
    assert!(matches!(
        read_frame(&mut reader).expect("能读到 ready"),
        Frame::Ready { .. }
    ));

    {
        let mut guard = bridge.lock().expect("没被毒死");
        guard.ingest(message("在吗")).expect("能记下来");
    }
    assert!(matches!(
        read_frame(&mut reader).expect("能读到 turn_start"),
        Frame::TurnStart { .. }
    ));

    // harness 跑了，连招呼都不打就断。
    drop(reader);
    drop(stream);

    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let busy = {
            let guard = bridge.lock().expect("没被毒死");
            guard.busy_sessions()
        };
        if busy.is_empty() {
            break;
        }
        assert!(Instant::now() < deadline, "断线之后设备没被放出来");
        thread::sleep(Duration::from_millis(20));
    }

    server.stop();
}
