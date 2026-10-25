//! 适配器接入：内核侧的监听端。
//!
//! 一侧是内核，一侧是适配器。协议见 `docs/architecture/05-protocol.md`——每行一个 JSON
//! 对象，未知 `type` 忽略而不是断连。
//!
//! **平台无关是硬要求。** 监听形态由 [`Endpoint`] 决定（文件路径 / 命名空间 / TCP），
//! 前两类由 `interprocess` 落到各平台的本地套接字，TCP 则跨机器。本模块里没有
//! `#[cfg(unix)]` 分支：同一份代码在 Linux、macOS、Windows 上编译并运行。
//!
//! 线程模型与 [ADR-0006] 一致：每条接入连接一个线程，**非阻塞 + 轮询**同时照看读与写。
//!
//! 为什么不是「读超时」：Windows 命名管道不支持 IO 超时（`interprocess` 的
//! `set_recv_timeout` 在那里直接返回 `Unsupported`），用超时就得写两套分支。非阻塞 +
//! `WouldBlock` 轮询在两个平台上语义相同，于是只有一套。
//!
//! 写也是非阻塞的：对端不读时不能让内核卡死在一条连接上。攒在 `pending` 里，下一轮继续推；
//! 攒过头就断开那条连接，而不是无限吃内存。
//!
//! 断开要报给内核：适配器崩溃时进行中的回合视为失败，由内核重投。所以读到 EOF 或传输
//! 错误一律发 [`AdapterEvent::Closed`]，不静默退出。
//!
//! [ADR-0006]: ../../../docs/adr/0006-blocking-websocket-per-thread.md

use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::path::Path;
use std::sync::mpsc::{Sender, TryRecvError, channel};
use std::thread;
use std::time::{Duration, Instant};

use interprocess::local_socket::traits::{Listener as _, Stream as _, StreamCommon as _};
use interprocess::local_socket::{
    GenericFilePath, GenericNamespaced, Listener as LocalListener, ListenerOptions, Stream,
    ToFsName, ToNsName,
};
use qqab_proto::{DecodeError, Frame, decode, encode};

use crate::endpoint::Endpoint;

/// 轮询间隔。这是「多久看一眼有没有新字节」，不表示「多久算断线」——断线由读到 EOF 或
/// 非重试类错误判断。
pub const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// 多久没收到任何字节就判定连接已死。
///
/// 适配器每条连接上都有心跳（`ping`/`pong`），真正活着的连接不会沉默这么久；给足余量
/// 避免误杀。半开连接（对端断电、网线拔掉）不会自己报错，只能靠这个数时间。
pub const LIVENESS_WINDOW: Duration = Duration::from_secs(120);

/// 单条连接一次最多读多少字节。剩下的留给下一轮。
const READ_CHUNK: usize = 8192;

/// 单行上限。协议帧是 JSON，超过这个大小的一定不是我们想要的帧。
const MAX_LINE: usize = 16 * 1024 * 1024;

/// 一条连接可以攒多少待发字节。对端长时间不读时断开它。
const MAX_PENDING_WRITE: usize = 8 * 1024 * 1024;

/// 服务端向内核上报的事件。
#[derive(Debug)]
pub enum AdapterEvent {
    /// 新连接已接受。`out` 用来给这条连接发帧。
    Connected {
        conn: u64,
        out: Sender<String>,
        /// 对端地址，仅用于日志。
        peer: String,
    },
    /// 收到一帧。
    ///
    /// `frame` 装箱：`Frame` 因为 `deliver` 一族字段而很大，不装箱会让整个
    /// 枚举按最大的变体分配，每条拍一拍、每次时钟推进都跟着付这份内存。
    Frame { conn: u64, frame: Box<Frame> },
    /// 收到一行但解不出来。内核丢弃它，连接继续。
    Undecodable { conn: u64, reason: String },
    /// 连接结束。
    Closed { conn: u64, reason: String },
}

