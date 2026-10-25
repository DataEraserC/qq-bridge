//! OneBot 11 事件的解码与动作的编码。
//!
//! 这一层只做「JSON 行 ↔ 本内核的形状」，不持有连接、不读时钟、不重试——传输在别处，
//! 所以每个转换都能用一段固定文本测。见 `docs/architecture/03-kernel.md#连接管理`。
//!
//! 三条取舍：
//!
//! - **只解出判定要用的字段。** 内核不需要 OneBot 事件的全集；未知的 `post_type` 与未知的
//!   段类型一律忽略而不是报错——上游实现（SnowLuma、Lagrange）的扩展字段随时在变。
//! - **群号、消息号、QQ 号一律按字符串持有。** 上游把它们序列化成 number 还是 string 不
//!   一致，用数字接会在某些实现上丢精度，用字符串比较才稳。
//! - **媒体只留占位符。** 归档要的是已落地的路径，而落媒体属于工具面/连接之外的职责；
//!   在此之前图片与文件在正文里留 `[图片]` `[文件]`，不让消息因为「附件还没实现」而消失。

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::archive::Message;

/// 一帧 OneBot 事件的解码结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Message(MessageEvent),
    /// 拍一拍。它不是消息，不进归档，但要走同一条唤醒判定链。
    Poke(PokeEvent),
    /// 生命周期：`connect` 表示这条连接可以收发了。
    Lifecycle(LifecycleEvent),
    Heartbeat(HeartbeatEvent),
    /// 结构合法但不归本层处理（如 `notice/group_upload`、`request/friend`）。
    ///
    /// 保留它的类型名只为日志可读；调用方应丢弃而不是断连。
    Ignored {
        kind: String,
    },
}

/// 一条群消息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageEvent {
    /// 收到它的连接所属账号的 QQ 号（`self_id`）。
    pub self_id: String,
    pub group_id: String,
    pub message_id: String,
    pub user_id: String,
    /// 群名片优先，没有则昵称。只是显示数据，不参与判定。
    pub sender_name: Option<String>,
    /// 正文。媒体已换成占位符，见模块文档。
    pub text: String,
    /// 消息里有 `@` 指向 `self_id`。
    pub at_self: bool,
    /// Unix 毫秒。
    pub time: i64,
}

impl MessageEvent {
    /// 转成归档消息。`account` 是配置里的账号标识，不是 QQ 号。
    ///
    /// `platform` 由调用方给：OneBot 的帧里没有平台字段，它是什么平台由**部署**决定
    /// （同一个 OneBot 实现可以接 QQ，也可以接别的平台）。
    ///
    /// `own` 不在这里判定：它是**设备**的属性（设备声明的账号集合），同一个事件进入两台
    /// 设备时结论可能不同，所以由 [`crate::conn::route`] 在路由时按设备填。
    pub fn into_archive(&self, account: &str, platform: &str) -> Message {
        Message {
            seq: 0,
            account: account.to_owned(),
            platform: platform.to_owned(),
            group: self.group_id.clone(),
            // OneBot 上群号既是会话也是目标：没有「一个群多个频道」这回事。
            reply: None,
            message_id: self.message_id.clone(),
            sender_id: self.user_id.clone(),
            sender_name: self.sender_name.clone(),
            text: self.text.clone(),
            at_self: self.at_self,
            own: false,
            at: self.time,
            media: Vec::new(),
        }
    }
}

/// 一次拍一拍。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PokeEvent {
    pub group_id: Option<String>,
    /// 谁拍的。
    pub user_id: String,
    /// 拍了谁。只有拍的是本账号时才值得判定，但内核不知道「本账号」，所以两个都留下。
    pub target_id: String,
    pub self_id: String,
}

