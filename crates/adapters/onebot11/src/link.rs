//! 与 OneBot 11 实现之间的 WebSocket 长连接。
//!
//! 一个线程同时管读和写：读带 20ms 超时，超时不是错，只是「这会儿没消息」，回头把排队的
//! 动作写出去再读。这样做的好处是不需要拆 socket，也不会出现「读线程阻塞、回复发不出去」
//! 的僵局。
//!
//! 两处特意设计的地方：
//! - **动作走队列**：`send` 只是把动作塞进队列，由这个线程统一写 socket 并按 `echo` 配对
//!   回包，写读永远只有一个人，不会串字节。
//! - **事件走队列**：适配器只把 `RecordDraft` 丢进 core 的 `EventQueue`，不拿桥的锁，
//!   也不直接调 `ingest`。真正入库由 core 的 drain 线程统一做。

use std::collections::HashMap;
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use acb_kernel::{Bridge, EventQueue, Outbound, RoomId, Store};
use serde_json::{Value, json};
use tungstenite::error::Error as WsError;
use tungstenite::{Message, WebSocket};

use crate::event::{self, Parsed};
use crate::outbound::OneBotOutbound;

/// 一条 WebSocket 连接。
pub type Ws = WebSocket<TcpStream>;

/// 每次读最多等这么久，然后回头看看有没有动作要写。
pub const READ_TICK: Duration = Duration::from_millis(20);
/// 握手最多等这么久。握手期间不能用读超时，否则正常网络也会被判超时。
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// 断线之后隔多久重连。
pub const RECONNECT_DELAY: Duration = Duration::from_secs(5);

/// 适配器错误。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LinkError {
    #[error("地址不合法：{0}（要写成 ws://主机:端口/路径）")]
    BadUrl(String),
    #[error("连不上 {address}：{detail}")]
    Connect { address: String, detail: String },
    #[error("WebSocket 握手失败：{0}")]
    Handshake(String),
    #[error("连接出错：{0}")]
    Transport(String),
    #[error("等平台回话超时")]
    Timeout,
}

/// 一个待发出的动作，外加等结果的地方。
pub struct Action {
    pub body: Value,
    pub reply: Sender<Result<Value, LinkError>>,
}

/// 当前这条连接的发送端。断线时清空、重连时换新的，出站那边只管往里塞。
#[derive(Clone, Default)]
pub struct LinkSlot {
    inner: Arc<Mutex<Option<Sender<Action>>>>,
}

impl LinkSlot {
    pub fn new() -> Self {
        Self::default()
    }

    /// 这条连接现在通不通。
    pub fn connected(&self) -> bool {
        self.lock().is_some()
    }

    fn install(&self, sender: Sender<Action>) {
        *self.lock() = Some(sender);
    }

    fn clear(&self) {
        *self.lock() = None;
    }

