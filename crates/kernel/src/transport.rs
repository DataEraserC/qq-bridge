//! WebSocket 传输：把一条 OneBot 连接跑起来。
//!
//! 形状由 [ADR-0006](../../../docs/adr/0006-blocking-websocket-per-thread.md) 定：阻塞
//! `tungstenite`，每账号一个线程，不引入 async runtime。这一层做三件事——建连（含令牌）、
//! 读循环、写队列——然后把每一帧交给上面的纯逻辑。
//!
//! 两条不可省的细节：
//!
//! - **令牌走 `Authorization` 头。** OneBot 实现的访问令牌有 `Bearer <token>` 与裸 `<token>`
//!   两种写法，配置里的值原样放进 `Authorization`：带前缀的写法由部署者自己写全。
//! - **读循环不因单帧解码失败而退出。** 上游发来一条我们不认识的 JSON，丢掉它继续读；
//!   只有真正的传输错误（断开、握手失败）才结束这条连接。

use std::io::Error as IoError;
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use tungstenite::WebSocket;
use tungstenite::client::IntoClientRequest;
use tungstenite::protocol::Message as WsMessage;
use tungstenite::stream::MaybeTlsStream;

use crate::config::{Account, Secret};
use crate::conn::Pool;
use crate::onebot::{Action, DecodeError, Event};

/// 建连与握手的上限。超过就当作一次失败，交给连接池的退避。
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// socket 读超时。轮询动作队列的节奏，不是「多久算断线」——断线由读返回错误判断。
pub const READ_TIMEOUT: Duration = Duration::from_millis(200);

/// 一条连接上发生的事。
#[derive(Debug)]
pub enum Incoming {
    /// 解码成功的事件。
    Event(Box<Event>),
    /// 收到了但解不出来。`kind` 是给日志用的粗分类。
    Undecodable { reason: String },
    /// 读超时：这一段时间没有帧。调用方应回头检查是否要发动作。
    Timeout,
    /// 连接结束（正常关闭或传输错误），说明原因。
    Closed { reason: String },
}

/// 一条已建立的连接。
#[derive(Debug)]
pub struct Connection {
    socket: Sock,
    /// 已分配的号码，用于 `ActionResponse` 与 `deliver` 的请求标识。
    self_id: String,
    account: String,
}

impl Connection {
    /// 按配置建连。
    ///
    /// 失败即返回错误，由调用方记一次失败并交给退避——这一层不重试。
    pub fn open(account: &Account) -> Result<Self, ConnectError> {
        let mut request = account
            .ws_url
            .clone()
            .into_client_request()
            .map_err(|error| ConnectError::BadUrl {
                url: account.ws_url.clone(),
                detail: error.to_string(),
            })?;

        if let Some(secret) = &account.token {
            let value = resolve_token(secret)?;
            request.headers_mut().insert(
                "Authorization",
                value.parse().map_err(|_| ConnectError::BadToken)?,
            );
        }

        let stream = dial(&account.ws_url).map_err(|error| ConnectError::Handshake {
            url: account.ws_url.clone(),
            detail: error.to_string(),
        })?;

        // `wss://` 由 `tungstenite` 的 TLS 支持处理：请求里是 `wss` 就套一层 rustls，是
        // `ws` 就原样走明文。两个分支在同一个调用里，上层不必判断。
        let (socket, _response) =
            tungstenite::client_tls_with_config(request, stream, Some(build_config()), None)
                .map_err(|error| ConnectError::Handshake {
                    url: account.ws_url.clone(),
                    detail: error.to_string(),
                })?;

        set_read_timeout(&socket);

        Ok(Self {
            socket,
            self_id: account.self_id.clone(),
            account: account.id.clone(),
        })
    }

