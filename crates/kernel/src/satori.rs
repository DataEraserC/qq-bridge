//! Satori 协议实现。
//!
//! 这个实现存在的**唯一理由**是验证 [`Transport`](crate::chat::Transport) 抽象是否成立。
//! OneBot 只有一个，看不出抽象对不对；Satori 在结构上和 OneBot 差得很远：
//!
//! | 差异 | OneBot | Satori |
//! | --- | --- | --- |
//! | 入站帧 | 裸 JSON 事件 | 带 `op` 的信令封装，事件在 `body` 里 |
//! | 出站 | 同一条 WS 上发 `action`，回 `echo` | WS 只收事件；动作走 HTTP `POST /message.create` |
//! | 会话标识 | `group_id` | `channel.id`（群号是 `guild.id`） |
//! | 身份 | `self_id` | `login.user.id` |
//! | 消息正文 | 消息段数组，要拍平 | 一段已经拍平的内容字符串 |
//! | 拍一拍 | `notice/notify/poke` | 没有这个概念 |
//! | 频道类型 | 只有群 | `TEXT`/`DIRECT`/`CATEGORY`/`VOICE` |
//!
//! 如果抽象是对的，上面每一条差异都只出现在这个文件里，内核一行不改。写下来之后确认了：
//! 只有「动作不在同一条连接上」这一条越过了本层——它落在 [`SatoriTransport::encode`] 的
//! 返回值里，而不是内核的分支里。
//!
//! **没有实现的部分要明说**：这里只做 WS 信令与 `message.create` 的编码，不实现 HTTP 客户端。
//! 出站动作被编成一个待发的 HTTP 请求描述，真发由传输层按协议名决定——见
//! [ADR-0009](../../../docs/adr/0009-chat-transport-boundary.md) 的「后果」一节。

use serde_json::{Value, json};

use crate::chat::{
    ChatAction, ChatEvent, ChatMessage, ChatOutcome, ChatStatus, ConversationKey, ReplyAddress,
    Transport, TransportError,
};

/// Satori 的信令 op 码。
mod op {
    pub const EVENT: i64 = 0;
    pub const PONG: i64 = 2;
    pub const READY: i64 = 4;
}

/// Satori 协议的 [`Transport`] 实现。
#[derive(Debug, Default)]
pub struct SatoriTransport {
    /// 当前登录的身份。`READY` 到达前为空，之后由 `login.user.id` 填上。
    ///
    /// 它是 `&self` 之外的可变状态，所以用 `Mutex` 而不是 `&mut self`：`decode` 的签名
    /// 是共享借用——传输层在读循环里同时持着它对解码调用。
    login: std::sync::Mutex<Option<String>>,
    /// 当前平台名。`READY` 的 `login.platform` 填上，之前为空。
    ///
    /// 会话命名空间的一部分：Satori 一个服务器可以同时挂 Discord、KOOK、Telegram，
    /// 所以「频道 123」在不同 platform 下是不同会话。
    platform: std::sync::Mutex<Option<String>>,
    /// 只处理文本频道。私聊频道没有群号，不在内核的范围模型里。
    ///
    /// 固定为真，不做成配置：内核只处理群，这是不变量而不是偏好。
    _text_only: bool,
}

impl SatoriTransport {
    pub fn new() -> Self {
        Self::default()
    }

    /// 当前登录身份。`READY` 之前是 `None`。
    pub fn login(&self) -> Option<String> {
        self.login.lock().ok()?.clone()
    }

    /// 当前平台名。`READY` 之前是 `None`。
    pub fn known_platform(&self) -> Option<String> {
        self.platform.lock().ok()?.clone()
    }

    fn remember_login(&self, user_id: Option<String>, platform: Option<String>) {
        if let Some(user_id) = user_id
            && let Ok(mut guard) = self.login.lock()
        {
            *guard = Some(user_id);
        }
        if let Some(platform) = platform.filter(|platform| !platform.is_empty())
            && let Ok(mut guard) = self.platform.lock()
        {
            *guard = Some(platform);
        }
    }
}

