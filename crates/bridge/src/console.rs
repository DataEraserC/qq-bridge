//! 桥侧统一控制台：一个极简 HTTP 服务（ADR-0018）。
//!
//! 只处理几个端点，用标准库手写 HTTP（不引依赖）：读会话/映射状态、重置会话、看概览。
//! 可远程访问，所以所有请求都要 token 鉴权（`Authorization: Bearer <token>`）。

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use acb_kernel::{Bridge, ControlRegistry, Store};
use acb_proto::Frame;
use serde_json::json;

/// 控制台服务。桥本体之外的一条 HTTP 服务，跟接入点（TCP 7700）互不相干。
pub struct Console<S: Store> {
    bridge: Arc<Mutex<Bridge<S>>>,
    controls: ControlRegistry,
    token: String,
}

impl<S: Store + Send + 'static> Console<S> {
    pub fn new(bridge: Arc<Mutex<Bridge<S>>>, controls: ControlRegistry, token: String) -> Self {
        Self {
            bridge,
            controls,
            token,
        }
    }

    /// 守着监听端口，来一个请求处理一个。
    pub fn serve(listener: TcpListener, console: Arc<Console<S>>, shutdown: Arc<AtomicBool>) {
        listener.set_nonblocking(true).ok();
        while !shutdown.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((stream, _)) => {
                    let console = Arc::clone(&console);
                    std::thread::spawn(move || {
                        if let Err(error) = console.handle(stream) {
                            eprintln!("控制台：处理请求失败：{error}");
                        }
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                Err(error) => {
                    eprintln!("控制台：接受连接失败：{error}");
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            }
        }
    }

    fn handle(&self, mut stream: TcpStream) -> std::io::Result<()> {
        stream.set_read_timeout(Some(std::time::Duration::from_secs(5))).ok();
        let mut buffer = Vec::new();
        // 读请求头 + body（简版：读到空行结束头，再按 Content-Length 读 body）。
        let mut header_end = None;
        let mut chunk = [0_u8; 8192];
        loop {
            let read = stream.read(&mut chunk)?;
            if read == 0 {
                break;
            }
            buffer.extend_from_slice(&chunk[..read]);
            if let Some(pos) = find_header_end(&buffer) {
                header_end = Some(pos);
                break;
            }
            if buffer.len() > 64 * 1024 {
                break;
            }
        }
        let Some(header_end) = header_end else {
            return write_response(&mut stream, 400, "application/json", r#"{"error":"请求头不完整"}"#);
        };
        let header_text = String::from_utf8_lossy(&buffer[..header_end]).to_string();
        let (method, path, auth) = parse_request_line(&header_text);
        // 读 body（reset 等 POST 可能带 body，但 reset 不需要 body）。
        let _body = read_body(&mut stream, &buffer[header_end + 4..], &header_text);

        if !self.authorized(&auth) {
            return write_response(
                &mut stream,
                401,
                "application/json",
                r#"{"error":"未授权：带 Authorization: Bearer <token>"}"#,
            );
        }

        let (status, body) = self.route(&method, &path);
        write_response(&mut stream, status, "application/json", &body)
    }

    fn authorized(&self, auth: &str) -> bool {
        // 常量时间比较，避免时序侧信道（token 是敏感凭据）。
        constant_time_eq(auth, &format!("Bearer {}", self.token))
    }

    fn route(&self, method: &str, path: &str) -> (u16, String) {
        match (method, path) {
            ("GET", "/api/status") => (200, self.status()),
            ("GET", "/api/sessions") => (200, self.sessions()),
            ("POST", p) if p.starts_with("/api/session/") => self.reset(p),
            ("GET", "/") => (200, json!({"name":"acb-bridge console","api":["/api/status","/api/sessions","/api/session/:id/reset"]}).to_string()),
            _ => (404, json!({"error":"没有这个端点"}).to_string()),
        }
    }

    fn status(&self) -> String {
        let bridge = lock(&self.bridge);
        let config = bridge.config();
        json!({
            "connections": config.connections.len(),
            "sessions": config.sessions.len(),
            "harnesses": config.harnesses.len(),
            "busy_sessions": bridge.busy_sessions(),
            "queued_turns": bridge.queued_turns(),
        })
        .to_string()
    }

    fn sessions(&self) -> String {
        let bridge = lock(&self.bridge);
        let config = bridge.config();
        let maps = bridge.session_maps().unwrap_or_default();
        let sessions: Vec<_> = config
            .sessions
            .iter()
            .map(|session| {
                let map = maps.iter().find(|m| m.session == session.id);
                json!({
                    "id": session.id,
                    "name": session.name,
                    "harness": session.harness,
                    "persona": session.persona,
                    "model": session.model,
                    "memories": session.memories,
                    "subscriptions": session.subscriptions.iter().map(|s| s.room.to_string()).collect::<Vec<_>>(),
                    "map": map.map(|m| json!({
                        "harness_session": m.harness_session,
                        "meta": m.meta,
                        "updated_at": m.updated_at,
                    })),
                })
            })
            .collect();
        json!({ "sessions": sessions }).to_string()
    }

    fn reset(&self, path: &str) -> (u16, String) {
        // path 形如 /api/session/:id/reset
        let session = path
            .trim_start_matches("/api/session/")
            .trim_end_matches("/reset")
            .to_owned();
        if session.is_empty() {
            return (400, json!({"error":"会话 id 不能为空"}).to_string());
        }
        // 确认会话存在。
        let exists = {
            let bridge = lock(&self.bridge);
            bridge.config().session(&session).is_some()
        };
        if !exists {
            return (404, json!({"error": format!("没有会话 {session}")}).to_string());
        }
        // 发 control{reset} 帧给管理这个会话的 harness。
        let frame = Frame::Control {
            id: format!("ctl-{}", now_millis()),
            session: session.clone(),
            command: "reset".into(),
            args: json!({}),
        };
        let delivered = self.controls.send_control(frame);
        if delivered {
            (200, json!({"ok": true, "session": session, "note": "reset 指令已发出"}).to_string())
        } else {
            (
                409,
                json!({"ok": false, "session": session, "error": "没有 harness 在管理这个会话（可能没连上）"}).to_string(),
            )
        }
    }
}

fn lock<S: Store>(bridge: &Mutex<Bridge<S>>) -> std::sync::MutexGuard<'_, Bridge<S>> {
    bridge
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p)
}