    /// 读一帧。阻塞到有数据、读超时或连接结束。
    ///
    /// 不叫 `next`：它不是迭代器，叫 `next` 会让人以为可以 `for` 起来。
    pub fn read_event(&mut self) -> Incoming {
        loop {
            let frame = match self.socket.read() {
                Ok(frame) => frame,
                Err(tungstenite::Error::Io(error))
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        || error.kind() == std::io::ErrorKind::TimedOut =>
                {
                    return Incoming::Timeout;
                }
                Err(error) => {
                    return Incoming::Closed {
                        reason: closed_reason(error),
                    };
                }
            };

            match frame {
                WsMessage::Text(text) => {
                    return match crate::onebot::decode(&text) {
                        Ok(event) => Incoming::Event(Box::new(event)),
                        Err(error) => Incoming::Undecodable {
                            reason: describe_decode_error(&error),
                        },
                    };
                }
                // 上游可能把帧发成分片；`tungstenite` 默认会拼好，走到这里的是控制帧。
                WsMessage::Binary(_) => continue,
                WsMessage::Ping(payload) => {
                    // 必须回 pong，否则对端会认为连接已死。
                    if let Err(error) = self.socket.send(WsMessage::Pong(payload)) {
                        return Incoming::Closed {
                            reason: closed_reason(error),
                        };
                    }
                }
                WsMessage::Close(_) => {
                    return Incoming::Closed {
                        reason: "对端关闭".to_owned(),
                    };
                }
                _ => continue,
            }
        }
    }

    /// 发一个动作。同步等结果——动作是短请求，不值得为它做异步化。
    ///
    /// 等响应的这段时间里上游可能照样推事件（群里不会因为你在发消息就安静）。这些事件
    /// **不能丢**，所以随结果一并交出，由调用方补投给上层。
    pub fn act(
        &mut self,
        action: &Action,
        echo: &str,
    ) -> (Result<ActionOutcome, WriteError>, Vec<Event>) {
        let mut carried = Vec::new();
        match self.act_inner(action, echo, &mut carried) {
            Ok(outcome) => (Ok(outcome), carried),
            Err(error) => (Err(error), carried),
        }
    }

    fn act_inner(
        &mut self,
        action: &Action,
        echo: &str,
        carried: &mut Vec<Event>,
    ) -> Result<ActionOutcome, WriteError> {
        let line = action.encode(echo);
        self.socket
            .send(WsMessage::Text(line.into()))
            .map_err(|source| WriteError {
                reason: closed_reason(source),
            })?;

        // 动作响应可能夹在事件之间：一路读到 echo 对上的那一帧，中间的事件收进 `carried`。
        loop {
            let frame = self.socket.read().map_err(|source| WriteError {
                reason: closed_reason(source),
            })?;
            let WsMessage::Text(text) = frame else {
                continue;
            };

            let Ok(response) = crate::onebot::ActionResponse::decode(&text) else {
                // 不是动作响应，是一条事件：留着，别丢。
                match crate::onebot::decode(&text) {
                    Ok(event) => carried.push(event),
                    Err(_) => carried.push(Event::Ignored {
                        kind: "undecodable".to_owned(),
                    }),
                }
                continue;
            };
            if response.echo.as_deref() != Some(echo) {
                continue;
            }

            return Ok(if response.is_ok() {
                ActionOutcome::Ok {
                    data: response.data.clone(),
                }
            } else {
                ActionOutcome::Rejected {
                    reason: response.reason(),
                }
            });
        }
    }

    pub fn account(&self) -> &str {
        &self.account
    }

    pub fn self_id(&self) -> &str {
        &self.self_id
    }
}

/// 动作的结论。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionOutcome {
    /// 上游接受，`data` 里通常带 `message_id`。
    Ok { data: Option<serde_json::Value> },
    /// 上游拒绝（`retcode != 0`）。不重试——同一个动作重发只会再被拒。
    Rejected { reason: String },
}

/// 发起一次动作的请求。传输线程持有一条连接，动作经这个通道排进去。
pub struct ActionRequest {
    pub action: Action,
    pub echo: String,
    pub reply: Sender<Result<ActionOutcome, WriteError>>,
}