    pub(crate) fn current(&self) -> Option<Sender<Action>> {
        self.lock().clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Sender<Action>>> {
        self.inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }
}

/// 连上 OneBot 的 WebSocket 服务端（正向 WS）。
///
/// `token` 是 OneBot 那边开的访问令牌，填了就放在 `Authorization: Bearer` 头里——
/// NapCat / Lagrange / go-cqhttp 都认这个头；地址里带 `?access_token=` 也行，那样令牌
/// 会跟着请求路径送过去。
///
/// 只支持 `ws://`：`wss://` 要额外拉 TLS 依赖，等真有需要再加。
pub fn connect(url: &str, token: Option<&str>, read_timeout: Duration) -> Result<Ws, LinkError> {
    let address = endpoint_of(url)?;
    let request = build_request(url, token)?;
    let stream = TcpStream::connect(&address).map_err(|error| LinkError::Connect {
        address: address.clone(),
        detail: error.to_string(),
    })?;
    let _ = stream.set_nodelay(true);
    stream
        .set_read_timeout(Some(HANDSHAKE_TIMEOUT))
        .map_err(|error| LinkError::Connect {
            address: address.clone(),
            detail: error.to_string(),
        })?;

    let (ws, _response) = tungstenite::client(request, stream)
        .map_err(|error| LinkError::Handshake(error.to_string()))?;

    // 握手之后换成短超时：读一下没消息就回去看有没有动作要写。
    let _ = ws.get_ref().set_read_timeout(Some(read_timeout));
    Ok(ws)
}

/// 组一个带令牌的握手请求。
fn build_request(
    url: &str,
    token: Option<&str>,
) -> Result<tungstenite::http::Request<()>, LinkError> {
    use tungstenite::client::IntoClientRequest;

    let mut request = url
        .into_client_request()
        .map_err(|error| LinkError::BadUrl(format!("{url}（{error}）")))?;

    if let Some(token) = token {
        let token = token.trim();
        if !token.is_empty() {
            let value = tungstenite::http::HeaderValue::from_str(&format!("Bearer {token}"))
                .map_err(|_| LinkError::BadUrl("访问令牌里有不能放进请求头的字符".into()))?;
            request
                .headers_mut()
                .insert(tungstenite::http::header::AUTHORIZATION, value);
        }
    }

    Ok(request)
}

/// 从地址里取 `主机:端口`。
///
/// 认三种常见写法：`ws://h:1`、`ws://h:1/path`、`ws://h:1/path?access_token=x`。
/// 查询串是给人看的（很多 OneBot 实现用 `access_token` 参数鉴权），不属于地址。
fn endpoint_of(url: &str) -> Result<String, LinkError> {
    let rest = url
        .strip_prefix("ws://")
        .ok_or_else(|| LinkError::BadUrl(url.to_owned()))?;
    let authority = rest
        .split(['/', '?'])
        .next()
        .ok_or_else(|| LinkError::BadUrl(url.to_owned()))?;
    if authority.is_empty() {
        return Err(LinkError::BadUrl(url.to_owned()));
    }
    if authority.contains(':') {
        Ok(authority.to_owned())
    } else {
        Ok(format!("{authority}:80"))
    }
}

/// 一条连接的主循环：写动作、喂事件、读回包。连不上或断了就返回。
pub fn pump(
    mut ws: Ws,
    actions: Receiver<Action>,
    events: &EventQueue,
    connection: &str,
    admin_qqs: &[String],
    slot: &LinkSlot,
    shutdown: &AtomicBool,
) -> Result<(), LinkError> {
    let mut waiting: HashMap<String, Sender<Result<Value, LinkError>>> = HashMap::new();

    while !shutdown.load(Ordering::Relaxed) {
        // 1. 把排队的动作写出去。写和读在同一个线程，必须严格串行。
        loop {
            match actions.try_recv() {
                Ok(action) => {
                    if let Some(echo) = action.body.get("echo").and_then(Value::as_str) {
                        waiting.insert(echo.to_owned(), action.reply);
                    }
                    if let Err(error) = ws.send(Message::text(action.body.to_string())) {
                        fail_all(&mut waiting, "发送时连接断了");
                        return Err(LinkError::Transport(error.to_string()));
                    }
                }
                Err(TryRecvError::Empty) => break,
                // 出站那边没了（进程要退），当成正常收工。
                Err(TryRecvError::Disconnected) => return Ok(()),
            }
        }

        // 2. 读一帧。事件由 handle_text 丢进 core 的入站队列。
        match ws.read() {
            Ok(Message::Text(text)) => {
                handle_text(
                    text.as_str(),
                    events,
                    connection,
                    admin_qqs,
                    slot,
                    &mut waiting,
                );
            }
            // tungstenite 收到 ping 会自动回 pong，这里只兜一手。
            Ok(Message::Ping(payload)) => {
                let _ = ws.send(Message::Pong(payload));
            }
            Ok(Message::Close(_)) => return Ok(()),
            Ok(_) => {}
            Err(WsError::Io(error)) if is_timeout(&error) => continue,
            Err(WsError::ConnectionClosed | WsError::AlreadyClosed) => return Ok(()),
            Err(error) => {
                fail_all(&mut waiting, "读的时候连接断了");
                return Err(LinkError::Transport(error.to_string()));
            }
        }
    }

    fail_all(&mut waiting, "桥要退出了");
    Ok(())
}

fn is_timeout(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::WouldBlock
            | std::io::ErrorKind::TimedOut
            | std::io::ErrorKind::Interrupted
    )
}

fn handle_text(
    text: &str,
    events: &EventQueue,
    connection: &str,
    admin_qqs: &[String],
    slot: &LinkSlot,
    waiting: &mut HashMap<String, Sender<Result<Value, LinkError>>>,
) {
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        eprintln!("适配器 {connection}：收到一段不是 JSON 的内容，丢掉");
        return;
    };

    // 有 echo 就是动作的回包，交给等它的那个人。
    if let Some(echo) = value.get("echo").and_then(Value::as_str) {
        if let Some(reply) = waiting.remove(echo) {
            let _ = reply.send(Ok(value));
        }
        return;
    }

    // 拍一拍常不带 sender：先异步查群成员资料补昵称，再入库。查询线程通过
    // LinkSlot 发动作，由本线程继续读写回包，不会把 pump 卡死。
    if let Some((group, user)) = event::poke_missing_sender(&value) {
        spawn_poke_lookup(
            value,
            slot.clone(),
            connection.to_owned(),
            admin_qqs.to_vec(),
            events.clone(),
            group,
            user,
        );
        return;
    }

    submit_event(events, connection, &value, admin_qqs);
}