/// 一条已接受的连接。两种形态都实现 `Read + Write` 与非阻塞切换，上层不必区分。
#[derive(Debug)]
pub enum Conn {
    /// Unix 域套接字或 Windows 命名管道。
    Local(Stream),
    /// TCP。跨机器时唯一可行的形态。
    Tcp(TcpStream),
}

impl Conn {
    fn set_nonblocking(&self, nonblocking: bool) -> std::io::Result<()> {
        match self {
            // `interprocess` 的 `Stream` 在 Unix 上是套接字、Windows 上是管道，两者都支持。
            Self::Local(stream) => stream.set_nonblocking(nonblocking),
            Self::Tcp(stream) => stream.set_nonblocking(nonblocking),
        }
    }

    fn peer(&self) -> String {
        match self {
            Self::Local(stream) => stream
                .peer_creds()
                .map(|creds| format!("{creds:?}"))
                .unwrap_or_else(|_| "本地".to_owned()),
            Self::Tcp(stream) => stream
                .peer_addr()
                .map(|addr| addr.to_string())
                .unwrap_or_else(|_| "TCP".to_owned()),
        }
    }
}

impl Read for Conn {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::Local(stream) => stream.read(buf),
            Self::Tcp(stream) => stream.read(buf),
        }
    }
}

impl Write for Conn {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Self::Local(stream) => stream.write(buf),
            Self::Tcp(stream) => stream.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Self::Local(stream) => stream.flush(),
            Self::Tcp(stream) => stream.flush(),
        }
    }
}

/// 已绑定的服务端。`bind` 之后由 [`Server::spawn`] 起接受循环。
pub struct Server {
    listener: Listener,
    endpoint: Endpoint,
}

enum Listener {
    Local(LocalListener),
    Tcp(TcpListener),
}

impl Server {
    /// 按接入点绑定。
    ///
    /// 路径形态会先清掉同名旧文件：内核异常退出会留下一个没人监听的套接字文件，不删就
    /// 永远绑定不上。命名空间与 TCP 不受此影响。
    pub fn bind(endpoint: &Endpoint) -> Result<Self, ServerError> {
        match endpoint {
            Endpoint::Path(path) => {
                // 只有真正的目录才需要预先建出来。Windows 的 `\\.\pipe\` 前缀在路径上
                // 也有「父路径」，但那是命名空间而不是可创建的目录，对它调 `create_dir_all`
                // 只会失败。所以这里按形态判断，而不是按平台写两套代码。
                if !is_pipe_namespace(path)
                    && let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty())
                {
                    std::fs::create_dir_all(parent).map_err(|source| ServerError::Prepare {
                        endpoint: endpoint.describe(),
                        detail: source.to_string(),
                    })?;
                }
                // 旧文件可能是上次崩溃留下的；`try_overwrite` 才是兜底。
                if path.exists() {
                    let _ = std::fs::remove_file(path);
                }
                let listener = local_listener(path, endpoint)?;
                Ok(Self {
                    listener: Listener::Local(listener),
                    endpoint: endpoint.clone(),
                })
            }
            Endpoint::Namespaced(name) => {
                let listener = namespaced_listener(name, endpoint)?;
                Ok(Self {
                    listener: Listener::Local(listener),
                    endpoint: endpoint.clone(),
                })
            }
            Endpoint::Tcp { host, port } => {
                let address = (host.as_str(), *port)
                    .to_socket_addrs()
                    .map_err(|source| ServerError::Prepare {
                        endpoint: endpoint.describe(),
                        detail: format!("地址解析失败：{source}"),
                    })?
                    .next()
                    .ok_or_else(|| ServerError::Prepare {
                        endpoint: endpoint.describe(),
                        detail: "地址解析为空".to_owned(),
                    })?;
                let listener = TcpListener::bind(address).map_err(|source| ServerError::Bind {
                    endpoint: endpoint.describe(),
                    detail: source.to_string(),
                })?;
                Ok(Self {
                    listener: Listener::Tcp(listener),
                    endpoint: endpoint.clone(),
                })
            }
        }
    }

    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// 起接受循环，每条连接一个线程。
    pub fn spawn(self, tx: Sender<AdapterEvent>) {
        thread::spawn(move || {
            let mut next_id = 0u64;
            loop {
                let accepted = match &self.listener {
                    Listener::Local(listener) => listener.accept().map(Conn::Local),
                    Listener::Tcp(listener) => listener.accept().map(|(stream, _)| {
                        // 跨机器的连接要尽快给小包，否则一条消息会被拆成多次往返。
                        let _ = stream.set_nodelay(true);
                        Conn::Tcp(stream)
                    }),
                };
                let Ok(conn) = accepted else {
                    // 单次 accept 失败通常是瞬时的（对端在握手前放弃）；继续等下一个。
                    continue;
                };
                next_id += 1;
                let id = next_id;
                let peer = conn.peer();
                let tx = tx.clone();
                thread::spawn(move || serve_conn(id, conn, peer, tx));
            }
        });
    }
}

