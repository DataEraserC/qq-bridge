//! 适配器接入：内核侧的 Unix socket 服务端。
//!
//! 一侧是内核，一侧是适配器。协议见 `docs/architecture/05-protocol.md`——每行一个 JSON
//! 对象，未知 `type` 忽略而不是断连。
//!
//! 每条接入连接一个线程，用**读超时**同时照看两个方向（与 [ADR-0006] 的传输层同一形状）：
//! 醒来先看有没有帧要发，再读一段字节，按换行切出完整的行。这样内核主循环不必为每个
//! 连接维护状态，也不需要 async runtime。
//!
//! 断开要报给内核：适配器崩溃时进行中的回合视为失败，由内核重投。所以读到 EOF 或传输
//! 错误一律发 [`AdapterEvent::Closed`]，不静默退出。
//!
//! [ADR-0006]: ../../../docs/adr/0006-blocking-websocket-per-thread.md

use std::io::{ErrorKind, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Sender, TryRecvError, channel};
use std::thread;
use std::time::Duration;

use qqab_proto::{DecodeError, Frame, decode, encode};

/// 读超时。纯轮询节奏，不表示「多久算断线」——断线由读到 EOF 或错误判断。
pub const READ_TIMEOUT: Duration = Duration::from_millis(200);
/// 写超时。对端不读时不能让内核卡死在这里。
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

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
    Frame { conn: u64, frame: Frame },
    /// 收到一行但解不出来。内核丢弃它，连接继续。
    Undecodable { conn: u64, reason: String },
    /// 连接结束。
    Closed { conn: u64, reason: String },
}

/// 已绑定的服务端。`bind` 之后由 [`Server::spawn`] 起接受循环。
pub struct Server {
    path: PathBuf,
    listener: UnixListener,
}

impl Server {
    /// 绑定 socket。路径上已有一个无人监听的旧文件时一并清掉。
    ///
    /// 只在这里删一个文件：内核异常退出会留下 socket 文件，不删掉就永远绑定不上。
    pub fn bind(path: &Path) -> Result<Self, ServerError> {
        if path.exists() {
            std::fs::remove_file(path).map_err(|source| ServerError::Stale {
                path: path.display().to_string(),
                detail: source.to_string(),
            })?;
        }
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).map_err(|source| ServerError::Stale {
                path: parent.display().to_string(),
                detail: source.to_string(),
            })?;
        }

        let listener = UnixListener::bind(path).map_err(|source| ServerError::Bind {
            path: path.display().to_string(),
            detail: source.to_string(),
        })?;

        Ok(Self {
            path: path.to_path_buf(),
            listener,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 起接受循环，每条连接一个线程。
    pub fn spawn(self, tx: Sender<AdapterEvent>) {
        thread::spawn(move || {
            let mut next_id = 0u64;
            for stream in self.listener.incoming() {
                let Ok(stream) = stream else {
                    continue;
                };
                next_id += 1;
                let id = next_id;
                let peer = stream
                    .peer_addr()
                    .map(|addr| format!("{addr:?}"))
                    .unwrap_or_else(|_| "未知".to_owned());
                let tx = tx.clone();
                thread::spawn(move || serve_conn(id, stream, peer, tx));
            }
        });
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        // 只删自己建的那个文件。
        let _ = std::fs::remove_file(&self.path);
    }
}

/// 一条连接的双向循环。
fn serve_conn(id: u64, stream: UnixStream, peer: String, tx: Sender<AdapterEvent>) {
    let _ = stream.set_read_timeout(Some(READ_TIMEOUT));
    let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));

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
    let mut chunk = [0u8; 4096];

    loop {
        // 先发：把内核排进来的帧全部写出去。
        loop {
            match out_rx.try_recv() {
                Ok(line) => {
                    let mut payload = line.into_bytes();
                    payload.push(b'\n');
                    if (&stream).write_all(&payload).is_err() {
                        let _ = tx.send(AdapterEvent::Closed {
                            conn: id,
                            reason: "写失败".to_owned(),
                        });
                        return;
                    }
                    let _ = (&stream).flush();
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    let _ = tx.send(AdapterEvent::Closed {
                        conn: id,
                        reason: "内核关闭了写出通道".to_owned(),
                    });
                    return;
                }
            }
        }

        // 再读：读到多少算多少，按换行切完整行。
        match (&stream).read(&mut chunk) {
            Ok(0) => {
                let _ = tx.send(AdapterEvent::Closed {
                    conn: id,
                    reason: "对端关闭".to_owned(),
                });
                return;
            }
            Ok(count) => {
                buffer.extend_from_slice(&chunk[..count]);
                while let Some(index) = buffer.iter().position(|byte| *byte == b'\n') {
                    let line: Vec<u8> = buffer.drain(..=index).collect();
                    let text = String::from_utf8_lossy(&line[..line.len() - 1]).into_owned();
                    if text.trim().is_empty() {
                        continue;
                    }
                    match decode(&text) {
                        Ok(frame) => {
                            let _ = tx.send(AdapterEvent::Frame { conn: id, frame });
                        }
                        Err(error) => {
                            let _ = tx.send(AdapterEvent::Undecodable {
                                conn: id,
                                reason: describe(&error),
                            });
                        }
                    }
                }
            }
            Err(error)
                if error.kind() == ErrorKind::WouldBlock || error.kind() == ErrorKind::TimedOut =>
            {
                continue;
            }
            Err(error) => {
                let _ = tx.send(AdapterEvent::Closed {
                    conn: id,
                    reason: error.to_string(),
                });
                return;
            }
        }
    }
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
    Bind { path: String, detail: String },
    Stale { path: String, detail: String },
}