/// 解析并提交一条事件。正常路径和昵称补查线程都走这里。
fn submit_event(events: &EventQueue, connection: &str, value: &Value, admin_qqs: &[String]) {
    match event::parse(connection, value, admin_qqs) {
        Ok(Parsed::Record(draft)) => {
            if !events.submit(draft) {
                eprintln!("适配器 {connection}：入站队列已关闭，丢掉一条记录");
            }
        }
        Ok(Parsed::Ignored(reason)) => {
            // 心跳之类，不值得每次都打日志。
            if reason.starts_with("不认识") {
                eprintln!("适配器 {connection}：{reason}");
            }
        }
        Err(error) => eprintln!("适配器 {connection}：这条事件读不懂：{error}"),
    }
}

/// 查一次群成员资料，把 `sender` 注入原事件后再入库；查不到就沿用号码。
fn spawn_poke_lookup(
    raw: Value,
    slot: LinkSlot,
    connection: String,
    admin_qqs: Vec<String>,
    events: EventQueue,
    group: String,
    user: String,
) {
    std::thread::spawn(move || {
        let mut enriched = raw.clone();
        if let Ok(room) = RoomId::new(&connection, format!("group/{group}")) {
            let mut outbound = OneBotOutbound::new(slot, &connection);
            let params = json!({
                "group_id": number_value(&group),
                "user_id": number_value(&user),
            });
            match outbound.call(&room, "group.get_member_info", &params) {
                Ok(outcome) => {
                    let data = &outcome.value;
                    let nickname = data.get("nickname").and_then(Value::as_str).unwrap_or("");
                    let card = data.get("card").and_then(Value::as_str).unwrap_or("");
                    let role = data.get("role").and_then(Value::as_str).unwrap_or("");
                    if let Value::Object(map) = &mut enriched {
                        map.insert(
                            "sender".to_owned(),
                            json!({
                                "user_id": user,
                                "nickname": nickname,
                                "card": card,
                                "role": role,
                            }),
                        );
                    }
                }
                Err(error) => eprintln!(
                    "适配器 {connection}：查 {user} 的昵称失败，沿用号码：{error}"
                ),
            }
        }
        submit_event(&events, &connection, &enriched, &admin_qqs);
    });
}

/// 数字形式的号传数字，非数字（少见）原样传字符串。
fn number_value(text: &str) -> Value {
    text.parse::<u64>()
        .map(Value::from)
        .unwrap_or_else(|_| Value::from(text))
}

fn fail_all(waiting: &mut HashMap<String, Sender<Result<Value, LinkError>>>, reason: &str) {
    for (_, reply) in waiting.drain() {
        let _ = reply.send(Err(LinkError::Transport(reason.to_owned())));
    }
}

/// 守护一条平台连接：连不上就等着重试，断了就重连，直到要退出。
pub fn supervise<S: Store + Send + 'static>(
    slot: LinkSlot,
    url: String,
    token: Option<String>,
    connection: String,
    admin_qqs: Vec<String>,
    bridge: Arc<Mutex<Bridge<S>>>,
    shutdown: Arc<AtomicBool>,
    events: EventQueue,
) {
    while !shutdown.load(Ordering::Relaxed) {
        match connect(&url, token.as_deref(), READ_TICK) {
            Ok(ws) => {
                eprintln!("适配器 {connection}：已连上 {url}");
                let (sender, actions) = mpsc::channel();
                slot.install(sender.clone());

                // 顺手问一下对面登录的是哪个号：订阅里的 send_account 要是跟它对不上，
                // 自己说的话就会被当成别人说的，严重时会自己回自己。这种事必须当场喊出来。
                {
                    let sender = sender.clone();
                    let bridge = Arc::clone(&bridge);
                    let connection = connection.clone();
                    std::thread::spawn(move || probe_login(&sender, &bridge, &connection));
                }

                let outcome = pump(
                    ws,
                    actions,
                    &events,
                    &connection,
                    &admin_qqs,
                    &slot,
                    &shutdown,
                );
                slot.clear();
                if shutdown.load(Ordering::Relaxed) {
                    break;
                }
                match outcome {
                    Ok(()) => {
                        eprintln!("适配器 {connection}：连接关了，{RECONNECT_DELAY:?} 后重连")
                    }
                    Err(error) => {
                        eprintln!("适配器 {connection}：{error}，{RECONNECT_DELAY:?} 后重连")
                    }
                }
            }
            Err(error) => {
                eprintln!("适配器 {connection}：{error}");
            }
        }

        // 分小段睡，退出时不用等满一整个间隔。
        let mut waited = Duration::ZERO;
        while waited < RECONNECT_DELAY && !shutdown.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(100));
            waited += Duration::from_millis(100);
        }
    }
}