impl PokeEvent {
    /// 这次拍一拍是否冲着本账号来的。
    pub fn targets_self(&self) -> bool {
        self.target_id == self.self_id
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LifecycleEvent {
    pub self_id: String,
    /// `connect` / `enable` / `disable`。
    pub sub_type: String,
}

impl LifecycleEvent {
    /// 连接是否已可收发。`enable` 与 `connect` 都算——不同实现只发其中一个。
    pub fn is_ready(&self) -> bool {
        self.sub_type == "connect" || self.sub_type == "enable"
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeartbeatEvent {
    pub self_id: String,
    /// 账号是否在线。
    pub online: bool,
    /// 上游对「QQ → 实现」这条接收链路的自评。老实现不发这个字段。
    pub receive_healthy: Option<bool>,
}

/// 解码一帧 OneBot 事件。
pub fn decode(line: &str) -> Result<Event, DecodeError> {
    let value: Value = serde_json::from_str(line).map_err(|source| DecodeError::NotJson {
        source: Box::new(source),
    })?;
    let object = value.as_object().ok_or(DecodeError::NotObject)?;

    let post_type = string_field(object, "post_type").ok_or(DecodeError::MissingPostType)?;
    let self_id = string_field(object, "self_id").unwrap_or_default();

    match post_type.as_str() {
        "message" => decode_message(object, self_id),
        "notice" => Ok(decode_notice(object, self_id)),
        "meta_event" => decode_meta(object, self_id),
        other => Ok(Event::Ignored {
            kind: format!("post_type:{other}"),
        }),
    }
}

fn decode_message(
    object: &serde_json::Map<String, Value>,
    self_id: String,
) -> Result<Event, DecodeError> {
    // 私聊不走设备与群的范围模型——内核只处理群。保留成 Ignored 让日志能看出它来过。
    if string_field(object, "message_type").as_deref() != Some("group") {
        return Ok(Event::Ignored {
            kind: "message:private".to_owned(),
        });
    }

    let group_id = string_field(object, "group_id").ok_or(DecodeError::MissingField("group_id"))?;
    let message_id =
        string_field(object, "message_id").ok_or(DecodeError::MissingField("message_id"))?;
    let user_id = string_field(object, "user_id").ok_or(DecodeError::MissingField("user_id"))?;

    let sender_name = object
        .get("sender")
        .and_then(Value::as_object)
        .and_then(|sender| {
            // 群名片优先：群里叫得出名字的是 card，昵称常年不改。
            string_field(sender, "card")
                .filter(|card| !card.is_empty())
                .or_else(|| string_field(sender, "nickname"))
        });

    let segments = object
        .get("message")
        .ok_or(DecodeError::MissingField("message"))?;
    let (text, at_self) = flatten_segments(segments, &self_id);

    let time = object
        .get("time")
        .and_then(Value::as_i64)
        .map(|seconds| seconds * 1000)
        // 没有时间就记 0：它只用于展示，不参与任何判定。
        .unwrap_or(0);

    Ok(Event::Message(MessageEvent {
        self_id,
        group_id,
        message_id,
        user_id,
        sender_name,
        text,
        at_self,
        time,
    }))
}

fn decode_notice(object: &serde_json::Map<String, Value>, self_id: String) -> Event {
    let notice_type = string_field(object, "notice_type").unwrap_or_default();
    let sub_type = string_field(object, "sub_type").unwrap_or_default();

    if notice_type == "notify" && sub_type == "poke" {
        // 拍一拍的发起者字段在不同实现里叫 user_id 或 sender_id。
        let user_id = string_field(object, "user_id")
            .or_else(|| string_field(object, "sender_id"))
            .unwrap_or_default();
        return Event::Poke(PokeEvent {
            group_id: string_field(object, "group_id"),
            user_id,
            target_id: string_field(object, "target_id").unwrap_or_default(),
            self_id,
        });
    }

    Event::Ignored {
        kind: format!("notice:{notice_type}/{sub_type}"),
    }
}

fn decode_meta(
    object: &serde_json::Map<String, Value>,
    self_id: String,
) -> Result<Event, DecodeError> {
    match string_field(object, "meta_event_type").as_deref() {
        Some("lifecycle") => Ok(Event::Lifecycle(LifecycleEvent {
            self_id,
            sub_type: string_field(object, "sub_type").unwrap_or_default(),
        })),
        Some("heartbeat") => {
            let status = object.get("status").and_then(Value::as_object);
            Ok(Event::Heartbeat(HeartbeatEvent {
                self_id,
                online: status
                    .and_then(|status| status.get("online"))
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                receive_healthy: status
                    .and_then(|status| status.get("good"))
                    .and_then(Value::as_bool),
            }))
        }
        other => Ok(Event::Ignored {
            kind: format!("meta_event:{}", other.unwrap_or("?")),
        }),
    }
}

/// 把消息段拍平成正文，顺带判断有没有 @ 自己。
///
/// 段可以是数组（标准），也可以是字符串（部分实现的 CQ 码退化态）。字符串形态原样收下：
/// 解析 CQ 码属于上游实现的事，内核按纯文本对待不会丢消息。
fn flatten_segments(segments: &Value, self_id: &str) -> (String, bool) {
    if let Some(raw) = segments.as_str() {
        return (
            raw.to_owned(),
            raw.contains(&format!("[CQ:at,qq={self_id}]")),
        );
    }

    let Some(items) = segments.as_array() else {
        return (String::new(), false);
    };

    let mut text = String::new();
    let mut at_self = false;

    for item in items {
        let Some(segment) = item.as_object() else {
            continue;
        };
        let kind = segment
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let data = segment.get("data").and_then(Value::as_object);

        match kind {
            "text" => {
                if let Some(inner) = data.and_then(|data| string_field(data, "text")) {
                    text.push_str(&inner);
                }
            }
            "at" => {
                let qq = data
                    .and_then(|data| string_field(data, "qq"))
                    .unwrap_or_default();
                if qq == self_id {
                    // 正文不写「@自己」：投递渲染统一加 `@你 ` 前缀，这里再写一遍会重复。
                    at_self = true;
                } else {
                    text.push('@');
                    text.push_str(&qq);
                }
            }
            "image" => text.push_str("[图片]"),
            "file" => text.push_str("[文件]"),
            "mface" => text.push_str("[表情]"),
            "forward" => text.push_str("[聊天记录]"),
            // reply / face / json / xml / 未知段：不产生正文。回复的目标已经在归档的
            // 上下文里，重复摘一份反而会让模型以为是新内容。
            _ => {}
        }
    }

    (text.trim().to_owned(), at_self)
}

fn string_field(object: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
    match object.get(key)? {
        // 上游把这些 id 序列化成 number 或 string 的情况都存在。
        Value::String(value) => Some(value.clone()),
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    }
}

/// 要发给 OneBot 实现的动作。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    SendGroupMsg { group_id: String, text: String },
}

impl Action {
    /// 编成一行 OneBot 动作帧（不含换行符）。
    ///
    /// `echo` 由调用方给，用于把响应与请求对上——协议层的请求标识不在这一层生成。
    pub fn encode(&self, echo: &str) -> String {
        let frame = match self {
            Self::SendGroupMsg { group_id, text } => json!({
                "action": "send_group_msg",
                "params": { "group_id": group_id, "message": text },
                "echo": echo,
            }),
        };
        frame.to_string()
    }
}

/// 动作响应。`status` 与 `retcode` 是 OneBot 的标准字段。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionResponse {
    pub status: String,
    pub retcode: i64,
    #[serde(default)]
    pub echo: Option<String>,
    #[serde(default)]
    pub data: Option<Value>,
}

impl ActionResponse {
    /// 解码一行动作响应。
    pub fn decode(line: &str) -> Result<Self, DecodeError> {
        serde_json::from_str(line).map_err(|source| DecodeError::NotJson {
            source: Box::new(source),
        })
    }

    /// 上游是否接受了这个动作。`retcode != 0` 即失败——不看 `status` 字符串。
    pub fn is_ok(&self) -> bool {
        self.retcode == 0
    }

    /// 失败原因，用于日志。
    pub fn reason(&self) -> String {
        self.data
            .as_ref()
            .and_then(|data| data.get("message").or_else(|| data.get("wording")))
            .and_then(Value::as_str)
            .map_or_else(
                || format!("status={} retcode={}", self.status, self.retcode),
                str::to_owned,
            )
    }
}

/// 解码失败的原因。与协议层同构，但不共用类型：OneBot 与内核-适配器协议是两份接口。
#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    #[error("不是合法 JSON")]
    NotJson {
        #[source]
        source: Box<serde_json::Error>,
    },
    #[error("不是 JSON 对象")]
    NotObject,
    #[error("缺少 post_type 字段")]
    MissingPostType,
    #[error("缺少 {0} 字段")]
    MissingField(&'static str),
}

/// OneBot 11 的 [`Transport`] 实现。
///
/// 这是第一个实现，也是抽象的验证对象：如果它对内核的接口要求不止
/// [`ChatEvent`](crate::chat::ChatEvent) 那几个分支，抽象就是错的。
///
/// **平台名由配置给。** OneBot 的帧里没有平台字段——`self_id` 是 QQ 号、`group_id` 是
/// 群号，但没有一个字说这是哪个平台。所以平台是部署者的事实，不是协议的事实。
#[derive(Debug, Clone)]
pub struct OneBot {
    /// 会话命名空间。缺省 `qq`：最常见的部署是 QQ，且旧配置不必改。
    platform: String,
}

impl OneBot {
    pub fn new() -> Self {
        Self {
            platform: "qq".to_owned(),
        }
    }

    /// 指定平台名。
    pub fn with_platform(platform: impl Into<String>) -> Self {
        Self {
            platform: platform.into(),
        }
    }
}

impl Default for OneBot {
    fn default() -> Self {
        Self::new()
    }
}

impl crate::chat::Transport for OneBot {
    fn name(&self) -> &'static str {
        "onebot11"
    }

    /// OneBot 的平台名是配置声明的，所以连接一建立就知道。
    fn platform(&self) -> Option<String> {
        Some(self.platform.clone())
    }

    fn decode(&self, line: &str) -> Result<crate::chat::ChatEvent, crate::chat::TransportError> {
        Ok(match decode(line)? {
            Event::Message(message) => crate::chat::ChatEvent::Message(crate::chat::ChatMessage {
                self_id: message.self_id,
                conversation: crate::chat::ConversationKey::new(&self.platform, message.group_id),
                // OneBot 上会话就是目标：群号既是收信的地方也是发信的地方。
                reply_to: None,
                message_id: message.message_id,
                sender_id: message.user_id,
                sender_name: message.sender_name,
                text: message.text,
                at_self: message.at_self,
                time: message.time,
            }),
            Event::Poke(poke) => crate::chat::ChatEvent::Poke(crate::chat::ChatPoke {
                conversation: poke
                    .group_id
                    .map(|group| crate::chat::ConversationKey::new(&self.platform, group)),
                sender_id: poke.user_id,
                target_id: poke.target_id,
                self_id: poke.self_id,
            }),
            Event::Lifecycle(lifecycle) => {
                // `enable`/`connect` 都算就绪；其他子类型（比如 `disable`）不是内核要的状态。
                if lifecycle.is_ready() {
                    crate::chat::ChatEvent::Status(crate::chat::ChatStatus::Ready {
                        platform: Some(self.platform.clone()),
                    })
                } else {
                    crate::chat::ChatEvent::Ignored {
                        kind: format!("lifecycle:{}", lifecycle.sub_type),
                    }
                }
            }
            Event::Heartbeat(heartbeat) => {
                if heartbeat.online {
                    // 在线心跳不改变状态：就绪由 lifecycle 给，心跳只是确认。
                    crate::chat::ChatEvent::Ignored {
                        kind: "heartbeat:online".to_owned(),
                    }
                } else {
                    crate::chat::ChatEvent::Status(crate::chat::ChatStatus::Offline)
                }
            }
            Event::Ignored { kind } => crate::chat::ChatEvent::Ignored { kind },
        })
    }

    fn encode(&self, action: &crate::chat::ChatAction, id: &str) -> String {
        let mapped = match action {
            crate::chat::ChatAction::Send { to, text } => Action::SendGroupMsg {
                group_id: to.address.clone(),
                text: text.clone(),
            },
        };
        mapped.encode(id)
    }

    fn response(&self, line: &str) -> Option<(Option<String>, crate::chat::ChatOutcome)> {
        let response = ActionResponse::decode(line).ok()?;
        let outcome = if response.is_ok() {
            crate::chat::ChatOutcome::Ok
        } else {
            crate::chat::ChatOutcome::Rejected {
                reason: response.reason(),
            }
        };
        Some((response.echo, outcome))
    }
}

impl From<DecodeError> for crate::chat::TransportError {
    fn from(source: DecodeError) -> Self {
        match source {
            DecodeError::NotJson { source } => Self::NotJson {
                detail: source.to_string(),
            },
            DecodeError::NotObject => Self::NotObject,
            DecodeError::MissingPostType => Self::MissingField("post_type"),
            DecodeError::MissingField(field) => Self::MissingField(field),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group_message(extra: &str) -> String {
        format!(
            r#"{{"post_type":"message","message_type":"group","self_id":10001,
                "group_id":20001,"message_id":"7451","user_id":30002,
                "sender":{{"user_id":30002,"nickname":"小明"}},
                "time":1792561540,"message":[{{"type":"text","data":{{"text":"在吗"}}}}]{extra}}}"#
        )
    }

