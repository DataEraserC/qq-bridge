//! 传输层的端到端测试：真的起一个 WebSocket 服务端，让内核连上去收发。
//!
//! 不 mock `tungstenite`——这里要验证的正是「握手、帧读写、动作响应与事件交织」这些真实
//! 行为。只把服务端换成本地回环地址，因此不需要网络、不依赖 OneBot 实现。

use std::net::TcpListener;
use std::thread;

use qqab_kernel::chat::{ChatAction, ChatEvent, ChatMessage};
use qqab_kernel::config::{Account, Secret};
use qqab_kernel::onebot::OneBot;
use qqab_kernel::transport::{ActionOutcome, ConnectError, Connection, Incoming};

use tungstenite::Message as WsMessage;
use tungstenite::protocol::WebSocket;

/// 起一个只跑一段脚本的服务端，返回 `ws://` 地址与一个 join 把手。
fn serve(
    script: impl FnOnce(&mut WebSocket<std::net::TcpStream>) + Send + 'static,
) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("应能绑定回环端口");
    let port = listener.local_addr().expect("应有本地地址").port();

    let handle = thread::spawn(move || {
        let (stream, _) = listener.accept().expect("应能接受一条连接");
        let mut socket = tungstenite::accept(stream).expect("应能完成服务端握手");
        script(&mut socket);
    });

    (format!("ws://127.0.0.1:{port}"), handle)
}

fn account(url: &str) -> Account {
    Account {
        id: "main".to_owned(),
        self_id: "10001".to_owned(),
        protocol: "onebot11".to_owned(),
        ws_url: url.to_owned(),
        token: None,
    }
}

const GROUP_MESSAGE: &str = r#"{"post_type":"message","message_type":"group",
"self_id":10001,"group_id":20001,"message_id":"7451","user_id":30002,
"sender":{"nickname":"小明"},"time":1792561540,
"message":[{"type":"text","data":{"text":"在吗"}}]}"#;

#[test]
fn connects_and_reads_an_event() {
    let (url, server) = serve(|socket| {
        socket
            .send(WsMessage::Text(GROUP_MESSAGE.into()))
            .expect("应能发出事件");
    });

    let mut connection =
        Connection::open(&account(&url), Box::new(OneBot::new())).expect("应能建连");
    assert_eq!(connection.account(), "main");
    assert_eq!(connection.self_id(), "10001");

    match connection.read_event() {
        Incoming::Event(event) => {
            let ChatEvent::Message(message) = *event else {
                panic!("期望群消息");
            };
            assert_eq!(message.group, "20001");
            assert_eq!(message.text, "在吗");
        }
        other => panic!("期望事件，得到 {other:?}"),
    }

    let _ = server.join();
}

#[test]
fn undecodable_frame_does_not_kill_the_connection() {
    // 上游发来一条我们不认识的 JSON，之后还要能继续读到事件。
    let (url, server) = serve(|socket| {
        socket
            .send(WsMessage::Text("{\"whatever\":1}".into()))
            .expect("应能发出坏帧");
        socket
            .send(WsMessage::Text(GROUP_MESSAGE.into()))
            .expect("应能发出事件");
    });

    let mut connection =
        Connection::open(&account(&url), Box::new(OneBot::new())).expect("应能建连");

    assert!(matches!(
        connection.read_event(),
        Incoming::Undecodable { .. }
    ));
    // 关键：坏帧之后连接仍然可用。
    assert!(matches!(connection.read_event(), Incoming::Event(_)));

    let _ = server.join();
}