/// 这条路径是不是 Windows 的命名管道命名空间。
///
/// `\\.\pipe\` 之下不是文件系统，任何「建目录」「删文件」的动作都不适用。判据用前缀而
/// 不是 `cfg!(windows)`：同一段代码可能被部署脚本在另一个平台上预检。
fn is_pipe_namespace(path: &Path) -> bool {
    let text = path.as_os_str().to_string_lossy().to_ascii_lowercase();
    text.starts_with(r"\\.\pipe\") || text.starts_with(r"\pipe\")
}

fn local_listener(path: &Path, endpoint: &Endpoint) -> Result<LocalListener, ServerError> {
    let name = path
        .to_fs_name::<GenericFilePath>()
        .map_err(|source| ServerError::BadName {
            endpoint: endpoint.describe(),
            detail: source.to_string(),
        })?;
    ListenerOptions::new()
        .name(name)
        .try_overwrite(true)
        .create_sync()
        .map_err(|source| ServerError::Bind {
            endpoint: endpoint.describe(),
            detail: source.to_string(),
        })
}

fn namespaced_listener(name: &str, endpoint: &Endpoint) -> Result<LocalListener, ServerError> {
    let name = name
        .to_ns_name::<GenericNamespaced>()
        .map_err(|source| ServerError::BadName {
            endpoint: endpoint.describe(),
            detail: source.to_string(),
        })?;
    ListenerOptions::new()
        .name(name)
        .create_sync()
        .map_err(|source| ServerError::Bind {
            endpoint: endpoint.describe(),
            detail: source.to_string(),
        })
}

impl Drop for Server {
    fn drop(&mut self) {
        // 文件形态的套接字关闭后不会自动消失；其它形态无此问题，交给各平台回收。
        if let (Endpoint::Path(path), Listener::Local(_)) = (&self.endpoint, &self.listener) {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// 一条连接的双向循环。
///
/// 每轮三件事，顺序固定：**推完待发字节 → 读一批字节 → 切行**。先推再读，保证对端在等
/// 我们回话时不会被我们自己的读阻塞住。
fn serve_conn(id: u64, mut conn: Conn, peer: String, tx: Sender<AdapterEvent>) {
    if conn.set_nonblocking(true).is_err() {
        let _ = tx.send(AdapterEvent::Closed {
            conn: id,
            reason: "无法设为非阻塞".to_owned(),
        });
        return;
    }

    let (out_tx, out_rx) = channel::<String>();
    if tx
        .send(AdapterEvent::Connected {
            conn: id,
            out: out_tx,
            peer,
        })
        .is_err()
    {
        return;
    }

    let mut buffer: Vec<u8> = Vec::new();
    let mut pending: Vec<u8> = Vec::new();
    let mut read_chunk = [0u8; READ_CHUNK];
    let mut last_read = Instant::now();

    loop {
        // ---- 推：把内核排进来的帧追加到待发缓冲 ----
        loop {
            match out_rx.try_recv() {
                Ok(line) => {
                    if pending.len() + line.len() + 1 > MAX_PENDING_WRITE {
                        let _ = tx.send(AdapterEvent::Closed {
                            conn: id,
                            reason: "对端不读，待发字节超限".to_owned(),
                        });
                        return;
                    }
                    pending.extend_from_slice(line.as_bytes());
                    pending.push(b'\n');
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    // 内核放弃了这条连接。但它可能刚刚排进去一帧「为什么放弃」——
                    // 比如 `hello_reject`。排空缓冲再走，否则对端只看到连接被关，
                    // 不知道原因。
                    push_pending(&mut conn, &mut pending);
                    let _ = tx.send(AdapterEvent::Closed {
                        conn: id,
                        reason: "内核关闭了写出通道".to_owned(),
                    });
                    return;
                }
            }
        }

        // ---- 推：尽量把待发字节写出去 ----
        let mut write_failed = false;
        while !pending.is_empty() {
            match conn.write(&pending) {
                Ok(0) => {
                    let _ = tx.send(AdapterEvent::Closed {
                        conn: id,
                        reason: "对端不再接收".to_owned(),
                    });
                    return;
                }
                Ok(count) => {
                    pending.drain(..count);
                }
                Err(error) if is_retryable(&error) => {
                    write_failed = true;
                    break;
                }
                Err(error) => {
                    let _ = tx.send(AdapterEvent::Closed {
                        conn: id,
                        reason: format!("写失败：{error}"),
                    });
                    return;
                }
            }
        }
        if !write_failed {
            let _ = conn.flush();
        }

        // ---- 读：读到多少算多少，按换行切完整行 ----
        match conn.read(&mut read_chunk) {
            Ok(0) => {
                let _ = tx.send(AdapterEvent::Closed {
                    conn: id,
                    reason: "对端关闭".to_owned(),
                });
                return;
            }
            Ok(count) => {
                last_read = Instant::now();
                buffer.extend_from_slice(&read_chunk[..count]);
                if buffer.len() > MAX_LINE {
                    let _ = tx.send(AdapterEvent::Closed {
                        conn: id,
                        reason: format!("单行超过 {MAX_LINE} 字节"),
                    });
                    return;
                }
                while let Some(index) = buffer.iter().position(|byte| *byte == b'\n') {
                    let line: Vec<u8> = buffer.drain(..=index).collect();
                    deliver_line(id, &line[..line.len() - 1], &tx);
                }
            }
            // 没有新字节。轮询是常态，不是异常。
            Err(error) if is_retryable(&error) => {}
            Err(error) => {
                let _ = tx.send(AdapterEvent::Closed {
                    conn: id,
                    reason: format!("读失败：{error}"),
                });
                return;
            }
        }

        if last_read.elapsed() > LIVENESS_WINDOW {
            let _ = tx.send(AdapterEvent::Closed {
                conn: id,
                reason: format!("{LIVENESS_WINDOW:?} 内没有任何字节"),
            });
            return;
        }

        thread::sleep(POLL_INTERVAL);
    }
}

/// 尽力把待发字节推出去，推不完就放弃。
///
/// 只在「连接已经要关了」时用：这时候还愿意等一小会儿，是因为那一帧往往正是关闭的原因，
/// 值得让对端收到。不无限等——对端不读的话这时候也没救了。
fn push_pending(conn: &mut Conn, pending: &mut Vec<u8>) {
    let deadline = Instant::now() + Duration::from_millis(200);
    while !pending.is_empty() && Instant::now() < deadline {
        match conn.write(pending) {
            Ok(0) => return,
            Ok(count) => {
                pending.drain(..count);
            }
            Err(error) if is_retryable(&error) => thread::sleep(POLL_INTERVAL),
            Err(_) => return,
        }
    }
    let _ = conn.flush();
}

/// 把一行交给解码器。
///
/// 坏行只报告、不关闭连接：对端可能是新版本发了我们不认识的帧，也可能是它自己出了 bug。
/// 两种情况下断连都会让一个可恢复的问题变成失联。
fn deliver_line(id: u64, line: &[u8], tx: &Sender<AdapterEvent>) {
    let text = String::from_utf8_lossy(line);
    if text.trim().is_empty() {
        return;
    }
    match decode(&text) {
        Ok(frame) => {
            let _ = tx.send(AdapterEvent::Frame {
                conn: id,
                frame: Box::new(frame),
            });
        }
        Err(error) => {
            let _ = tx.send(AdapterEvent::Undecodable {
                conn: id,
                reason: describe(&error),
            });
        }
    }
}

/// 该不该把它当成「暂时没有数据」而不是错误。
///
/// 超时也算：TCP 与 Unix 套接字在非阻塞下报 `WouldBlock`，而某些平台的管道在边界情况下
/// 会报 `TimedOut`。两者含义相同。
fn is_retryable(error: &std::io::Error) -> bool {
    matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut)
}

fn describe(error: &DecodeError) -> String {
    match error {
        DecodeError::NotJson { .. } => "不是合法 JSON".to_owned(),
        DecodeError::NotObject => "不是 JSON 对象".to_owned(),
        DecodeError::MissingType => "缺少 type".to_owned(),
        DecodeError::BadShape { .. } => "帧结构与 type 不符".to_owned(),
    }
}

/// 把一个帧编成一行（不含换行符）。失败时返回原因，调用方据此记日志。
pub fn line_for(frame: &Frame) -> Result<String, String> {
    encode(frame).map_err(|error| error.to_string())
}

/// 绑定失败的原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerError {
    /// 目录建不出来、地址解析不了。
    Prepare { endpoint: String, detail: String },
    /// 接入点名字不被当前平台接受。
    BadName { endpoint: String, detail: String },
    /// 绑不上：端口/路径被占、权限不足。
    Bind { endpoint: String, detail: String },
}

impl std::fmt::Display for ServerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Prepare { endpoint, detail } => {
                write!(f, "接入点准备失败：{endpoint}（{detail}）")
            }
            Self::BadName { endpoint, detail } => {
                write!(f, "接入点名字不被本平台接受：{endpoint}（{detail}）")
            }
            Self::Bind { endpoint, detail } => {
                write!(f, "接入点绑定失败：{endpoint}（{detail}）")
            }
        }
    }
}