    #[test]
    fn group_message_decodes_with_stringified_ids() {
        let Event::Message(message) = decode(&group_message("")).expect("应能解码") else {
            panic!("期望群消息");
        };
        // number 形态的 id 一律拍成字符串。
        assert_eq!(message.group_id, "20001");
        assert_eq!(message.message_id, "7451");
        assert_eq!(message.self_id, "10001");
        assert_eq!(message.user_id, "30002");
        assert_eq!(message.sender_name.as_deref(), Some("小明"));
        assert_eq!(message.text, "在吗");
        // OneBot 的 time 是秒，归档要毫秒。
        assert_eq!(message.time, 1_792_561_540_000);
        assert!(!message.at_self);
    }

    #[test]
    fn card_beats_nickname() {
        let line = r#"{"post_type":"message","message_type":"group","self_id":1,
            "group_id":2,"message_id":3,"user_id":4,
            "sender":{"nickname":"小明","card":"群名片"},"message":[]}"#;
        let Event::Message(message) = decode(line).expect("应能解码") else {
            panic!("期望群消息");
        };
        assert_eq!(message.sender_name.as_deref(), Some("群名片"));
    }

    #[test]
    fn empty_card_falls_back_to_nickname() {
        // 没设群名片时上游可能给空字符串，不能被当成名字。
        let line = r#"{"post_type":"message","message_type":"group","self_id":1,
            "group_id":2,"message_id":3,"user_id":4,
            "sender":{"nickname":"小明","card":""},"message":[]}"#;
        let Event::Message(message) = decode(line).expect("应能解码") else {
            panic!("期望群消息");
        };
        assert_eq!(message.sender_name.as_deref(), Some("小明"));
    }

    #[test]
    fn at_self_is_detected_and_not_duplicated_in_text() {
        let line = r#"{"post_type":"message","message_type":"group","self_id":10001,
            "group_id":2,"message_id":3,"user_id":4,
            "message":[{"type":"at","data":{"qq":"10001"}},{"type":"text","data":{"text":" 在吗"}}]}"#;
        let Event::Message(message) = decode(line).expect("应能解码") else {
            panic!("期望群消息");
        };
        assert!(message.at_self);
        // 前缀由投递渲染加，正文里不能自带「@自己」。
        assert_eq!(message.text, "在吗");
    }

    #[test]
    fn at_someone_else_stays_as_text() {
        let line = r#"{"post_type":"message","message_type":"group","self_id":10001,
            "group_id":2,"message_id":3,"user_id":4,
            "message":[{"type":"at","data":{"qq":"30002"}},{"type":"text","data":{"text":" 看这个"}}]}"#;
        let Event::Message(message) = decode(line).expect("应能解码") else {
            panic!("期望群消息");
        };
        assert!(!message.at_self);
        assert_eq!(message.text, "@30002 看这个");
    }

    #[test]
    fn media_segments_become_placeholders() {
        let line = r#"{"post_type":"message","message_type":"group","self_id":1,
            "group_id":2,"message_id":3,"user_id":4,
            "message":[{"type":"image","data":{"file":"a.jpg"}},
                       {"type":"file","data":{"file":"b.zip"}},
                       {"type":"reply","data":{"id":"9"}}]}"#;
        let Event::Message(message) = decode(line).expect("应能解码") else {
            panic!("期望群消息");
        };
        // 回复段不进正文：目标那条已在归档上下文里。
        assert_eq!(message.text, "[图片][文件]");
    }

    #[test]
    fn string_message_is_taken_as_plain_text() {
        // 部分实现退化成 CQ 码字符串。
        let line = r#"{"post_type":"message","message_type":"group","self_id":10001,
            "group_id":2,"message_id":3,"user_id":4,"message":"[CQ:at,qq=10001] 在吗"}"#;
        let Event::Message(message) = decode(line).expect("应能解码") else {
            panic!("期望群消息");
        };
        assert!(message.at_self);
        assert_eq!(message.text, "[CQ:at,qq=10001] 在吗");
    }

    #[test]
    fn private_message_is_ignored_not_fatal() {
        let line = r#"{"post_type":"message","message_type":"private","self_id":1,
            "user_id":2,"message":[]}"#;
        assert!(matches!(
            decode(line).expect("应能解码"),
            Event::Ignored { .. }
        ));
    }

    #[test]
    fn poke_is_decoded_with_both_ids() {
        let line = r#"{"post_type":"notice","notice_type":"notify","sub_type":"poke",
            "self_id":10001,"group_id":20001,"user_id":30002,"target_id":10001}"#;
        let Event::Poke(poke) = decode(line).expect("应能解码") else {
            panic!("期望拍一拍");
        };
        assert_eq!(poke.group_id.as_deref(), Some("20001"));
        assert_eq!(poke.user_id, "30002");
        assert!(poke.targets_self());
    }

    #[test]
    fn poke_at_someone_else_is_not_for_us() {
        let line = r#"{"post_type":"notice","notice_type":"notify","sub_type":"poke",
            "self_id":10001,"group_id":20001,"user_id":30002,"target_id":30003}"#;
        let Event::Poke(poke) = decode(line).expect("应能解码") else {
            panic!("期望拍一拍");
        };
        assert!(!poke.targets_self());
    }

    #[test]
    fn other_notices_are_ignored_with_their_kind() {
        let line = r#"{"post_type":"notice","notice_type":"group_upload","sub_type":"",
            "self_id":1,"group_id":2,"user_id":3}"#;
        match decode(line).expect("应能解码") {
            Event::Ignored { kind } => assert_eq!(kind, "notice:group_upload/"),
            other => panic!("期望忽略，得到 {other:?}"),
        }
    }

    #[test]
    fn lifecycle_marks_connection_ready() {
        let line = r#"{"post_type":"meta_event","meta_event_type":"lifecycle",
            "sub_type":"connect","self_id":10001}"#;
        let Event::Lifecycle(lifecycle) = decode(line).expect("应能解码") else {
            panic!("期望生命周期");
        };
        assert!(lifecycle.is_ready());
        assert_eq!(lifecycle.self_id, "10001");
    }

    #[test]
    fn disable_lifecycle_is_not_ready() {
        let line = r#"{"post_type":"meta_event","meta_event_type":"lifecycle",
            "sub_type":"disable","self_id":10001}"#;
        let Event::Lifecycle(lifecycle) = decode(line).expect("应能解码") else {
            panic!("期望生命周期");
        };
        assert!(!lifecycle.is_ready());
    }

    #[test]
    fn heartbeat_reads_receive_health_when_present() {
        let line = r#"{"post_type":"meta_event","meta_event_type":"heartbeat",
            "self_id":10001,"status":{"online":true,"good":false}}"#;
        let Event::Heartbeat(heartbeat) = decode(line).expect("应能解码") else {
            panic!("期望心跳");
        };
        assert!(heartbeat.online);
        assert_eq!(heartbeat.receive_healthy, Some(false));
    }

    #[test]
    fn heartbeat_without_status_is_tolerated() {
        // 老实现的心跳没有 status 字段，不能因此断连。
        let line = r#"{"post_type":"meta_event","meta_event_type":"heartbeat","self_id":1}"#;
        let Event::Heartbeat(heartbeat) = decode(line).expect("应能解码") else {
            panic!("期望心跳");
        };
        assert!(!heartbeat.online);
        assert_eq!(heartbeat.receive_healthy, None);
    }

    #[test]
    fn missing_group_id_is_an_error() {
        let line = r#"{"post_type":"message","message_type":"group","self_id":1,
            "message_id":3,"user_id":4,"message":[]}"#;
        assert!(matches!(
            decode(line),
            Err(DecodeError::MissingField("group_id"))
        ));
    }

    #[test]
    fn missing_post_type_is_an_error() {
        assert!(matches!(
            decode(r#"{"self_id":1}"#),
            Err(DecodeError::MissingPostType)
        ));
        assert!(matches!(
            decode("not json"),
            Err(DecodeError::NotJson { .. })
        ));
        assert!(matches!(decode("[1]"), Err(DecodeError::NotObject)));
    }

    #[test]
    fn unknown_post_type_is_ignored() {
        let line = r#"{"post_type":"future_type","self_id":1}"#;
        match decode(line).expect("应能解码") {
            Event::Ignored { kind } => assert_eq!(kind, "post_type:future_type"),
            other => panic!("期望忽略，得到 {other:?}"),
        }
    }

    #[test]
    fn into_archive_keeps_account_not_qq_number() {
        let Event::Message(message) = decode(&group_message("")).expect("应能解码") else {
            panic!("期望群消息");
        };
        let archived = message.into_archive("main", "qq");
        assert_eq!(archived.account, "main");
        assert_eq!(archived.platform, "qq", "平台由配置给，协议里没有");
        assert_eq!(archived.group, "20001");
        assert_eq!(archived.message_id, "7451");
        assert_eq!(archived.sender_id, "30002");
        // own 由设备集合决定，这一层不能先填。
        assert!(!archived.own);
        assert!(archived.media.is_empty());
    }

    #[test]
    fn send_group_msg_encodes_action_frame() {
        let line = Action::SendGroupMsg {
            group_id: "20001".to_owned(),
            text: "在".to_owned(),
        }
        .encode("a-1");

        let value: Value = serde_json::from_str(&line).expect("应是 JSON");
        assert_eq!(value["action"], "send_group_msg");
        assert_eq!(value["params"]["group_id"], "20001");
        assert_eq!(value["params"]["message"], "在");
        assert_eq!(value["echo"], "a-1");
    }

    #[test]
    fn action_response_ok_depends_on_retcode() {
        let ok = ActionResponse::decode(r#"{"status":"ok","retcode":0,"echo":"a-1"}"#)
            .expect("应能解码");
        assert!(ok.is_ok());
        assert_eq!(ok.echo.as_deref(), Some("a-1"));

        // status 说 ok 但 retcode 非零：以 retcode 为准。
        let bad = ActionResponse::decode(
            r#"{"status":"ok","retcode":1200,"data":{"message":"群不存在"}}"#,
        )
        .expect("应能解码");
        assert!(!bad.is_ok());
        assert_eq!(bad.reason(), "群不存在");
    }
}