impl std::fmt::Display for ServerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Bind { path, detail } => {
                write!(f, "socket 绑定失败：{path}（{detail}）")
            }
            Self::Stale { path, detail } => {
                write!(f, "无法清理旧 socket 文件：{path}（{detail}）")
            }
        }
    }
}

impl std::error::Error for ServerError {}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::mpsc::Receiver;

    fn temp_socket(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("qqab-{tag}-{}.sock", std::process::id()))
    }

    fn recv(rx: &Receiver<AdapterEvent>) -> AdapterEvent {
        rx.recv_timeout(Duration::from_secs(5)).expect("应收到事件")
    }

    #[test]
    fn accepts_a_connection_and_reads_a_frame() {
        let path = temp_socket("accept");
        let server = Server::bind(&path).expect("应能绑定");
        let (tx, rx) = channel();
        server.spawn(tx);

        let mut client = UnixStream::connect(&path).expect("应能连上");
        let AdapterEvent::Connected {
            conn, out: _out, ..
        } = recv(&rx)
        else {
            panic!("应先收到 Connected");
        };

        client
            .write_all(b"{\"type\":\"hello\",\"id\":\"h-1\",\"version\":1,\"adapter\":\"test\"}\n")
            .expect("应能写出");

        let AdapterEvent::Frame { conn: id, frame } = recv(&rx) else {
            panic!("应收到一枚帧");
        };
        assert_eq!(id, conn);
        assert!(matches!(frame, Frame::Hello(_)));

        drop(client);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn two_frames_in_one_write_are_split() {
        // TCP/Unix 流不保证一次读恰好一行：两条帧粘在一起也必须切开。
        let path = temp_socket("coalesce");
        let server = Server::bind(&path).expect("应能绑定");
        let (tx, rx) = channel();
        server.spawn(tx);

        let mut client = UnixStream::connect(&path).expect("应能连上");
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
            match frame {
                Frame::Ping(ping) => ids.push(ping.id),
                other => panic!("期望 ping，得到 {other:?}"),
            }
        }
        assert_eq!(ids, vec!["p-1".to_owned(), "p-2".to_owned()]);

        drop(client);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn bad_line_is_reported_and_connection_survives() {
        let path = temp_socket("bad");
        let server = Server::bind(&path).expect("应能绑定");
        let (tx, rx) = channel();
        server.spawn(tx);

        let mut client = UnixStream::connect(&path).expect("应能连上");
        // 持有这一条：它带着写出通道，丢了就等于「内核不想再写」。
        let _connected = recv(&rx);

        client.write_all(b"not json\n").expect("应能写出");
        client.flush().expect("应能 flush");
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

        drop(client);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn kernel_can_write_a_frame_to_the_adapter() {
        let path = temp_socket("write");
        let server = Server::bind(&path).expect("应能绑定");
        let (tx, rx) = channel();
        server.spawn(tx);

        let mut client = UnixStream::connect(&path).expect("应能连上");
        let _ = client.set_read_timeout(Some(Duration::from_secs(5)));
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

        let mut got = [0u8; 64];
        let count = client.read(&mut got).expect("应读到内核发来的行");
        let text = String::from_utf8_lossy(&got[..count]);
        assert!(text.contains("\"type\":\"ping\""), "实际读到 {text:?}");

        drop(client);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn disconnect_is_reported_to_the_kernel() {
        let path = temp_socket("closed");
        let server = Server::bind(&path).expect("应能绑定");
        let (tx, rx) = channel();
        server.spawn(tx);

        let client = UnixStream::connect(&path).expect("应能连上");
        let _keep = recv(&rx);
        drop(client);

        assert!(matches!(recv(&rx), AdapterEvent::Closed { .. }));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn stale_socket_file_is_removed_before_bind() {
        let path = temp_socket("stale");
        std::fs::write(&path, b"leftover").expect("应能写旧文件");
        let server = Server::bind(&path).expect("应能覆盖旧文件");
        assert!(path.exists(), "绑定后应有新的 socket 文件");
        drop(server);
        assert!(!path.exists(), "Drop 时清掉自己建的文件");
    }
}