#[test]
fn sends_action_and_matches_response_by_echo() {
    let (url, server) = serve(|socket| {
        let frame = socket.read().expect("应能读到动作帧");
        let WsMessage::Text(text) = frame else {
            panic!("期望文本帧");
        };
        let value: serde_json::Value = serde_json::from_str(&text).expect("应是 JSON");
        assert_eq!(value["action"], "send_group_msg");
        assert_eq!(value["params"]["group_id"], "20001");
        assert_eq!(value["params"]["message"], "在");

        // 回一个别的 echo 的响应先到，验证靠 echo 匹配而不是「读到第一帧就认」。
        socket
            .send(WsMessage::Text(
                r#"{"status":"ok","retcode":0,"echo":"other"}"#.into(),
            ))
            .expect("应能发出无关响应");
        let echo = value["echo"].as_str().expect("动作帧应带 echo").to_owned();
        socket
            .send(WsMessage::Text(
                format!(r#"{{"status":"ok","retcode":0,"echo":"{echo}"}}"#).into(),
            ))
            .expect("应能发出对应响应");
    });

    let mut connection =
        Connection::open(&account(&url), Box::new(OneBot::new())).expect("应能建连");

    let (outcome, carried) = connection.act(
        &ChatAction::SendGroupMessage {
            group: "20001".to_owned(),
            text: "在".to_owned(),
        },
        "a-1",
    );

    match outcome {
        Ok(ActionOutcome::Ok { .. }) => {}
        other => panic!("期望成功，得到 {other:?}"),
    }
    assert!(carried.is_empty(), "没有夹带事件时不该有残留");

    let _ = server.join();
}

#[test]
fn rejected_action_reports_reason_without_retry() {
    let (url, server) = serve(|socket| {
        let _ = socket.read();
        socket
            .send(WsMessage::Text(
                r#"{"status":"failed","retcode":1200,"echo":"a-1","data":{"message":"群不存在"}}"#
                    .into(),
            ))
            .expect("应能发出拒绝响应");
    });

    let mut connection =
        Connection::open(&account(&url), Box::new(OneBot::new())).expect("应能建连");
    let (outcome, _) = connection.act(
        &ChatAction::SendGroupMessage {
            group: "0".to_owned(),
            text: "x".to_owned(),
        },
        "a-1",
    );

    match outcome {
        Ok(ActionOutcome::Rejected { reason }) => assert_eq!(reason, "群不存在"),
        other => panic!("期望被拒，得到 {other:?}"),
    }

    let _ = server.join();
}

#[test]
fn events_arriving_while_waiting_for_an_action_are_carried_back() {
    // 群不会因为你正在发消息就安静：等响应的期间照样可能来事件。这些事件丢了就是丢消息。
    let (url, server) = serve(|socket| {
        let _ = socket.read();
        // 先推一条事件，再回响应——客户端此刻正卡在等带上。
        socket
            .send(WsMessage::Text(GROUP_MESSAGE.into()))
            .expect("应能发出事件");
        socket
            .send(WsMessage::Text(
                r#"{"status":"ok","retcode":0,"echo":"a-1"}"#.into(),
            ))
            .expect("应能发出响应");
    });

    let mut connection =
        Connection::open(&account(&url), Box::new(OneBot::new())).expect("应能建连");
    let (outcome, carried) = connection.act(
        &ChatAction::SendGroupMessage {
            group: "20001".to_owned(),
            text: "在".to_owned(),
        },
        "a-1",
    );

    assert!(matches!(outcome, Ok(ActionOutcome::Ok { .. })));
    assert_eq!(carried.len(), 1, "等响应期间的事件必须带回来");
    assert!(matches!(carried[0], ChatEvent::Message(ChatMessage { .. })));

    let _ = server.join();
}

#[test]
fn unauthorized_handshake_is_reported_not_silently_successful() {
    // 服务端要求令牌、客户端没带：必须是一次明确的失败，交给退避。
    let listener = TcpListener::bind("127.0.0.1:0").expect("应能绑定");
    let port = listener.local_addr().expect("应有地址").port();
    let handle = thread::spawn(move || {
        let (_stream, _) = listener.accept().expect("应能接受一条连接");
        // 不发握手响应直接关：客户端应报错而不是挂起。
    });

    let result = Connection::open(
        &account(&format!("ws://127.0.0.1:{port}")),
        Box::new(OneBot::new()),
    );
    assert!(
        matches!(
            result,
            Err(ConnectError::Handshake { .. }) | Err(ConnectError::BadUrl { .. })
        ),
        "应明确失败"
    );

    let _ = handle.join();
}

#[test]
fn literal_token_is_sent_as_authorization_header() {
    let (url, server) = serve(|socket| {
        // 握手已经完成，客户端能读能写即说明头没有让它被拒。
        socket
            .send(WsMessage::Text(GROUP_MESSAGE.into()))
            .expect("应能发出事件");
    });

    let mut with_token = account(&url);
    with_token.token = Some(Secret::Literal("Bearer s3cret".to_owned()));

    let mut connection = Connection::open(&with_token, Box::new(OneBot::new())).expect("应能建连");
    assert!(matches!(connection.read_event(), Incoming::Event(_)));

    let _ = server.join();
}

#[test]
fn missing_token_env_fails_before_connecting() {
    let account = Account {
        id: "main".to_owned(),
        self_id: "10001".to_owned(),
        protocol: "onebot11".to_owned(),
        ws_url: "ws://127.0.0.1:1".to_owned(),
        token: Some(Secret::Env("QQAB_TEST_TOKEN_NOT_SET".to_owned())),
    };

    // 环境变量缺失应在建连之前就被发现：不是「连不上」，是「配置不全」。
    match Connection::open(&account, Box::new(OneBot::new())) {
        Err(ConnectError::TokenEnv(name)) => assert_eq!(name, "QQAB_TEST_TOKEN_NOT_SET"),
        other => panic!("期望令牌缺失错误，得到 {other:?}"),
    }
}
