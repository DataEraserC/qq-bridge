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
        // 读 body（反馈改状态要带 {"status": "...", "note": "..."}）。
        let body = read_body(&mut stream, &buffer[header_end + 4..], &header_text);

        if !self.authorized(&auth) {
            return write_response(
                &mut stream,
                401,
                "application/json",
                r#"{"error":"未授权：带 Authorization: Bearer <token>"}"#,
            );
        }

        let (status, body) = self.route(&method, &path, &body);
        write_response(&mut stream, status, "application/json", &body)
    }

    fn authorized(&self, auth: &str) -> bool {
        // 常量时间比较，避免时序侧信道（token 是敏感凭据）。
        constant_time_eq(auth, &format!("Bearer {}", self.token))
    }

    fn route(&self, method: &str, path: &str, body: &str) -> (u16, String) {
        // 路径可能带查询串（/api/feedback?status=open&limit=20）。
        let (path, query) = split_query(path);
        match (method, path) {
            ("GET", "/api/status") => (200, self.status()),
            ("GET", "/api/sessions") => (200, self.sessions()),
            ("GET", "/api/feedback") => self.feedback(query),
            ("POST", p) if p.starts_with("/api/feedback/") => self.set_feedback(p, body),
            ("POST", p) if p.starts_with("/api/session/") => self.reset(p),
            ("GET", "/") => (200, json!({
                "name": "acb-bridge console",
                "api": [
                    "/api/status",
                    "/api/sessions",
                    "/api/session/:id/reset",
                    "/api/feedback?status=open&limit=20",
                    "/api/feedback/:id/status"
                ],
            })
            .to_string()),
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

    /// 列反馈。默认只给最近 50 条，按最近一次被提的时间倒序。
    fn feedback(&self, query: &str) -> (u16, String) {
        let status = query_param(query, "status").filter(|value| !value.is_empty());
        let limit = query_param(query, "limit")
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(50)
            .clamp(1, 500);

        let bridge = lock(&self.bridge);
        match bridge.store().feedback(status.as_deref(), limit) {
            Ok(items) => {
                let returned = items.len();
                let body = json!({
                    "returned": returned,
                    "feedback": items.iter().map(feedback_json).collect::<Vec<_>>(),
                });
                (200, body.to_string())
            }
            Err(error) => (500, json!({"error": error.to_string()}).to_string()),
        }
    }

    /// 改一条反馈的状态。`POST /api/feedback/:id/status`，body `{"status":"resolved","note":"..."}`。
    ///
    /// 为什么要有这一步：反馈是给人处理用的，只进不出会变成垃圾场。
    /// 处理完标掉，下次再看列表就只剩真没解决的。
    fn set_feedback(&self, path: &str, body: &str) -> (u16, String) {
        let rest = path
            .trim_start_matches("/api/feedback/")
            .trim_end_matches("/status");
        let Ok(id) = rest.parse::<i64>() else {
            return (400, json!({"error": "路径要写成 /api/feedback/:id/status"}).to_string());
        };
        let parsed: serde_json::Value = serde_json::from_str(body).unwrap_or_else(|_| json!({}));
        let status = parsed
            .get("status")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("resolved");
        if !["open", "acked", "resolved", "wontfix"].contains(&status) {
            return (
                400,
                json!({"error": "status 只能是 open / acked / resolved / wontfix"}).to_string(),
            );
        }
        let note = parsed.get("note").and_then(serde_json::Value::as_str);

        let mut bridge = lock(&self.bridge);
        match bridge.store_mut().set_feedback_status(id, status, note) {
            Ok(true) => (200, json!({"ok": true, "id": id, "status": status}).to_string()),
            Ok(false) => (404, json!({"error": format!("没有编号 {id} 的反馈")}).to_string()),
            Err(error) => (500, json!({"error": error.to_string()}).to_string()),
        }
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

/// 一条反馈怎么给控制台看。字段名和存储层对齐，省得两头各起一套名字。
fn feedback_json(item: &acb_kernel::Feedback) -> serde_json::Value {
    json!({
        "id": item.id,
        "title": item.title,
        "body": item.body,
        "suggestion": item.suggestion,
        "kind": item.kind,
        "severity": item.severity,
        "refs": item.refs,
        "occurrences": item.occurrences,
        "repeats": item.repeats,
        "tool": item.tool,
        "session": item.session,
        "harness": item.harness,
        "room": item.room,
        "context": item.context,
        "status": item.status,
        "note": item.note,
        "first_at": item.first_at,
        "last_at": item.last_at,
    })
}

/// 把 `/api/feedback?status=open&limit=20` 拆成路径与查询串。
fn split_query(path: &str) -> (&str, &str) {
    match path.split_once('?') {
        Some((head, query)) => (head, query),
        None => (path, ""),
    }
}

/// 从查询串里取一个参数（`a=1&b=2`）。不引 url 库：只认 `k=v`，够用。
fn query_param<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    query.split('&').find_map(|pair| {
        let (name, value) = pair.split_once('=')?;
        (name == key).then_some(value)
    })
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
