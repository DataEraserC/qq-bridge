//! 接入点：harness 连进来的那一端。
//!
//! 一条 TCP 长连接，一行一帧 JSON（见 `docs/04-protocol.md`）。连上来先握手：harness 报
//! 协议版本、自己的名字（harness 名）和接入令牌，名字与令牌都对上，这条连接就绑到
//! 这个 harness 接管的那组会话——之后这一端只为这组会话取活、替这组会话说话（ADR-0017）。
//!
//! 这里刻意分两层：
//! - [`Conn`] 是纯状态机，不碰 IO，测试可以直接把帧喂给它；
//! - [`run_connection`] 负责把状态机接到 socket 上：有活就发 `turn_start`，来帧就交给状态机。
//!
//! 并发模型：整个桥在一把锁后面。锁只在「读一格状态 / 执行一个动作」时拿，**绝不**在等
//! 网络时拿着——所以出站慢、harness 发呆，都不会卡住平台那一侧。

use std::collections::HashMap;
use std::io::{BufRead, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use acb_proto::frame::{decode, encode};
use acb_proto::{Delivery, ErrorCode, Frame, LogLevel, Ticket, ToolError};
use thiserror::Error;

use crate::engine::{Bridge, BridgeError};
use crate::storage::Store;

/// 空闲时多久回头看一眼有没有活。越小越跟手，越大越省 CPU。
pub const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// 握手最多等多久。连上来又不说话的连接不能一直占着线程。
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// 控制指令注册表：会话 id → 给那个会话所在连接的出站指令通道。
///
/// 桥侧控制台要发 `control` 帧给某个 harness，得找到「管理那个会话的 Conn」。每个
/// Conn 在握手后把自己代表的每个会话注册进来（发帧的 sender），断连时注销。控制台
/// 从这里查到 sender，把 `control` 帧推过去，Conn 的循环里写出去。
#[derive(Default, Clone)]
pub struct ControlRegistry {
    senders: Arc<Mutex<std::collections::HashMap<String, std::sync::mpsc::Sender<Frame>>>>,
}

impl ControlRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册一组会话 → 通道。
    fn register(&self, sessions: &[String], sender: std::sync::mpsc::Sender<Frame>) {
        let mut map = self
            .senders
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for session in sessions {
            map.insert(session.clone(), sender.clone());
        }
    }

    fn unregister(&self, sessions: &[String]) {
        let mut map = self
            .senders
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for session in sessions {
            map.remove(session);
        }
    }

    /// 把一条控制指令推给管理 `session` 的连接。没有连接在管就返回 false。
    pub fn send_control(&self, frame: Frame) -> bool {
        let session = match &frame {
            Frame::Control { session, .. } => session.clone(),
            _ => return false,
        };
        let sender = {
            let map = self
                .senders
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            map.get(&session).cloned()
        };
        match sender {
            Some(sender) => sender.send(frame).is_ok(),
            None => false,
        }
    }
}