/// 一条连接的所有权被移进线程后，留给外界的把手。
#[derive(Debug)]
pub struct Handle {
    pub account: String,
    pub actions: Sender<ActionRequest>,
    /// 请求线程退出。
    pub stop: Arc<Mutex<bool>>,
}

/// 建连失败的原因。
#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    #[error("WebSocket 地址无效：{url}（{detail}）")]
    BadUrl { url: String, detail: String },
    #[error("令牌环境变量未设置：{0}")]
    TokenEnv(String),
    #[error("令牌无法放进 HTTP 头")]
    BadToken,
    #[error("握手失败：{url}（{detail}）")]
    Handshake { url: String, detail: String },
}

/// 写失败：连接已经不可用。
#[derive(Debug, Clone, thiserror::Error)]
#[error("连接不可写：{reason}")]
pub struct WriteError {
    pub reason: String,
}

/// 把配置里的令牌落到字符串。`env:` 形式在启动期读环境，缺失即启动失败。
fn resolve_token(secret: &Secret) -> Result<String, ConnectError> {
    match secret {
        Secret::Literal(value) => Ok(value.clone()),
        Secret::Env(name) => std::env::var(name).map_err(|_| ConnectError::TokenEnv(name.clone())),
    }
}

fn build_config() -> WebSocketConfig {
    // 一帧上限放到 16 MiB：OneBot 的合并转发与长消息能很大，默认的 64 KiB 会直接断连。
    WebSocketConfig::default()
        .max_message_size(Some(16 * 1024 * 1024))
        .max_frame_size(Some(16 * 1024 * 1024))
}

/// 给底层 TCP 设读超时。
///
/// `tungstenite` 把 IO 错误原样透出，所以超时表现为 `WouldBlock`/`TimedOut`——读循环
/// 据此回头检查动作队列。没有这个超时，一个空闲群会让发送动作永远等不到执行。
fn set_read_timeout(socket: &Sock) {
    // TLS 之下仍是一条 TCP：读超时必须设在它上面。漏掉 `wss` 这一支的后果不是「读得慢」，
    // 是读永久阻塞——空闲群上永远等不到字节，动作队列就再也不被检查，发送全部卡死。
    let _ = match socket.get_ref() {
        MaybeTlsStream::Plain(stream) => stream.set_read_timeout(Some(READ_TIMEOUT)),
        MaybeTlsStream::Rustls(stream) => stream.sock.set_read_timeout(Some(READ_TIMEOUT)),
        // `non_exhaustive`：将来若再加一个 TLS 后端，这里必须回来补上分支。
        _ => Ok(()),
    };
}

fn closed_reason(error: tungstenite::Error) -> String {
    match error {
        tungstenite::Error::ConnectionClosed => "连接已关闭".to_owned(),
        tungstenite::Error::AlreadyClosed => "连接已关闭".to_owned(),
        other => other.to_string(),
    }
}

fn describe_decode_error(error: &DecodeError) -> String {
    match error {
        DecodeError::NotJson { .. } => "不是合法 JSON".to_owned(),
        DecodeError::NotObject => "不是 JSON 对象".to_owned(),
        DecodeError::MissingPostType => "缺少 post_type".to_owned(),
        DecodeError::MissingField(field) => format!("缺少 {field}"),
    }
}

/// 连接类型。底层是「可能套了 TLS 的 TCP」——`ws://` 与 `wss://` 共用这一种类型，
/// 由 `tungstenite` 在握手里选分支（见 ADR-0006 的后果一节）。
type Sock = WebSocket<MaybeTlsStream<TcpStream>>;
type WebSocketConfig = tungstenite::protocol::WebSocketConfig;