impl std::error::Error for ServerError {}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::mpsc::Receiver;

    /// 测试用接入点：临时目录下的唯一文件名。
    ///
    /// Windows 上 `GenericFilePath` 只接受 `\\.\pipe\` 开头的路径，所以这里在 Windows 上
    /// 落到管道命名空间——同一份测试在两个平台上都成立。
    fn temp_endpoint(tag: &str) -> Endpoint {
        let unique = format!("qqab-{tag}-{}", std::process::id());
        if cfg!(windows) {
            Endpoint::Namespaced(unique)
        } else {
            Endpoint::Path(std::env::temp_dir().join(unique))
        }
    }

    fn recv(rx: &Receiver<AdapterEvent>) -> AdapterEvent {
        rx.recv_timeout(Duration::from_secs(5)).expect("应收到事件")
    }

    /// 测试当适配器：连到内核绑的接入点上。
    fn connect(endpoint: &Endpoint) -> Conn {
        let stream = match endpoint {
            Endpoint::Path(path) => {
                let name = path
                    .as_path()
                    .to_fs_name::<GenericFilePath>()
                    .expect("路径应可转成名字");
                Stream::connect(name).expect("应能连上")
            }
            Endpoint::Namespaced(name) => {
                let name = name
                    .as_str()
                    .to_ns_name::<GenericNamespaced>()
                    .expect("名字应可转成命名空间");
                Stream::connect(name).expect("应能连上")
            }
            other => panic!("测试不走 TCP 形态，实际 {other:?}"),
        };
        // 客户端也设非阻塞，测试才不会因为等字节而挂住。
        stream.set_nonblocking(true).expect("应能设非阻塞");
        Conn::Local(stream)
    }

    /// 非阻塞读到一个完整行为止。轮询，不猜一次读够。
    fn read_line(conn: &mut Conn) -> String {
        let mut got = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut chunk = [0u8; 4096];
        while Instant::now() < deadline {
            match conn.read(&mut chunk) {
                Ok(0) => panic!("连接被关闭了"),
                Ok(count) => {
                    got.extend_from_slice(&chunk[..count]);
                    if got.contains(&b'\n') {
                        break;
                    }
                }
                Err(error) if is_retryable(&error) => thread::sleep(POLL_INTERVAL),
                Err(error) => panic!("读失败：{error}"),
            }
        }
        String::from_utf8_lossy(&got).trim_end().to_owned()
    }

    #[test]
    fn accepts_a_connection_and_reads_a_frame() {
        let endpoint = temp_endpoint("accept");
        let server = Server::bind(&endpoint).expect("应能绑定");
        let (tx, rx) = channel();
        server.spawn(tx);

        let mut client = connect(&endpoint);
        // 必须持有 `out`：它的存在就是「内核还想跟这条连接说话」。丢了它，服务端会当作
        // 内核主动放弃连接而退出。
        let AdapterEvent::Connected { conn, out, .. } = recv(&rx) else {
            panic!("应先收到 Connected");
        };
        let _keep = out;

        client
            .write_all(b"{\"type\":\"hello\",\"id\":\"h-1\",\"version\":2,\"adapter\":\"test\"}\n")
            .expect("应能写出");

        let AdapterEvent::Frame { conn: id, frame } = recv(&rx) else {
            panic!("应收到一枚帧");
        };
        assert_eq!(id, conn);
        assert!(matches!(*frame, Frame::Hello(_)));
    }

    #[test]
    fn two_frames_in_one_write_are_split() {
        // 流不保证一次读恰好一行：两条帧粘在一起也必须切开。
        let endpoint = temp_endpoint("coalesce");
        let server = Server::bind(&endpoint).expect("应能绑定");
        let (tx, rx) = channel();
        server.spawn(tx);

        let mut client = connect(&endpoint);
        // 持有这一条：它带着写出通道，丢了就等于「内核不想再写」。
        let _connected = recv(&rx);

        client
            .write_all(b"{\"type\":\"ping\",\"id\":\"p-1\"}\n{\"type\":\"ping\",\"id\":\"p-2\"}\n")
            .expect("应能写出");

        let mut ids = Vec::new();
        for _ in 0..2 {
            let AdapterEvent::Frame { frame, .. } = recv(&rx) else {
                panic!("应收到两枚帧");
            };
            match *frame {
                Frame::Ping(ping) => ids.push(ping.id),
                other => panic!("期望 ping，得到 {other:?}"),
            }
        }
        assert_eq!(ids, vec!["p-1".to_owned(), "p-2".to_owned()]);
    }

    #[test]
    fn bad_line_is_reported_and_connection_survives() {
        let endpoint = temp_endpoint("bad");
        let server = Server::bind(&endpoint).expect("应能绑定");
        let (tx, rx) = channel();
        server.spawn(tx);

        let mut client = connect(&endpoint);
        let _connected = recv(&rx);

        client.write_all(b"not json\n").expect("应能写出");
        let got = recv(&rx);
        match &got {
            AdapterEvent::Undecodable { reason, .. } => assert!(!reason.is_empty()),
            AdapterEvent::Frame { frame, .. } => panic!("竟然解出来了：{frame:?}"),
            AdapterEvent::Closed { reason, .. } => panic!("连接被关闭了：{reason}"),
            AdapterEvent::Connected { .. } => panic!("又收到一次 Connected"),
        }

        client
            .write_all(b"{\"type\":\"ping\",\"id\":\"p-3\"}\n")
            .expect("应能写出");
        let got = recv(&rx);
        assert!(
            matches!(got, AdapterEvent::Frame { .. }),
            "坏行之后的正常帧应被解出来，实际 {got:?}"
        );
    }

    #[test]
    fn kernel_can_write_a_frame_to_the_adapter() {
        let endpoint = temp_endpoint("write");
        let server = Server::bind(&endpoint).expect("应能绑定");
        let (tx, rx) = channel();
        server.spawn(tx);

        let mut client = connect(&endpoint);
        let AdapterEvent::Connected { out, .. } = recv(&rx) else {
            panic!("应先收到 Connected");
        };

        out.send(
            line_for(&Frame::Ping(qqab_proto::Ping {
                id: "k-1".to_owned(),
            }))
            .unwrap(),
        )
        .expect("应能排队");

        let line = read_line(&mut client);
        assert!(line.contains("\"type\":\"ping\""), "实际读到 {line:?}");
    }

    #[test]
    fn disconnect_is_reported_to_the_kernel() {
        let endpoint = temp_endpoint("closed");
        let server = Server::bind(&endpoint).expect("应能绑定");
        let (tx, rx) = channel();
        server.spawn(tx);

        let client = connect(&endpoint);
        let _keep = recv(&rx);
        drop(client);

        assert!(matches!(recv(&rx), AdapterEvent::Closed { .. }));
    }

    #[test]
    fn stale_socket_file_is_removed_before_bind() {
        let endpoint = temp_endpoint("stale");
        let Endpoint::Path(path) = &endpoint else {
            // Windows 走命名管道，没有「残留文件」这回事。
            return;
        };
        std::fs::write(path, b"leftover").expect("应能写旧文件");

        let server = Server::bind(&endpoint).expect("应能覆盖旧文件");
        assert!(path.exists(), "绑定后应有新的套接字文件");
        drop(server);
        assert!(!path.exists(), "Drop 时清掉自己建的文件");
    }

    #[test]
    fn pipe_namespace_is_recognised() {
        // Windows 部署写的是 `\\.\pipe\qqab`：那段「父路径」不是可创建的目录。
        assert!(is_pipe_namespace(Path::new(r"\\.\pipe\qqab")));
        assert!(is_pipe_namespace(Path::new(r"\\.\PIPE\qqab")));
        assert!(is_pipe_namespace(Path::new(r"\pipe\qqab")));
        assert!(!is_pipe_namespace(Path::new("/run/user/1000/qqab.sock")));
        assert!(!is_pipe_namespace(Path::new("/tmp/qqab")));
    }

    #[test]
    fn tcp_endpoint_round_trips_a_frame() {
        // 跨机器形态：同一套连接循环走 TCP，行为与本地形态一致。
        let listener = TcpListener::bind("127.0.0.1:0").expect("应能绑定回环端口");
        let port = listener.local_addr().expect("应有本地地址").port();

        let (tx, rx) = channel();
        let server = Server {
            listener: Listener::Tcp(listener),
            endpoint: Endpoint::Tcp {
                host: "127.0.0.1".to_owned(),
                port,
            },
        };
        server.spawn(tx);

        let mut client = TcpStream::connect(("127.0.0.1", port)).expect("应能连上");
        client.set_nonblocking(true).expect("应能设非阻塞");
        let _connected = recv(&rx);

        client
            .write_all(b"{\"type\":\"hello\",\"id\":\"h-9\",\"version\":2,\"adapter\":\"tcp\"}\n")
            .expect("应能写出");
        let AdapterEvent::Frame { frame, .. } = recv(&rx) else {
            panic!("应收到一枚帧");
        };
        assert!(matches!(*frame, Frame::Hello(_)));
    }

    #[test]
    fn namespaced_endpoint_binds_and_accepts() {
        // 跨平台的「不落文件」形态：Linux 抽象命名空间、Windows 命名管道。
        // 这条测试正是平台无关性的证据——它在两个平台上都跑同一段代码。
        if !cfg!(any(target_os = "linux", windows)) {
            return;
        }
        let endpoint = Endpoint::Namespaced(format!("qqab-ns-{}", std::process::id()));
        let server = Server::bind(&endpoint).expect("应能绑定命名空间");
        let (tx, rx) = channel();
        server.spawn(tx);

        let mut client = connect(&endpoint);
        let _connected = recv(&rx);
        client
            .write_all(b"{\"type\":\"ping\",\"id\":\"ns-1\"}\n")
            .expect("应能写出");
        let AdapterEvent::Frame { frame, .. } = recv(&rx) else {
            panic!("应收到一枚帧");
        };
        assert!(matches!(*frame, Frame::Ping(_)));
    }
}