/// 接入点错误。
#[derive(Debug, Error)]
pub enum EndpointError {
    #[error("网络读写失败：{0}")]
    Io(#[from] std::io::Error),
    #[error("协议不对：{0}")]
    Protocol(String),
    #[error("握手失败：{0}")]
    Handshake(String),
    #[error(transparent)]
    Bridge(#[from] BridgeError),
}

/// 握手：校验版本、harness 名与令牌，返回这条连接代表的会话（配置顺序）。
pub fn handshake<S: Store>(bridge: &Bridge<S>, frame: Frame) -> Result<Vec<String>, EndpointError> {
    let (version, harness, token) = match frame {
        Frame::Hello {
            version,
            harness,
            token,
        } => (version, harness, token),
        other => {
            return Err(EndpointError::Handshake(format!(
                "第一帧必须是 hello，收到 {}",
                other.kind()
            )));
        }
    };
    if !acb_proto::frame::version_compatible(version) {
        return Err(EndpointError::Handshake(format!(
            "协议版本不兼容：对面是 {version}，这边是 {}",
            acb_proto::frame::PROTOCOL_VERSION
        )));
    }
    if harness.trim().is_empty() {
        return Err(EndpointError::Handshake("没有报 harness 名".into()));
    }
    let Some(token) = token else {
        return Err(EndpointError::Handshake("没有带接入令牌".into()));
    };
    // harness 名下的所有会话，再按令牌筛出「这次接入能代表的」。
    let sessions: Vec<String> = bridge
        .config()
        .sessions_by_harness(&harness)
        .into_iter()
        .filter(|session| session.token == token)
        .map(|session| session.id.clone())
        .collect();
    if sessions.is_empty() {
        return Err(EndpointError::Handshake("harness 名或接入令牌无效".into()));
    }
    Ok(sessions)
}

/// 一条连接上的协议状态机。不碰 IO，喂帧就行。
#[derive(Debug)]
pub struct Conn {
    /// 这条连接代表的会话组（同一 harness + 令牌）。
    sessions: Vec<String>,
    /// 正在跑的轮次的票，按会话存：一个会话一张票，不同会话可并行（ADR-0019）。
    tickets: HashMap<String, Ticket>,
}

impl Conn {
    pub fn new(sessions: Vec<String>) -> Self {
        Self {
            sessions,
            tickets: HashMap::new(),
        }
    }

    /// 这条连接代表的会话组。
    pub fn sessions(&self) -> &[String] {
        &self.sessions
    }

    /// 手上还有几张在跑的票。
    pub fn active_turns(&self) -> usize {
        self.tickets.len()
    }

    /// 有活就领一轮过来。引擎层自己会跳过「已经在跑」的会话，所以这里只管问。
    ///
    /// 一条连接可以同时跑多个会话的轮次：每个会话一张票，互不阻塞。
    pub fn take_delivery<S: Store>(
        &mut self,
        bridge: &mut Bridge<S>,
    ) -> Result<Option<Delivery>, EndpointError> {
        let Some(delivery) = bridge.poll_turn_for(&self.sessions)? else {
            return Ok(None);
        };
        self.tickets
            .insert(delivery.session.clone(), delivery.ticket.clone());
        Ok(Some(delivery))
    }

    /// 处理 harness 发来的一帧，返回要回给它的帧。
    ///
    /// 工具失败不关连接——作为 `tool_result` 的错误字段回过去，让 agent 自己看着办。
    pub fn handle<S: Store>(
        &mut self,
        bridge: &mut Bridge<S>,
        frame: Frame,
    ) -> Result<Vec<Frame>, EndpointError> {
        match frame {
            Frame::ToolCall {
                id,
                ticket,
                tool,
                args,
            } => {
                let reply = if self.tickets.values().any(|t| t == &ticket) {
                    match bridge.tool_call(&ticket, &self.sessions, &tool, args) {
                        Ok(result) => Frame::ToolResult {
                            id,
                            result: Some(result),
                            error: None,
                        },
                        Err(error) => Frame::ToolResult {
                            id,
                            result: None,
                            error: Some(error),
                        },
                    }
                } else {
                    Frame::ToolResult {
                        id,
                        result: None,
                        error: Some(ToolError::new(
                            ErrorCode::Unauthorized,
                            "这张票不是本轮签发的",
                        )),
                    }
                };
                Ok(vec![reply])
            }
            Frame::TurnEnd { ticket, error } => {
                // 找到这张票对应的会话，把它从「在跑」里摘掉。
                let owned = self
                    .tickets
                    .iter()
                    .find_map(|(session, t)| (t == &ticket).then(|| session.clone()));
                let Some(session) = owned else {
                    return Ok(vec![Frame::Log {
                        level: LogLevel::Warn,
                        message: "收到了不属于当前轮次的 turn_end，已忽略".into(),
                    }]);
                };
                self.tickets.remove(&session);
                bridge.end_turn(&ticket, error.as_deref())?;
                let mut replies = Vec::new();
                if let Some(error) = error {
                    replies.push(Frame::Log {
                        level: LogLevel::Warn,
                        message: format!("harness 报告本轮出错：{error}"),
                    });
                }
                Ok(replies)
            }
            // harness 回报控制指令结果：重置成功/失败，把会话映射落盘。
            Frame::ControlResult {
                id: _,
                session,
                command,
                result,
                error,
            } => {
                match command.as_str() {
                    "reset" => {
                        match error {
                            Some(err) => {
                                // 重置失败：不动映射，记一笔日志。
                                Ok(vec![Frame::Log {
                                    level: LogLevel::Warn,
                                    message: format!("重置会话 {session} 失败：{err}"),
                                }])
                            }
                            None => {
                                // 重置成功：删掉旧映射，等 harness 下次回报新映射。
                                let _ = bridge.delete_session_map(&session);
                                // 如果 harness 在 result 里直接给了新会话 id，就记下来。
                                if let Some(serde_json::Value::Object(map)) = &result {
                                    if let Some(serde_json::Value::String(new_id)) =
                                        map.get("session_id")
                                    {
                                        let _ = bridge.upsert_session_map(
                                            session.clone(),
                                            new_id.clone(),
                                            serde_json::json!({}),
                                        );
                                    }
                                }
                                Ok(vec![Frame::Log {
                                    level: LogLevel::Info,
                                    message: format!("会话 {session} 已重置"),
                                }])
                            }
                        }
                    }
                    "report_map" => {
                        // harness 主动回报映射（比如新建会话后）。
                        match error {
                            Some(err) => Ok(vec![Frame::Log {
                                level: LogLevel::Warn,
                                message: format!("会话 {session} 回报映射失败：{err}"),
                            }]),
                            None => {
                                if let Some(serde_json::Value::Object(map)) = &result {
                                    let session_id = map
                                        .get("session_id")
                                        .and_then(serde_json::Value::as_str)
                                        .unwrap_or_default();
                                    if !session_id.is_empty() {
                                        let _ = bridge.upsert_session_map(
                                            session.clone(),
                                            session_id.to_owned(),
                                            map.get("meta").cloned().unwrap_or(serde_json::json!({})),
                                        );
                                    }
                                }
                                Ok(Vec::new())
                            }
                        }
                    }
                    other => Ok(vec![Frame::Log {
                        level: LogLevel::Warn,
                        message: format!("不认识的 control_result 指令：{other}"),
                    }]),
                }
            }
            Frame::Ping => Ok(vec![Frame::Pong]),
            Frame::Pong => Ok(Vec::new()),
            other => Err(EndpointError::Protocol(format!(
                "harness 不该发 {} 这种帧",
                other.kind()
            ))),
        }
    }

    /// 连接断了：把没交代完的轮次都放掉，别让会话一直占着。
    ///
    /// 不确认任何消息——它们还是未读，下一轮重投。
    pub fn abandon<S: Store>(&mut self, bridge: &mut Bridge<S>) {
        let tickets: Vec<Ticket> = self.tickets.values().cloned().collect();
        for ticket in tickets {
            let _ = bridge.end_turn(&ticket, None);
        }
        self.tickets.clear();
    }
}

/// 服务一条已经建立的连接，直到对端断开或出错。
pub fn run_connection<S: Store>(
    bridge: &Mutex<Bridge<S>>,
    mut stream: TcpStream,
    controls: &ControlRegistry,
) -> Result<(), EndpointError> {
    let _ = stream.set_nodelay(true);
    // 一个句柄读写到底，不复制 fd：读和写在同一个线程里本来就是串行的，
    // 复制一份除了多一个要管的东西没别的用。
    let mut buffer: Vec<u8> = Vec::new();

    // 握手：第一帧必须是 hello。
    let sessions = {
        let line = match read_line(&mut stream, &mut buffer, HANDSHAKE_TIMEOUT)? {
            Incoming::Line(line) => line,
            Incoming::Idle => return Err(EndpointError::Handshake("握手超时".into())),
            Incoming::Closed => return Err(EndpointError::Handshake("还没握手就断了".into())),
        };
        let frame = decode(&line).map_err(|error| EndpointError::Protocol(error.to_string()))?;
        let guard = lock(bridge);
        handshake(&guard, frame)?
    };
    // 把这条连接代表会话的已落盘映射带上，让 harness 跨重启复用旧会话（ADR-0018）。
    let maps = {
        let guard = lock(bridge);
        guard
            .session_maps()
            .map(|all| {
                all.into_iter()
                    .filter(|map| sessions.contains(&map.session))
                    .map(|map| acb_proto::SessionMapRef {
                        session: map.session,
                        harness_session: map.harness_session,
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    };
    write_frame(
        &mut stream,
        &Frame::Ready {
            sessions: sessions.clone(),
            maps,
        },
    )?;

    // 连上就吭一声：以后「它不理我」时，一眼能看出是不是这里断了。
    if let Ok(peer) = stream.peer_addr() {
        eprintln!(
            "接入点：harness 已连上（{}），代表会话组 [{}]",
            peer,
            sessions.join("、")
        );
    }

    let mut session = Conn::new(sessions);

    // 控制指令通道：本连接把「代表哪些会话」的 sender 注册出去，控制台据此推 control 帧。
    let (control_tx, control_rx) = std::sync::mpsc::channel::<Frame>();
    controls.register(&session.sessions().to_vec(), control_tx);

    let outcome = 'conn: loop {
        // 1. 有活就发过去。引擎层会跳过「已经在跑」的会话，所以一条连接能同时
        //    跑多个会话的轮次——每个会话一张票，互不阻塞（ADR-0019）。
        {
            let delivery = {
                let mut guard = lock(bridge);
                session.take_delivery(&mut guard)
            };
            match delivery {
                Ok(Some(delivery)) => {
                    if let Err(error) = write_frame(&mut stream, &Frame::TurnStart { delivery }) {
                        break 'conn Err(error);
                    }
                    // 领到一轮后别立刻又领同一会话的下一轮，先回头处理 harness 的响应；
                    // 别的会话的活下一圈再领。
                    continue;
                }
                Ok(None) => {}
                Err(error) => break 'conn Err(error),
            }
        }

        // 2. 控制台推来的控制指令，直接写出去。
        loop {
            match control_rx.try_recv() {
                Ok(frame) => {
                    if let Err(error) = write_frame(&mut stream, &frame) {
                        break 'conn Err(error);
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
            }
        }

        // 3. 看看 harness 说了什么。等不到就回头再看一眼有没有活。
        match read_line(&mut stream, &mut buffer, POLL_INTERVAL) {
            Ok(Incoming::Idle) => continue,
            Ok(Incoming::Closed) => break 'conn Ok(()),
            Ok(Incoming::Line(line)) => {
                let frame = match decode(&line) {
                    Ok(frame) => frame,
                    Err(error) => break 'conn Err(EndpointError::Protocol(error.to_string())),
                };
                let replies = {
                    let mut guard = lock(bridge);
                    match session.handle(&mut guard, frame) {
                        Ok(replies) => replies,
                        Err(error) => break 'conn Err(error),
                    }
                };
                for reply in replies {
                    if let Err(error) = write_frame(&mut stream, &reply) {
                        break 'conn Err(error);
                    }
                }
            }
            Err(error) => break 'conn Err(error),
        }
    };

    // 断连就注销控制指令通道，别让控制台往死通道上推。
    controls.unregister(&session.sessions().to_vec());

    // 无论怎么结束，都要把没跑完的轮次放掉。
    let queued = {
        let mut guard = lock(bridge);
        session.abandon(&mut guard);
        guard.queued_turns()
    };

    // 断开也说清楚为什么断的：正常收工、对端关了、还是网络出错。
    // 顺带报一下队列：要是断的时候还压着活，就是「没人接了」，一眼能看出。
    match &outcome {
        Ok(()) => eprintln!(
            "接入点：harness 断开（会话组 [{}]）；队列里还压着 {queued} 个待开轮次",
            session.sessions().join("、")
        ),
        Err(error) => eprintln!(
            "接入点：harness 连接异常结束（会话组 [{}]）：{error}；队列里还压着 {queued} 个待开轮次",
            session.sessions().join("、")
        ),
    }
    outcome
}

/// 守着监听端口，来一个连一个，每个连接一个线程。
///
/// `shutdown` 置真后不再接新连接，下次检查时退出（现有连接各自跑完）。
pub fn serve<S: Store + Send + 'static>(
    listener: TcpListener,
    bridge: Arc<Mutex<Bridge<S>>>,
    shutdown: Arc<AtomicBool>,
    controls: ControlRegistry,
) -> Result<(), EndpointError> {
    listener.set_nonblocking(true)?;
    while !shutdown.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _)) => {
                // 接过来的连接要显式改回阻塞模式，读超时的逻辑建立在阻塞 socket 上。
                stream.set_nonblocking(false)?;
                let bridge = Arc::clone(&bridge);
                let controls = controls.clone();
                std::thread::spawn(move || {
                    // 一条连接坏了不影响别的连接，但要说清楚为什么坏的。
                    if let Err(error) = run_connection(&bridge, stream, &controls) {
                        eprintln!("接入点：一条连接结束了：{error}");
                    }
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => {
                // 单个连接建不起来不算致命，接着等下一个。
                eprintln!("接入点：接受连接失败：{error}");
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
    Ok(())
}

/// 桥的锁。被毒死（别的线程 panic）也接着用——桥的状态没有「半条」这种中间态。
fn lock<S: Store>(bridge: &Mutex<Bridge<S>>) -> MutexGuard<'_, Bridge<S>> {
    bridge
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn write_frame(writer: &mut TcpStream, frame: &Frame) -> Result<(), EndpointError> {
    let line = encode(frame).map_err(|error| EndpointError::Protocol(error.to_string()))?;
    writer.write_all(line.as_bytes())?;
    writer.flush()?;
    Ok(())
}

/// 读一行的结果。超时和断开是两件不同的事，别混在一起。
enum Incoming {
    Line(String),
    Idle,
    Closed,
}

/// 按行读一帧，自带缓冲区。
///
/// 不直接用 `BufRead::read_line`：带读超时时它会「读了一半就报错」，半个帧的数据就丢了。
/// 这里把读到的字节攒着，下次接着攒，攒到换行才算一帧。
fn read_line(
    stream: &mut TcpStream,
    buffer: &mut Vec<u8>,
    timeout: Duration,
) -> Result<Incoming, EndpointError> {
    stream.set_read_timeout(Some(timeout))?;
    loop {
        if let Some(position) = buffer.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = buffer.drain(..=position).collect();
            let text = String::from_utf8(line)
                .map_err(|error| EndpointError::Protocol(format!("不是 UTF-8：{error}")))?;
            return Ok(Incoming::Line(text));
        }
        if buffer.len() > acb_proto::frame::MAX_FRAME_BYTES {
            return Err(EndpointError::Protocol("帧太大，没有换行".into()));
        }

        let mut chunk = [0_u8; 8192];
        match stream.read(&mut chunk) {
            Ok(0) => {
                return if buffer.is_empty() {
                    Ok(Incoming::Closed)
                } else {
                    Err(EndpointError::Protocol("连接在半个帧处断了".into()))
                };
            }
            Ok(read) => buffer.extend_from_slice(&chunk[..read]),
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.kind() == std::io::ErrorKind::TimedOut =>
            {
                return Ok(Incoming::Idle);
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(EndpointError::Io(error)),
        }
    }
}

/// 给测试用：把一端连好的 socket 包成 [`Conn`] 的对手方，方便直接写帧。
pub fn reader_of(stream: &TcpStream) -> std::io::BufReader<TcpStream> {
    std::io::BufReader::new(stream.try_clone().expect("能复制句柄"))
}

/// 给测试用：从连接上读一帧。
pub fn read_frame(reader: &mut impl BufRead) -> Result<Frame, EndpointError> {
    let mut line = String::new();
    reader.read_line(&mut line)?;
    decode(&line).map_err(|error| EndpointError::Protocol(error.to_string()))
}