/// 问一下对面登录的是哪个号，顺手检查订阅里配的发送账号对不对。
fn probe_login<S: Store>(sender: &Sender<Action>, bridge: &Mutex<Bridge<S>>, connection: &str) {
    let (reply_tx, reply_rx) = mpsc::channel();
    let request = Action {
        body: serde_json::json!({"action": "get_login_info", "params": {}, "echo": "acb-probe"}),
        reply: reply_tx,
    };
    if sender.send(request).is_err() {
        return;
    }

    let Ok(Ok(value)) = reply_rx.recv_timeout(Duration::from_secs(5)) else {
        eprintln!("适配器 {connection}：问不出登录账号（不影响收发）");
        return;
    };

    let self_id = match value.get("data").and_then(|data| data.get("user_id")) {
        Some(Value::Number(number)) => number.to_string(),
        Some(Value::String(text)) => text.clone(),
        _ => {
            eprintln!("适配器 {connection}：对面没说登录账号（不影响收发）");
            return;
        }
    };
    eprintln!("适配器 {connection}：登录账号 {self_id}");

    let configured = {
        let guard = bridge.lock().unwrap_or_else(|poison| poison.into_inner());
        guard
            .config()
            .connection(connection)
            .map(|connection| connection.account.clone())
            .unwrap_or_default()
    };
    if !configured.is_empty() && configured != self_id {
        eprintln!(
            "注意：连接 {connection} 登录的是 {self_id}，但配置里写的是 {configured}。\
             这样自己发的消息会被当成别人说的，请把 account 改成 {self_id}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use acb_kernel::Config;

    #[test]
    fn endpoint_is_taken_from_the_url() {
        assert_eq!(
            endpoint_of("ws://127.0.0.1:3001").expect("合法"),
            "127.0.0.1:3001"
        );
        assert_eq!(
            endpoint_of("ws://127.0.0.1:3001/onebot/v11/ws").expect("合法"),
            "127.0.0.1:3001"
        );
        assert_eq!(
            endpoint_of("ws://example.com").expect("合法"),
            "example.com:80"
        );
    }

    #[test]
    fn query_string_is_not_part_of_the_address() {
        assert_eq!(
            endpoint_of("ws://127.0.0.1:3001?access_token=abc").expect("合法"),
            "127.0.0.1:3001"
        );
        assert_eq!(
            endpoint_of("ws://127.0.0.1:3001/ws?access_token=abc").expect("合法"),
            "127.0.0.1:3001"
        );
    }

    #[test]
    fn token_goes_into_the_authorization_header() {
        let request = build_request("ws://127.0.0.1:3001", Some("abc")).expect("能组请求");
        assert_eq!(
            request
                .headers()
                .get(tungstenite::http::header::AUTHORIZATION)
                .expect("应当有令牌头")
                .to_str()
                .expect("可见字符"),
            "Bearer abc"
        );
        assert_eq!(request.uri().path(), "/");
    }

    #[test]
    fn no_token_means_no_header_and_the_path_is_kept() {
        let request =
            build_request("ws://127.0.0.1:3001/onebot/v11/ws?x=1", None).expect("能组请求");
        assert!(
            request
                .headers()
                .get(tungstenite::http::header::AUTHORIZATION)
                .is_none()
        );
        assert_eq!(request.uri().path(), "/onebot/v11/ws");
        assert_eq!(request.uri().query(), Some("x=1"));
    }

    #[test]
    fn tls_urls_are_refused_with_a_clear_message() {
        assert!(matches!(
            endpoint_of("wss://example.com/ws"),
            Err(LinkError::BadUrl(_))
        ));
        assert!(matches!(endpoint_of("http://x"), Err(LinkError::BadUrl(_))));
    }

    #[test]
    fn the_configured_account_is_read_back_for_the_right_connection() {
        let config = Config::parse(
            r#"
            [[connections]]
            id = "qq"
            platform = "qq"
            kind = "onebot11"
            url = "ws://127.0.0.1:3001"
            account = "999"

            [[connections]]
            id = "qq2"
            platform = "qq"
            kind = "onebot11"
            url = "ws://127.0.0.1:3002"
            account = "888"

            [[devices]]
            id = "main"
            persona = "p"
            memory = "m"
            token = "t"

            [[devices.subscriptions]]
            connection = "qq"
            room = "group/1"
            "#,
        )
        .expect("配置合法");

        assert_eq!(config.connection("qq").expect("有").account, "999");
        assert_eq!(config.connection("qq2").expect("有").account, "888");
        assert!(config.connection("kook").is_none());
    }
}
