//! 桥侧统一控制台：一个极简 HTTP 服务（ADR-0018）。
//!
//! webapi + webui（用户 2026-10-31 裁定：配置页面要有，做成 webapi + webui 且带鉴权；
//! 参考旧 Node 版 bridge.js 控制台的安全做法）。用标准库手写 HTTP（不引依赖）：
//! - `GET /`：单文件管理页（`console.html`，include 进二进制）。浏览器没法随手发
//!   Authorization 头，所以页面用 `/?token=<控制台token>` 进门，JS 把 token 存进
//!   localStorage，之后所有 `/api/*` 请求都带 `Authorization: Bearer`。
//! - `/api/*`：状态、会话（含桥↔harness 映射与重置）、反馈、配置只读视图。
//! - 鉴权：每条请求都要 token（Bearer 头或 `?token=`，常量时间比较）。
//! - 安全响应头（X-Frame-Options / CSP / nosniff / no-store / no-referrer）照抄旧版。

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use acb_kernel::{Bridge, ControlRegistry, Store};
use acb_proto::Frame;
use serde_json::json;

/// 管理页。单文件、零外部依赖：改页面就是改这个 HTML，编译时嵌进二进制。
const CONSOLE_HTML: &str = include_str!("console.html");

/// 没带 token 打开 `/` 时给的一小块登录页：弹窗要 token，拼进 URL 重进。
/// 跟旧 Node 版同一招——不让完整页面在未授权时先渲染出来。
const LOGIN_HTML: &str = r#"<!doctype html><meta charset="utf-8"><title>需要令牌</title>
<body style="font-family:system-ui;background:#14161a;color:#d8dce3;display:grid;place-items:center;height:100vh">
<div style="text-align:center">
<p>请输入控制台访问令牌</p>
<input id="t" type="password" style="padding:6px 10px;border-radius:6px;border:1px solid #33373f;background:#1e2127;color:#d8dce3">
<button onclick="go()" style="padding:6px 14px;margin-left:8px;cursor:pointer">进入</button>
</div>
<script>function go(){var t=document.getElementById('t').value;if(t)location.href='/?token='+encodeURIComponent(t)}
document.getElementById('t').addEventListener('keydown',function(e){if(e.key==='Enter')go()})</script>
</body>"#;

/// 控制台服务。桥本体之外的一条 HTTP 服务，跟接入点（TCP 7700）互不相干。
pub struct Console<S: Store> {
    bridge: Arc<Mutex<Bridge<S>>>,
    controls: ControlRegistry,
    token: String,
    /// 配置文件路径：编辑写回要落到这个文件（ADR-0009）。
    config_path: String,
}