impl Transport for SatoriTransport {
    fn name(&self) -> &'static str {
        "satori"
    }

    /// `READY` 之前是 `None`。协议没给 `login.platform` 也留 `None`——不拿 adapter 名
    /// 顶替，也不猜 `qq`。
    fn platform(&self) -> Option<String> {
        self.known_platform()
    }

    fn decode(&self, line: &str) -> Result<ChatEvent, TransportError> {
        let value: Value = serde_json::from_str(line)?;
        let object = value.as_object().ok_or(TransportError::NotObject)?;

        let opcode = object
            .get("op")
            .and_then(Value::as_i64)
            .ok_or(TransportError::MissingField("op"))?;

        match opcode {
            op::EVENT => self.decode_event(object),
            op::READY => {
                let login_body = object.get("body").and_then(|body| body.get("login"));
                // `login.user.id` 是本连接的身份。没有它就没法判断「是不是自己发的」。
                let login = login_body
                    .and_then(|login| login.get("user"))
                    .and_then(|user| user.get("id"))
                    .and_then(string_of);
                // `login.platform` 是会话命名空间。协议没给就留空，不猜。
                let platform = login_body
                    .and_then(|login| login.get("platform"))
                    .and_then(string_of);
                self.remember_login(login, platform);
                Ok(ChatEvent::Status(ChatStatus::Ready {
                    platform: self.known_platform(),
                }))
            }
            op::PONG => Ok(ChatEvent::Ignored {
                kind: "pong".to_owned(),
            }),
            other => Ok(ChatEvent::Ignored {
                kind: format!("op:{other}"),
            }),
        }
    }

    fn encode(&self, action: &ChatAction, id: &str) -> String {
        // Satori 的动作走 HTTP，不在 WS 上。这里产出的是**请求描述**而不是 WS 帧：
        // `{"path": "...", "body": {...}}`。传输层看到这个形状就用 HTTP 发出去。
        //
        // 用同一条 WS 发是不可能的——Satori 的 WS 是单向的事件流。这条差异是抽象必须
        // 容纳的唯一一处非对称，所以它显式地落在这里。
        let ChatAction::Send { to, text } = action;
        json!({
            "path": "/message.create",
            "body": { "channel_id": to.address, "content": text },
            "id": id,
        })
        .to_string()
    }

    fn response(&self, _line: &str) -> Option<(Option<String>, ChatOutcome)> {
        // WS 上不会有动作响应：HTTP 的响应由传输层的 HTTP 分支处理。
        None
    }

    fn self_id(&self, account_self_id: &str) -> String {
        // 配置里写的账号身份优先；`READY` 之后以平台给的为准。
        self.login().unwrap_or_else(|| account_self_id.to_owned())
    }
}

impl SatoriTransport {
    fn decode_event(
        &self,
        object: &serde_json::Map<String, Value>,
    ) -> Result<ChatEvent, TransportError> {
        let body = object
            .get("body")
            .and_then(Value::as_object)
            .ok_or(TransportError::MissingField("body"))?;
        let kind = body.get("type").and_then(Value::as_str).unwrap_or_default();

        match kind {
            "message-created" => self.decode_message(body),
            // 拍一拍在 Satori 里不存在。硬映射到别的事件会让「拍一拍唤醒」在 Satori 上
            // 悄悄失效或误触发，不如明说不支持。
            other => Ok(ChatEvent::Ignored {
                kind: format!("event:{other}"),
            }),
        }
    }