fn parse_request_line(header_text: &str) -> (String, String, String) {
    let mut lines = header_text.lines();
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_owned();
    let path = parts.next().unwrap_or_default().to_owned();
    let mut auth = String::new();
    for line in lines {
        if let Some(value) = line.strip_prefix("Authorization:") {
            auth = value.trim().to_owned();
        }
    }
    (method, path, auth)
}

fn read_body(stream: &mut TcpStream, leftover: &[u8], header_text: &str) -> String {
    let mut body = leftover.to_vec();
    if let Some(len) = header_text
        .lines()
        .find_map(|l| l.strip_prefix("Content-Length:").and_then(|v| v.trim().parse::<usize>().ok()))
    {
        while body.len() < len {
            let mut chunk = [0_u8; 8192];
            match stream.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => body.extend_from_slice(&chunk[..n]),
                Err(_) => break,
            }
        }
    }
    String::from_utf8_lossy(&body).to_string()
}

fn write_response(stream: &mut TcpStream, status: u16, content_type: &str, body: &str) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        409 => "Conflict",
        _ => "Error",
    };
    let header = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nAccess-Control-Allow-Origin: *\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes())?;
    stream.write_all(body.as_bytes())?;
    stream.flush()
}

/// 常量时间字符串比较。
fn constant_time_eq(a: &str, b: &str) -> bool {
    let a = a.as_bytes();
    let b = b.as_bytes();
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0_u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|e| e.as_millis() as i64)
        .unwrap_or(0)
}