impl<S: Store + Send + 'static> Console<S> {
    pub fn new(
        bridge: Arc<Mutex<Bridge<S>>>,
        controls: ControlRegistry,
        token: String,
        config_path: String,
    ) -> Self {
        Self {
            bridge,
            controls,
            token,
            config_path,
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

        // 浏览器自动请求 favicon 且不带 token；放鉴权前 204 收掉，别给 DevTools 留 401 噪音。
        if method == "GET" && (path == "/favicon.ico" || path == "/apple-touch-icon.png") {
            return write_response(&mut stream, 204, "text/plain", "");
        }

        let (path, query) = split_query(&path);
        // 两条门路：Authorization: Bearer 头，或 ?token=（只有浏览器进门用）。
        let authorized = self.authorized(&auth) || self.query_token_authorized(query);

        if !authorized {
            // 页面本身：没 token 时给登录页（不是 JSON），人才能输 token 进来。
            if method == "GET" && path == "/" {
                return write_response(&mut stream, 401, "text/html; charset=utf-8", LOGIN_HTML);
            }
            return write_response(
                &mut stream,
                401,
                "application/json",
                r#"{"error":"未授权：带 Authorization: Bearer <token>"}"#,
            );
        }

        // 管理页。route 只管 JSON API。
        if method == "GET" && path == "/" {
            return write_response(&mut stream, 200, "text/html; charset=utf-8", CONSOLE_HTML);
        }

        let (status, body) = self.route(&method, path, &body);
        write_response(&mut stream, status, "application/json", &body)
    }

    fn authorized(&self, auth: &str) -> bool {
        // 常量时间比较，避免时序侧信道（token 是敏感凭据）。
        constant_time_eq(auth, &format!("Bearer {}", self.token))
    }

    /// `?token=<控制台token>` 形式的鉴权（浏览器进门用）。
    fn query_token_authorized(&self, query: &str) -> bool {
        query_token_matches(query, &self.token)
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
            // 配置只读视图（方案 A 第一步）。敏感字段在桥这侧就遮掉，完整原文
            // 永远不发给浏览器。编辑写回 + 热重载是第二步（ADR-0009）。
            ("GET", "/api/config") => (200, self.config_view()),
            // TOML 原文编辑（方案 A 第二步，ADR-0009）：读原文 → 管理员改 →
            // 保存时先校验再落盘（备份旧文件），解析通过就暂存进桥，下一轮生效。
            ("GET", "/api/config/toml") => self.config_toml(),
            ("POST", "/api/config/toml") => self.save_config_toml(body),
            ("POST", "/api/config/discard") => {
                let mut bridge = lock(&self.bridge);
                let discarded = bridge.discard_staged_config();
                (
                    200,
                    json!({"ok": true, "discarded": discarded}).to_string(),
                )
            }
            _ => (404, json!({"error":"没有这个端点"}).to_string()),
        }
    }

    /// 当前配置的只读 JSON 视图。遮蔽规则：
    /// - 字段名按 `_`/`-` 切段后命中敏感词（token/password/secret/cookie/credential/key）
    ///   的整值换成 `******`（有值才遮，没有如实给 null）；
    /// - 连接 url 里查询参数名敏感的值同样遮掉（`ws://…?token=xxx` 是常见写法）；
    /// - 控制台自己的 token 根本不进这个视图。
    fn config_view(&self) -> String {
        let bridge = lock(&self.bridge);
        let config = bridge.config();
        use acb_kernel::config::DestructivePolicy;
        json!({
            "personas": config.personas.iter().map(|p| json!({
                "id": p.id, "name": p.name, "text": p.text,
            })).collect::<Vec<_>>(),
            "memories": config.memories.iter().map(|m| json!({
                "id": m.id, "name": m.name, "kind": m.kind, "location": m.location,
            })).collect::<Vec<_>>(),
            "harnesses": config.harnesses.iter().map(|h| json!({
                "id": h.id, "name": h.name,
            })).collect::<Vec<_>>(),
            "sessions": config.sessions.iter().map(|s| json!({
                "id": s.id,
                "name": s.name,
                "harness": s.harness,
                "persona": s.persona,
                "model": s.model,
                "memories": s.memories,
                "token": mask_secret(&s.token),
                "subscriptions": s.subscriptions.iter().map(|sub| json!({
                    "room": sub.room.to_string(),
                    "wake": json!({
                        "at_mention": sub.wake.at_mention,
                        "reply": sub.wake.reply,
                        "name_mention": sub.wake.name_mention,
                        "names": sub.wake.names,
                        "accumulate": sub.wake.accumulate,
                        "poke": sub.wake.poke,
                        "events": sub.wake.events,
                        "keywords": sub.wake.keywords,
                        "ignore_speakers": sub.wake.ignore_speakers,
                    }),
                })).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
            "connections": config.connections.iter().map(|c| json!({
                "id": c.id,
                "platform": c.platform,
                "kind": c.kind,
                "transport": c.transport.as_str(),
                "url": mask_url_query(&c.url),
                "token": c.token.as_deref().map(mask_secret),
                "account": c.account,
                "admin_qqs": c.admin_qqs,
                "allow_destructive": match &c.allow_destructive {
                    DestructivePolicy::None => json!("默认全拒"),
                    DestructivePolicy::All => json!("全部放行"),
                    DestructivePolicy::Only(methods) => json!(methods),
                },
            })).collect::<Vec<_>>(),
            "file_relay": config.file_relay.as_ref().map(|r| json!({
                "mode": r.mode,
                "listen": r.listen,
                "public_base": r.public_base,
                "dir": r.dir,
            })),
            "storage": { "path": config.storage.path },
            "retention": {
                "max_records_per_room": config.retention.max_records_per_room,
                "max_age_days": config.retention.max_age_days,
            },
            "worktrees_root": config.worktrees_root,
            "delivery_limit": config.delivery_limit,
            "default_on_turn_error": config.default_on_turn_error.iter().map(|action| match action {
                acb_kernel::config::TurnErrorAction::Log => json!("log"),
                acb_kernel::config::TurnErrorAction::Send { room } => json!(format!("send:{room}")),
            }).collect::<Vec<_>>(),
        })
        .to_string()
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
            "config_version": bridge.config_version(),
            "staged_config": bridge.has_staged_config(),
        })
        .to_string()
    }

    /// 配置 TOML 原文。**只在鉴权之后给**——原文里就是明文密钥，管理员编辑用。
    fn config_toml(&self) -> (u16, String) {
        match std::fs::read_to_string(&self.config_path) {
            Ok(text) => (
                200,
                json!({"path": self.config_path, "text": text}).to_string(),
            ),
            Err(error) => (
                500,
                json!({"error": format!("读配置文件 {} 失败：{error}", self.config_path)})
                    .to_string(),
            ),
        }
    }

    /// 保存编辑后的配置 TOML（`POST /api/config/toml`，body `{"text": "..."}`）。
    ///
    /// 顺序是先校验后落盘：解析不过就原样退回，一个字节都不写——
    /// 「用户不该需要懂文件格式」的底线是**坏配置永远进不了磁盘**。
    /// 落盘前把旧文件备份成 `<路径>.bak`（只留最近一份）。
    /// 解析通过的新配置暂存进桥，下一次开轮换上（ADR-0009 轮次边界重载）。
    /// 返回里带影响清单：哪些下一轮自动生效、哪些要重启桥。
    fn save_config_toml(&self, body: &str) -> (u16, String) {
        let parsed: serde_json::Value = match serde_json::from_str(body) {
            Ok(value) => value,
            Err(error) => {
                return (
                    400,
                    json!({"error": format!("body 要是 JSON：{{\"text\": \"…\"}}（{error}）")})
                        .to_string(),
                );
            }
        };
        let Some(text) = parsed.get("text").and_then(serde_json::Value::as_str) else {
            return (400, json!({"error": "body 里要有 text 字段（配置 TOML 原文）"}).to_string());
        };

        // 1. 校验：解析不过就拒绝，什么都不写。
        let new_config = match acb_kernel::Config::parse(text) {
            Ok(config) => config,
            Err(error) => {
                return (
                    400,
                    json!({
                        "error": format!("配置解析不过，一个字节都没写：{error}"),
                        "saved": false,
                    })
                    .to_string(),
                );
            }
        };

        // 2. 读旧配置算影响清单（在改桥之前，用的是当前生效的这份）。
        let (hot, restart) = {
            let bridge = lock(&self.bridge);
            reload_impact(bridge.config(), &new_config)
        };

        // 3. 备份旧文件 → 落盘新文件。备份失败不算致命（首次保存时旧文件可能不存在），
        //    但落盘失败必须报——不能让「磁盘上和桥里跑的」悄悄分家。
        let old_text = std::fs::read_to_string(&self.config_path).ok();
        if let Some(old_text) = &old_text {
            let backup_path = format!("{}.bak", self.config_path);
            if let Err(error) = std::fs::write(&backup_path, old_text) {
                eprintln!("控制台：备份配置到 {backup_path} 失败：{error}");
            }
        }
        if let Err(error) = std::fs::write(&self.config_path, text) {
            return (
                500,
                json!({"error": format!("写配置文件 {} 失败：{error}", self.config_path)})
                    .to_string(),
            );
        }

        // 4. 暂存进桥，下一轮生效。
        {
            let mut bridge = lock(&self.bridge);
            bridge.stage_config(new_config);
        }

        (
            200,
            json!({
                "ok": true,
                "saved": true,
                "note": "已保存并暂存，下一次开轮自动生效（轮次边界重载，ADR-0009）",
                "hot_reload": hot,
                "needs_restart": restart,
                "backup": old_text.map(|_| format!("{}.bak", self.config_path)),
            })
            .to_string(),
        )
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

/// 敏感字段名黑名单。字段名按 `_`/`-` 切段后逐段对——`keywords` 里虽然含
/// 「key」字样，但切段后是 `keywords` 整段，不匹配 `key`，不会误伤。
const SENSITIVE_KEY_PARTS: &[&str] = &[
    "token",
    "password",
    "passwd",
    "secret",
    "cookie",
    "credential",
    "credentials",
    "privatekey",
    "private",
    "apikey",
    "api",
    "key",
    "accesskey",
];

/// 字段名是否敏感。
fn is_sensitive_key(field: &str) -> bool {
    field
        .split(['_', '-'])
        .any(|part| SENSITIVE_KEY_PARTS.contains(&part))
}

/// `?token=<…>` 查询串鉴权。自由函数：纯逻辑，直接可测。
fn query_token_matches(query: &str, token: &str) -> bool {
    match query_param(query, "token") {
        Some(supplied) if !supplied.is_empty() => constant_time_eq(supplied, token),
        _ => false,
    }
}

/// 有值就遮成 `******`（遮罩本身进 JSON，页面认它画遮蔽样式）；空值如实给 null。
fn mask_secret(value: &str) -> serde_json::Value {
    if value.is_empty() {
        serde_json::Value::Null
    } else {
        json!("******")
    }
}

/// 把 URL 查询串里敏感参数的值遮掉（`ws://h:3001/ws?token=x` → `?token=******`）。
/// 非 http(s)/ws(s) 或没有查询串时原样返回。路径与主机部分不动。
fn mask_url_query(url: &str) -> String {
    let Some((head, query)) = url.split_once('?') else {
        return url.to_owned();
    };
    let masked: Vec<String> = query
        .split('&')
        .map(|pair| match pair.split_once('=') {
            Some((name, value)) if is_sensitive_key(name) && !value.is_empty() => {
                format!("{name}=******")
            }
            _ => pair.to_owned(),
        })
        .collect();
    format!("{head}?{}", masked.join("&"))
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
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        409 => "Conflict",
        _ => "Error",
    };
    // 安全响应头照抄旧 Node 版控制台：管理页不许被别处 iframe/引脚本，缓存一律不留。
    // 不再给 `Access-Control-Allow-Origin: *`——token 鉴权的服务不该跨域放行。
    const SECURITY_HEADERS: &str = "X-Frame-Options: DENY\r\n\
        Content-Security-Policy: default-src 'self'; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'; connect-src 'self'\r\n\
        Cache-Control: no-store\r\n\
        X-Content-Type-Options: nosniff\r\n\
        Referrer-Policy: no-referrer\r\n";
    let header = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n{SECURITY_HEADERS}Connection: close\r\n\r\n",
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

/// 比较新旧配置，列出热重载影响（ADR-0009 轮次边界重载的边界在哪）：
///
/// - 返回第一项（`hot`）：下一轮自动生效的改动清单；
/// - 返回第二项（`restart`）：**要重启桥才生效**的改动清单——这些是启动时
///   就定死的东西：监听地址绑定、守护线程手里攥着的连接信息、打开的库路径。
///
/// 注意 `file_relay`：`listen` 变了要重启（HTTP 服务启动时绑定），
/// 但 `public_base`/`dir` 不用——发布端（桥这边）每轮用当前配置重建。
fn reload_impact(old: &acb_kernel::Config, new: &acb_kernel::Config) -> (Vec<String>, Vec<String>) {
    let mut hot = Vec::new();
    let mut restart = Vec::new();

    if old.sessions != new.sessions {
        hot.push("sessions（会话/订阅/唤醒策略/人格与记忆挂载）".to_owned());
    }
    if old.harnesses != new.harnesses {
        hot.push("harnesses".to_owned());
    }
    if old.personas != new.personas {
        hot.push("personas（人格）".to_owned());
    }
    if old.memories != new.memories {
        hot.push("memories（记忆仓库声明）".to_owned());
    }
    if old.retention != new.retention {
        hot.push("retention（流水保留）".to_owned());
    }
    if old.default_on_turn_error != new.default_on_turn_error {
        hot.push("default_on_turn_error（默认失败通知）".to_owned());
    }
    if old.delivery_limit != new.delivery_limit {
        hot.push("delivery_limit（单轮投递上限）".to_owned());
    }
    match (&old.file_relay, &new.file_relay) {
        (Some(a), Some(b)) => {
            if a.public_base != b.public_base || a.dir != b.dir {
                hot.push("file_relay.public_base/dir（发布端下一轮重建）".to_owned());
            }
            if a.listen != b.listen {
                restart.push("file_relay.listen（文件服务监听在启动时绑定）".to_owned());
            }
        }
        (None, Some(_)) | (Some(_), None) => {
            restart.push("file_relay（增删要重起/停掉文件服务，监听在启动时定）".to_owned());
        }
        (None, None) => {}
    }

    if old.endpoint != new.endpoint {
        restart.push("endpoint（接入点监听在启动时绑定）".to_owned());
    }
    if old.console != new.console {
        restart.push("console（控制台监听与令牌在启动时定死）".to_owned());
    }
    if old.connections != new.connections {
        restart.push("connections（连接守护线程持有旧连接信息，改 url/token 要重启）".to_owned());
    }
    if old.storage != new.storage {
        restart.push("storage（状态库启动时打开，换路径等于换了一个桥）".to_owned());
    }
    if old.worktrees_root != new.worktrees_root {
        restart.push("worktrees_root（已有记忆 worktree 在旧根下，换根要重启）".to_owned());
    }

    (hot, restart)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sensitive_key_matches_parts_not_substrings() {
        assert!(is_sensitive_key("token"));
        assert!(is_sensitive_key("access_token"));
        assert!(is_sensitive_key("api-key"));
        assert!(is_sensitive_key("console_token"));
        // keywords 不是密钥——别因为含 key 字样就误伤。
        assert!(!is_sensitive_key("keywords"));
        assert!(!is_sensitive_key("url"));
    }

    #[test]
    fn mask_secret_masks_only_when_present() {
        assert_eq!(mask_secret("abc"), json!("******"));
        assert_eq!(mask_secret(""), serde_json::Value::Null);
    }

    #[test]
    fn mask_url_query_masks_sensitive_params_only() {
        assert_eq!(
            mask_url_query("ws://127.0.0.1:3001/ws?token=abc&flag=1"),
            "ws://127.0.0.1:3001/ws?token=******&flag=1"
        );
        assert_eq!(
            mask_url_query("ws://127.0.0.1:3001/ws"),
            "ws://127.0.0.1:3001/ws"
        );
        // 空值不遮——没有秘密可保。
        assert_eq!(
            mask_url_query("http://h/p?token=&a=1"),
            "http://h/p?token=&a=1"
        );
    }

    #[test]
    fn query_token_authorized_accepts_exact_match_only() {
        assert!(query_token_matches("a=1&token=s3cret", "s3cret"));
        assert!(!query_token_matches("token=wrong", "s3cret"));
        assert!(!query_token_matches("token=", "s3cret"));
        assert!(!query_token_matches("no-token-here", "s3cret"));
        assert!(!query_token_matches("token=S3CRET", "s3cret"));
    }

    /// 一份能通过解析的最小配置。改字段测影响对比用。
    fn toml_with(wake_line: &str, endpoint_listen: &str, connection_url: &str) -> String {
        format!(
            r#"
[[personas]]
id = "default"
name = "默认"
text = "测试。"

[[harnesses]]
id = "dsh"
name = "DSH"

[[connections]]
id = "qq"
platform = "qq"
kind = "onebot11"
url = "{connection_url}"
account = "10001"
token = "conn-secret"

[[sessions]]
id = "main"
harness = "dsh"
persona = "default"
token = "sess-secret"

[[sessions.subscriptions]]
connection = "qq"
room = "group/123"
{wake_line}

[endpoint]
listen = "{endpoint_listen}"

[console]
listen = "127.0.0.1:31990"
token = "ctok123"
"#
        )
    }

    #[test]
    fn reload_impact_sorts_hot_from_restart() {
        // 只改唤醒策略 → 热重载；只改监听/连接 → 要重启；什么都不改 → 两边都空。
        let base = toml_with("", "127.0.0.1:37700", "ws://127.0.0.1:53001/ws");
        let old = acb_kernel::Config::parse(&base).expect("能解析");

        let hot_only = toml_with(
            // 注意：at_mention 默认就是 true，改它等于没改；keywords 默认空，一定有差异。
            "\n[sessions.subscriptions.wake]\nkeywords = [\"喂\"]",
            "127.0.0.1:37700",
            "ws://127.0.0.1:53001/ws",
        );
        let new = acb_kernel::Config::parse(&hot_only).expect("能解析");
        let (hot, restart) = reload_impact(&old, &new);
        assert_eq!(restart, Vec::<String>::new());
        assert!(hot.iter().any(|item| item.contains("sessions")));

        let restart_only = toml_with("", "127.0.0.1:39999", "ws://127.0.0.1:53001/ws");
        let new = acb_kernel::Config::parse(&restart_only).expect("能解析");
        let (hot, restart) = reload_impact(&old, &new);
        assert_eq!(hot, Vec::<String>::new());
        assert!(restart.iter().any(|item| item.contains("endpoint")));

        let conn_changed =
            toml_with("", "127.0.0.1:37700", "ws://127.0.0.1:53002/ws?token=newsecret");
        let new = acb_kernel::Config::parse(&conn_changed).expect("能解析");
        let (_, restart) = reload_impact(&old, &new);
        assert!(restart.iter().any(|item| item.contains("connections")));

        let same = acb_kernel::Config::parse(&base).expect("能解析");
        let (hot, restart) = reload_impact(&old, &same);
        assert_eq!(hot, Vec::<String>::new());
        assert_eq!(restart, Vec::<String>::new());
    }
}