/// 建一个解析到 `ws_url` 的 TCP 连接，超时由 [`CONNECT_TIMEOUT`] 管。
///
/// 抽成函数是为了让「地址解析失败」与「握手失败」在错误里分得开。
fn dial(url: &str) -> Result<TcpStream, IoError> {
    let stripped = url
        .strip_prefix("ws://")
        .or_else(|| url.strip_prefix("wss://"))
        .unwrap_or(url);
    let host_port = stripped.split('/').next().unwrap_or(stripped);

    let address = host_port
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| IoError::new(std::io::ErrorKind::NotFound, "地址解析为空"))?;

    let stream = TcpStream::connect_timeout(&address, CONNECT_TIMEOUT)?;
    stream.set_nodelay(true)?;
    Ok(stream)
}

/// 起一条传输线程，返回把手。
///
/// `on_event` 在**该连接自己的线程**上被调用，所以调用方要保证它能跨线程安全使用
/// （通常是往一把 `Mutex` 里塞事件）。线程在 `stop` 置位或连接结束时可退出。
pub fn spawn(
    account: Account,
    connection: Connection,
    on_incoming: impl FnMut(Incoming) + Send + 'static,
) -> Handle {
    let (action_tx, action_rx) = channel::<ActionRequest>();
    let stop = Arc::new(Mutex::new(false));

    let account_id = account.id.clone();
    let stop_flag = Arc::clone(&stop);
    let account_for_thread = account.id.clone();

    let _ = connection.self_id();

    thread::spawn(move || {
        run(
            connection,
            action_rx,
            on_incoming,
            stop_flag,
            account_for_thread,
        )
    });

    Handle {
        account: account_id,
        actions: action_tx,
        stop,
    }
}

/// 读循环。
///
/// 单线程同时照看两个来源：一端是 socket（阻塞读），一端是动作队列。没有 async 就靠
/// **读超时**做多路复用——socket 设 200ms 读超时，超时就回来看看有没有动作要发。
///
/// 这是阻塞方案的代价，也是它唯一的复杂度：不是「事件驱动」，是「定时醒来看看」。换来
/// 的是一段直白的循环，不需要 executor。
fn run(
    mut connection: Connection,
    actions: Receiver<ActionRequest>,
    mut on_incoming: impl FnMut(Incoming),
    stop: Arc<Mutex<bool>>,
    _account: String,
) {
    loop {
        if *stop.lock().expect("stop 标志未中毒") {
            return;
        }

        // 先看有没有动作要发。发送延迟因此是「下一次读超时」，最多 200ms。
        match actions.try_recv() {
            Ok(request) => {
                let (outcome, carried) = connection.act(&request.action, &request.echo);
                let closed = outcome.is_err();
                let _ = request.reply.send(outcome);
                // 等响应期间收到的事件先补投，再处理新的事件。
                for event in carried {
                    on_incoming(Incoming::Event(Box::new(event)));
                }
                if closed {
                    on_incoming(Incoming::Closed {
                        reason: "动作写失败".to_owned(),
                    });
                    return;
                }
                continue;
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(std::sync::mpsc::TryRecvError::Disconnected) => return,
        }

        match connection.read_event() {
            // 读超时：没有事件，回到循环顶部再看一眼动作队列。
            Incoming::Timeout => continue,
            incoming => {
                let closed = matches!(incoming, Incoming::Closed { .. });
                on_incoming(incoming);
                if closed {
                    return;
                }
            }
        }
    }
}

/// 一个账号 + 它的传输把手。连接池只管状态，这一对是「实际能用的那条」。
#[derive(Debug)]
pub struct Linked {
    pub account: String,
    pub handle: Handle,
}

/// 用连接池与一组已建好的连接，挑出发送用的那条。
///
/// 按 `send` 列表的顺序尝试：第一个就绪的就是主用，退到下一个是轮换。
pub fn pick_sender<'a>(
    pool: &Pool,
    links: &'a [Linked],
    preferred: &[String],
) -> Option<&'a Linked> {
    preferred.iter().find_map(|account| {
        if !pool.state(account).is_some_and(|state| state.is_ready()) {
            return None;
        }
        links.iter().find(|link| &link.account == account)
    })
}