    fn decode_message(
        &self,
        body: &serde_json::Map<String, Value>,
    ) -> Result<ChatEvent, TransportError> {
        let message = body
            .get("message")
            .and_then(Value::as_object)
            .ok_or(TransportError::MissingField("message"))?;

        let message_id = message
            .get("id")
            .and_then(string_of)
            .ok_or(TransportError::MissingField("message.id"))?;

        // Satori 的 `channel.type == DIRECT` 是私聊：没有群号，不进设备范围。
        let channel = message.get("channel").and_then(Value::as_object);
        let channel_type = channel
            .and_then(|channel| channel.get("type"))
            .and_then(Value::as_i64);
        if channel_type == Some(1) {
            return Ok(ChatEvent::Ignored {
                kind: "message:private".to_owned(),
            });
        }

        // 平台名是会话命名空间的一部分。`READY` 之前不知道——这时**丢弃**而不是猜：
        // 猜一个平台名会把消息归进错误的命名空间，比丢一条更难查。
        let Some(platform) = self.known_platform() else {
            return Ok(ChatEvent::Ignored {
                kind: "message:platform_unknown".to_owned(),
            });
        };

        let channel_id = channel
            .and_then(|channel| channel.get("id"))
            .and_then(string_of);

        // 群号优先取 `guild.id`：Satori 里一个群可以有多个文本频道，而内核的范围模型
        // 是「一个群一个订阅」。退到 `channel.id` 只在没有 guild 时才发生。
        let conversation = message
            .get("guild")
            .and_then(|guild| guild.get("id"))
            .and_then(string_of)
            .or_else(|| channel_id.clone())
            .ok_or(TransportError::MissingField("channel.id"))?;

        let user = message.get("user").and_then(Value::as_object);
        let sender_id = user
            .and_then(|user| user.get("id"))
            .and_then(string_of)
            .ok_or(TransportError::MissingField("user.id"))?;
        // `nick` 优先于 `name`：文档明说昵称更容易被识别。
        let sender_name = user
            .and_then(|user| user.get("nick"))
            .and_then(string_of)
            .filter(|nick| !nick.is_empty())
            .or_else(|| user.and_then(|user| user.get("name")).and_then(string_of));

        let text = message
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();

        // `created_at` 是毫秒（Satori 与 OneBot 的秒不同），但缺失时记 0。
        let time = message
            .get("created_at")
            .and_then(Value::as_i64)
            .unwrap_or(0);

        let self_id = self.login().unwrap_or_default();
        // Satori 的正文是字符串，`@` 已经渲染成 `<at id="..."/>` 元素。按元素串判断。
        let at_self = !self_id.is_empty() && text.contains(&format!("<at id=\"{self_id}\""));

        // 正文里的 `<at>` 等元素要拍平成纯文本——内核只认文本。
        let flattened = flatten_mentions(&text);

        Ok(ChatEvent::Message(ChatMessage {
            self_id,
            conversation: ConversationKey::new(platform, conversation),
            // 发回去要打到**频道**，不是 guild——一个群可以有多个文本频道。
            reply_to: channel_id.map(|address| ReplyAddress {
                platform: self.known_platform().unwrap_or_default(),
                address,
            }),
            message_id,
            sender_id,
            sender_name,
            text: flattened,
            at_self,
            time,
        }))
    }
}

