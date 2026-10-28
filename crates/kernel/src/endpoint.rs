//! 接入点：harness 连进来的那一端。
//!
//! 一条 TCP 长连接，一行一帧 JSON（见 `docs/04-protocol.md`）。连上来先握手：harness 报
//! 协议版本和接入令牌，令牌对上了，这条连接就绑到那个设备——之后这一端只为这个设备取活。
//!
//! 这里刻意分两层：
//! - [`Session`] 是纯状态机，不碰 IO，测试可以直接把帧喂给它；
//! - [`run_connection`] 负责把状态机接到 socket 上：有活就发 `turn_start`，来帧就交给状态机。
//!
//! 并发模型：整个桥在一把锁后面。锁只在「读一格状态 / 执行一个动作」时拿，**绝不**在等
//! 网络时拿着——所以出站慢、harness 发呆，都不会卡住平台那一侧。

use std::io::{BufRead, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use qqab_proto::frame::{decode, encode};
use qqab_proto::{Delivery, ErrorCode, Frame, LogLevel, Ticket, ToolError};
use thiserror::Error;

use crate::engine::{Bridge, BridgeError};
use crate::storage::Store;

/// 空闲时多久回头看一眼有没有活。越小越跟手，越大越省 CPU。
pub const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// 握手最多等多久。连上来又不说话的连接不能一直占着线程。
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

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

/// 握手：校验版本与令牌，返回这条连接绑定的设备。
pub fn handshake<S: Store>(bridge: &Bridge<S>, frame: Frame) -> Result<String, EndpointError> {
    let (version, token) = match frame {
        Frame::Hello { version, token } => (version, token),
        other => {
            return Err(EndpointError::Handshake(format!(
                "第一帧必须是 hello，收到 {}",
                other.kind()
            )));
        }
    };
    if !qqab_proto::frame::version_compatible(version) {
        return Err(EndpointError::Handshake(format!(
            "协议版本不兼容：对面是 {version}，这边是 {}",
            qqab_proto::frame::PROTOCOL_VERSION
        )));
    }
    let Some(token) = token else {
        return Err(EndpointError::Handshake("没有带接入令牌".into()));
    };
    let Some(device) = bridge.config().device_by_token(&token) else {
        return Err(EndpointError::Handshake("接入令牌无效".into()));
    };
    Ok(device.id.clone())
}

/// 一条连接上的协议状态机。不碰 IO，喂帧就行。
#[derive(Debug)]
pub struct Session {
    device: String,
    /// 这个设备当前这一轮的票；有票说明 harness 正在跑一轮。
    ticket: Option<Ticket>,
}

impl Session {
    pub fn new(device: impl Into<String>) -> Self {
        Self {
            device: device.into(),
            ticket: None,
        }
    }

    pub fn device(&self) -> &str {
        &self.device
    }

    /// 正在跑的那一轮的票。
    pub fn ticket(&self) -> Option<&Ticket> {
        self.ticket.as_ref()
    }

    /// 有活就领一轮过来；正忙、没活都返回 `None`。
    pub fn take_delivery<S: Store>(
        &mut self,
        bridge: &mut Bridge<S>,
    ) -> Result<Option<Delivery>, EndpointError> {
        if self.ticket.is_some() {
            return Ok(None);
        }
        let Some(delivery) = bridge.poll_turn_for(&self.device)? else {
            return Ok(None);
        };
        self.ticket = Some(delivery.ticket.clone());
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
                let reply = match &self.ticket {
                    Some(current) if current == &ticket => {
                        match bridge.tool_call(&ticket, &tool, args) {
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
                    }
                    _ => Frame::ToolResult {
                        id,
                        result: None,
                        error: Some(ToolError::new(
                            ErrorCode::Unauthorized,
                            "这张票不是本轮签发的",
                        )),
                    },
                };
                Ok(vec![reply])
            }
            Frame::TurnEnd { ticket, error } => {
                if self.ticket.as_ref() != Some(&ticket) {
                    return Ok(vec![Frame::Log {
                        level: LogLevel::Warn,
                        message: "收到了不属于当前轮次的 turn_end，已忽略".into(),
                    }]);
                }
                bridge.end_turn(&ticket)?;
                self.ticket = None;
                let mut replies = Vec::new();
                if let Some(error) = error {
                    replies.push(Frame::Log {
                        level: LogLevel::Warn,
                        message: format!("harness 报告本轮出错：{error}"),
                    });
                }
                Ok(replies)
            }
            Frame::Ping => Ok(vec![Frame::Pong]),
            Frame::Pong => Ok(Vec::new()),
            other => Err(EndpointError::Protocol(format!(
                "harness 不该发 {} 这种帧",
                other.kind()
            ))),
        }
    }

    /// 连接断了：把没交代完的一轮放掉，别让设备一直占着。
    ///
    /// 不确认任何消息——它们还是未读，下一轮重投。
    pub fn abandon<S: Store>(&mut self, bridge: &mut Bridge<S>) {
        if let Some(ticket) = self.ticket.take() {
            let _ = bridge.end_turn(&ticket);
        }
    }
}

/// 服务一条已经建立的连接，直到对端断开或出错。
pub fn run_connection<S: Store>(
    bridge: &Mutex<Bridge<S>>,
    stream: TcpStream,
) -> Result<(), EndpointError> {
    let _ = stream.set_nodelay(true);
    let mut reader = LineReader::new(stream.try_clone()?);
    let mut writer = stream;

    // 握手：第一帧必须是 hello。
    let device = {
        let line = match reader.next(HANDSHAKE_TIMEOUT)? {
            Incoming::Line(line) => line,
            Incoming::Idle => return Err(EndpointError::Handshake("握手超时".into())),
            Incoming::Closed => return Err(EndpointError::Handshake("还没握手就断了".into())),
        };
        let frame = decode(&line).map_err(|error| EndpointError::Protocol(error.to_string()))?;
        let guard = lock(bridge);
        handshake(&guard, frame)?
    };
    write_frame(
        &mut writer,
        &Frame::Ready {
            device: device.clone(),
        },
    )?;

    let mut session = Session::new(device);

    let outcome = 'conn: loop {
        // 1. 有活就发过去。
        if session.ticket().is_none() {
            let delivery = {
                let mut guard = lock(bridge);
                session.take_delivery(&mut guard)
            };
            match delivery {
                Ok(Some(delivery)) => {
                    if let Err(error) = write_frame(&mut writer, &Frame::TurnStart { delivery }) {
                        break 'conn Err(error);
                    }
                }
                Ok(None) => {}
                Err(error) => break 'conn Err(error),
            }
        }

        // 2. 看看 harness 说了什么。等不到就回头再看一眼有没有活。
        match reader.next(POLL_INTERVAL) {
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
                    if let Err(error) = write_frame(&mut writer, &reply) {
                        break 'conn Err(error);
                    }
                }
            }
            Err(error) => break 'conn Err(error),
        }
    };

    // 无论怎么结束，都要把没跑完的轮次放掉。
    {
        let mut guard = lock(bridge);
        session.abandon(&mut guard);
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
) -> Result<(), EndpointError> {
    listener.set_nonblocking(true)?;
    while !shutdown.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _)) => {
                // 接过来的连接要显式改回阻塞模式，读超时的逻辑建立在阻塞 socket 上。
                stream.set_nonblocking(false)?;
                let bridge = Arc::clone(&bridge);
                std::thread::spawn(move || {
                    // 一条连接坏了不影响别的连接。
                    let _ = run_connection(&bridge, stream);
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

/// 按行读，自带缓冲区。
///
/// 不直接用 `BufRead::read_line`：带读超时时它会「读了一半就报错」，半个帧的数据就丢了。
/// 这里把读到的字节攒着，下次接着攒，攒到换行才算一帧。
struct LineReader {
    stream: TcpStream,
    buffer: Vec<u8>,
}

impl LineReader {
    fn new(stream: TcpStream) -> Self {
        Self {
            stream,
            buffer: Vec::new(),
        }
    }

    fn next(&mut self, timeout: Duration) -> Result<Incoming, EndpointError> {
        self.stream.set_read_timeout(Some(timeout))?;
        loop {
            if let Some(position) = self.buffer.iter().position(|byte| *byte == b'\n') {
                let line: Vec<u8> = self.buffer.drain(..=position).collect();
                let text = String::from_utf8(line)
                    .map_err(|error| EndpointError::Protocol(format!("不是 UTF-8：{error}")))?;
                return Ok(Incoming::Line(text));
            }
            if self.buffer.len() > qqab_proto::frame::MAX_FRAME_BYTES {
                return Err(EndpointError::Protocol("帧太大，没有换行".into()));
            }

            let mut chunk = [0_u8; 8192];
            match self.stream.read(&mut chunk) {
                Ok(0) => {
                    return if self.buffer.is_empty() {
                        Ok(Incoming::Closed)
                    } else {
                        Err(EndpointError::Protocol("连接在半个帧处断了".into()))
                    };
                }
                Ok(read) => self.buffer.extend_from_slice(&chunk[..read]),
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
}

/// 给测试用：把一端连好的 socket 包成 [`Session`] 的对手方，方便直接写帧。
pub fn reader_of(stream: &TcpStream) -> std::io::BufReader<TcpStream> {
    std::io::BufReader::new(stream.try_clone().expect("能复制句柄"))
}

/// 给测试用：从连接上读一帧。
pub fn read_frame(reader: &mut impl BufRead) -> Result<Frame, EndpointError> {
    let mut line = String::new();
    reader.read_line(&mut line)?;
    decode(&line).map_err(|error| EndpointError::Protocol(error.to_string()))
}