/// 把 Satori 的消息内容元素拍平成纯文本。
///
/// Satori 的 `content` 是「字符串 + 元素」的混合：`你好 <at id="1"/> 在吗`。内核不认元素，
/// 所以这里把 `<at>`、`<img>`、`<file>` 换成与 OneBot 一致的占位符——两个实现在归档里
/// 产生的正文形状因此相同，下游渲染不必分支。
fn flatten_mentions(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;

    while let Some(start) = rest.find('<') {
        out.push_str(&rest[..start]);
        let after = &rest[start..];
        let Some(end) = after.find('>') else {
            // 没有闭合的 `<`：把剩下的原样留下，不猜。
            out.push_str(after);
            // 循环末尾还会补一次 `rest`：置空，否则同一段被追加两遍。
            rest = "";
            break;
        };
        let element = &after[..=end];

        if element.starts_with("<at ") {
            // `<at id="123"/>` 里的 id 不是本账号时保留成 `@123`，与 OneBot 一致。
            let id = element
                .split("id=\"")
                .nth(1)
                .and_then(|rest| rest.split('"').next())
                .unwrap_or_default();
            out.push('@');
            out.push_str(id);
        } else if element.starts_with("<img ") {
            out.push_str("[图片]");
        } else if element.starts_with("<file ") {
            out.push_str("[文件]");
        } else if element.starts_with("<face ") {
            out.push_str("[表情]");
        }
        // 其他元素（`<br/>` 之类）不产生正文。

        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out.trim().to_owned()
}

fn string_of(value: &Value) -> Option<String> {
    match value {
        // Satori 的 id 都是字符串，但上游实现在个别字段上给数字也要认。
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ready(user: &str) -> String {
        format!(
            r#"{{"op":4,"body":{{"login":{{"status":1,"platform":"qqguild","user":{{"id":"{user}","name":"bot"}}}}}}}}"#
        )
    }

    /// 没有 `login.platform` 的 READY：平台学不到，消息只能丢弃。
    fn ready_without_platform(user: &str) -> String {
        format!(
            r#"{{"op":4,"body":{{"login":{{"status":1,"user":{{"id":"{user}","name":"bot"}}}}}}}}"#
        )
    }

    fn text_event(content: &str, user: &str, guild: &str, channel_type: i64) -> String {
        format!(
            r#"{{"op":0,"sn":1,"body":{{"type":"message-created","message":{{
                "id":"m-1","content":"{content}","created_at":1792561540000,
                "channel":{{"id":"c-1","type":{channel_type}}},"guild":{{"id":"{guild}"}},
                "user":{{"id":"{user}","name":"name","nick":"小明"}}
            }}}}}}"#
        )
    }

    fn transport() -> SatoriTransport {
        let transport = SatoriTransport::new();
        transport.decode(&ready("10001")).expect("READY 应能解码");
        transport
    }

    #[test]
    fn ready_records_the_login_identity() {
        let transport = SatoriTransport::new();
        assert_eq!(transport.login(), None);

        let event = transport.decode(&ready("10001")).expect("应能解码");
        assert_eq!(
            event,
            ChatEvent::Status(ChatStatus::Ready {
                platform: Some("qqguild".to_owned())
            })
        );
        assert_eq!(transport.login().as_deref(), Some("10001"));
        assert_eq!(transport.known_platform().as_deref(), Some("qqguild"));
    }

    #[test]
    fn message_created_becomes_a_neutral_message() {
        let transport = transport();
        let event = transport
            .decode(&text_event("在吗", "30002", "20001", 0))
            .expect("应能解码");

        let ChatEvent::Message(message) = event else {
            panic!("期望消息，得到 {event:?}");
        };
        // 这条是抽象的核心断言：Satori 的 channel/guild/user 全部落成内核的中立字段。
        assert_eq!(
            message.conversation,
            ConversationKey::new("qqguild", "20001"),
            "会话取 guild.id，平台取 READY 的 login.platform"
        );
        assert_eq!(
            message.reply_to,
            Some(ReplyAddress {
                platform: "qqguild".to_owned(),
                address: "c-1".to_owned(),
            }),
            "guild 是会话，channel 是回复目标"
        );
        assert_eq!(message.sender_id, "30002");
        assert_eq!(message.sender_name.as_deref(), Some("小明"), "nick 优先");
        assert_eq!(message.text, "在吗");
        assert_eq!(message.time, 1_792_561_540_000);
        assert!(!message.at_self);
    }

    #[test]
    fn private_channel_is_ignored_not_misread_as_a_group() {
        let transport = transport();
        let event = transport
            .decode(&text_event("私聊", "30002", "20001", 1))
            .expect("应能解码");
        assert!(
            matches!(event, ChatEvent::Ignored { ref kind } if kind == "message:private"),
            "得到 {event:?}"
        );
    }

    #[test]
    fn at_element_marks_at_self_and_flattens_to_plain_text() {
        let transport = transport();
        let event = transport
            .decode(&text_event(
                r#"<at id=\"10001\"/> 在吗"#,
                "30002",
                "20001",
                0,
            ))
            .expect("应能解码");

        let ChatEvent::Message(message) = event else {
            panic!("期望消息");
        };
        assert!(message.at_self, "被 at 到自己的消息要标上");
        assert_eq!(message.text, "@10001 在吗", "元素要拍平成文本");
    }

    #[test]
    fn media_elements_become_the_same_placeholders_as_onebot() {
        // 两个实现在归档里产生的正文形状必须一致，否则下游渲染要分支。
        let transport = transport();
        let event = transport
            .decode(&text_event(
                r#"看 <img src=\"x\"/> 和 <file name=\"y\"/>"#,
                "30002",
                "20001",
                0,
            ))
            .expect("应能解码");
        let ChatEvent::Message(message) = event else {
            panic!("期望消息");
        };
        assert_eq!(message.text, "看 [图片] 和 [文件]");
    }

    #[test]
    fn send_action_is_encoded_as_a_http_request_not_a_ws_frame() {
        let transport = transport();
        let line = transport.encode(
            &ChatAction::Send {
                to: ReplyAddress {
                    platform: "qqguild".to_owned(),
                    address: "c-1".to_owned(),
                },
                text: "在".to_owned(),
            },
            "a-1",
        );

        let value: Value = serde_json::from_str(&line).expect("应是 JSON");
        assert_eq!(value["path"], "/message.create");
        assert_eq!(value["body"]["channel_id"], "c-1", "动作打到频道");
        assert_eq!(value["body"]["content"], "在");
        assert_eq!(value["id"], "a-1");
    }

    #[test]
    fn there_is_no_ws_response_to_pair() {
        // WS 单向事件流：任何帧都不该被当成动作响应。
        let transport = transport();
        assert!(transport.response(r#"{"op":4,"body":{}}"#).is_none());
        assert!(transport.response("garbage").is_none());
    }

    #[test]
    fn unknown_event_types_are_ignored_not_fatal() {
        let transport = transport();
        let event = transport
            .decode(r#"{"op":0,"body":{"type":"guild-added"}}"#)
            .expect("应能解码");
        assert!(matches!(event, ChatEvent::Ignored { .. }));
    }

    #[test]
    fn missing_login_falls_back_to_the_configured_identity() {
        // 没有 READY 就来了事件：至少别把 self_id 变成空串。
        let offline = SatoriTransport::new();
        assert_eq!(offline.self_id("configured"), "configured");

        let connected = transport();
        assert_eq!(
            connected.self_id("configured"),
            "10001",
            "READY 之后以平台为准"
        );
    }

    #[test]
    fn unclosed_element_does_not_lose_text() {
        assert_eq!(flatten_mentions("你好 <at id=\"1\""), "你好 <at id=\"1\"");
        assert_eq!(flatten_mentions("没有元素"), "没有元素");
        assert_eq!(flatten_mentions(""), "");
    }

    #[test]
    fn message_before_ready_is_dropped_not_guessed() {
        // READY 之前不知道平台。猜一个平台名会把消息归进错误的命名空间，
        // 比丢一条更难查——所以显式丢弃并留下日志可读的原因。
        let transport = SatoriTransport::new();
        let event = transport
            .decode(&text_event("在吗", "30002", "20001", 0))
            .expect("应能解码");
        assert!(
            matches!(event, ChatEvent::Ignored { ref kind } if kind == "message:platform_unknown"),
            "得到 {event:?}"
        );
    }

    #[test]
    fn ready_without_platform_still_ignores_messages() {
        // 协议没给 `login.platform` 是不猜，不是编一个。没有平台就没有会话命名空间。
        let transport = SatoriTransport::new();
        transport
            .decode(&ready_without_platform("10001"))
            .expect("READY 应能解码");
        assert_eq!(transport.known_platform(), None, "协议没给就是不猜");

        let event = transport
            .decode(&text_event("在吗", "30002", "20001", 0))
            .expect("应能解码");
        assert!(matches!(event, ChatEvent::Ignored { .. }));
    }
}
