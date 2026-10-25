//! 内核与适配器之间的协议帧。
//!
//! 一行一个 JSON 对象，必须有 `type` 字段。未知 `type` 必须被忽略而不是断连——
//! 这是向前兼容的唯一手段，见 `docs/architecture/05-protocol.md`。

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub mod error;
pub mod frame;

pub use error::{ErrorBody, ErrorCode};
pub use frame::*;

/// 协议主版本。主版本不同即拒绝，不尝试兼容。
///
/// v2 起 `scope` 用 `conversation` 而不是 `group`，并新增 `reply`：会话名与回复目标
/// 在有的平台上是两个不同的东西（Satori 的 guild 与 channel）。见
/// `docs/adr/0011-conversation-identity.md`。
pub const PROTOCOL_VERSION: u32 = 2;

/// 解码一行协议文本。
///
/// 返回 `Ok(Frame::Unknown)` 表示这一帧结构合法但类型未知，调用方应丢弃它并继续。
/// 返回 `Err` 表示这一行根本不是合法 JSON 对象，或缺少 `type` 字段。
pub fn decode(line: &str) -> Result<Frame, DecodeError> {
    let value: Value = serde_json::from_str(line).map_err(|source| DecodeError::NotJson {
        source: Box::new(source),
    })?;

    let object = match value.as_object() {
        Some(object) => object,
        None => return Err(DecodeError::NotObject),
    };

    let type_name = match object.get("type").and_then(Value::as_str) {
        Some(name) => name.to_owned(),
        None => return Err(DecodeError::MissingType),
    };

    let frame = match type_name.as_str() {
        "deliver" => Frame::Deliver(from_value(value)?),
        "cancel" => Frame::Cancel(from_value(value)?),
        "ping" => Frame::Ping(from_value(value)?),
        "tool_result" => Frame::ToolResult(from_value(value)?),
        "hello_ok" => Frame::HelloOk(from_value(value)?),
        "hello_reject" => Frame::HelloReject(from_value(value)?),
        "hello" => Frame::Hello(from_value(value)?),
        "deliver_ok" => Frame::DeliverOk(from_value(value)?),
        "deliver_failed" => Frame::DeliverFailed(from_value(value)?),
        "tool_call" => Frame::ToolCall(from_value(value)?),
        "turn_end" => Frame::TurnEnd(from_value(value)?),
        "pong" => Frame::Pong(from_value(value)?),
        _ => Frame::Unknown {
            type_name,
            raw: value,
        },
    };

    Ok(frame)
}

fn from_value<T: for<'de> Deserialize<'de>>(value: Value) -> Result<T, DecodeError> {
    serde_json::from_value(value).map_err(|source| DecodeError::BadShape {
        source: Box::new(source),
    })
}

/// 编码一帧为一行协议文本（不含换行符）。
pub fn encode(frame: &Frame) -> Result<String, EncodeError> {
    serde_json::to_string(frame).map_err(|source| EncodeError { source })
}

#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    #[error("不是合法 JSON")]
    NotJson {
        #[source]
        source: Box<serde_json::Error>,
    },
    #[error("不是 JSON 对象")]
    NotObject,
    #[error("缺少 type 字段")]
    MissingType,
    #[error("帧结构与 type 不符")]
    BadShape {
        #[source]
        source: Box<serde_json::Error>,
    },
}

#[derive(Debug, thiserror::Error)]
#[error("无法编码协议帧")]
pub struct EncodeError {
    #[source]
    source: serde_json::Error,
}

/// 一个内容块。已经是渲染好的内容，不是 QQ 消息段。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Block {
    Text {
        text: String,
    },
    Image {
        /// 内核可读的绝对路径。
        path: String,
        mime: String,
    },
    File {
        path: String,
        mime: String,
    },
}

/// 本次投递涉及的会话与可用发送账号。
///
/// 字段是 `conversation` 而不是 `group`：内核处理的不再只是 QQ 群，而「群号」这个词在
/// Discord、KOOK 上不成立。`platform` 另给，因为同一个会话号在不同平台上是不同会话。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Scope {
    /// 会话命名空间，如 `qq`、`discord`。
    #[serde(default)]
    pub platform: String,
    /// 该平台内的会话标识。对群聊平台就是群号。
    pub conversation: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation_name: Option<String>,
    /// 账号的**标识**，不是 QQ 号。适配器当作不透明字符串原样回传。
    #[serde(default)]
    pub send_accounts: Vec<String>,
}

/// 一次回复要发到哪儿。
///
/// 与会话分开：有的平台上「收消息的会话」和「发消息的目标」不是同一个对象。Satori 一个
/// guild 可以有多条文本频道，消息从频道来、回复也要回频道去。多数平台两者相同，那时
/// `address` 等于会话号。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplyTo {
    #[serde(default)]
    pub platform: String,
    pub address: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_type_is_ignored_not_fatal() {
        let frame = decode(r#"{"type":"future_frame","whatever":1}"#).expect("结构合法");
        match frame {
            Frame::Unknown { type_name, .. } => assert_eq!(type_name, "future_frame"),
            other => panic!("期望 Unknown，得到 {other:?}"),
        }
    }

    #[test]
    fn missing_type_is_error() {
        assert!(matches!(
            decode(r#"{"id":"x"}"#),
            Err(DecodeError::MissingType)
        ));
    }

    #[test]
    fn non_object_is_error() {
        assert!(matches!(decode("[1,2]"), Err(DecodeError::NotObject)));
        assert!(matches!(
            decode("not json"),
            Err(DecodeError::NotJson { .. })
        ));
    }

    #[test]
    fn deliver_round_trip() {
        let frame = Frame::Deliver(Deliver {
            id: "d-1".into(),
            ticket: "t1-1792561540309".into(),
            memory: "alice".into(),
            persona: "你是……".into(),
            blocks: vec![
                Block::Text {
                    text: "在吗".into(),
                },
                Block::Image {
                    path: "/tmp/a.jpg".into(),
                    mime: "image/jpeg".into(),
                },
            ],
            scope: Scope {
                platform: "qq".into(),
                conversation: "20001".into(),
                conversation_name: Some("测试群".into()),
                send_accounts: vec!["main".into()],
            },
            reply: Some(ReplyTo {
                platform: "qq".into(),
                address: "20001".into(),
            }),
            wake_reason: "at_mention".into(),
            notes: String::new(),
        });

        let line = encode(&frame).expect("可编码");
        assert_eq!(decode(&line).expect("可解码"), frame);
    }

    #[test]
    fn unknown_fields_are_ignored() {
        // 次版本差异靠「未知字段忽略」处理。
        let line = r#"{"type":"ping","id":"p-1","future_field":true}"#;
        assert!(matches!(decode(line), Ok(Frame::Ping(_))));
    }

    #[test]
    fn missing_optional_fields_use_defaults() {
        let line = r#"{"type":"deliver","id":"d-1","ticket":"t1-1","memory":"alice","persona":"p","blocks":[],"scope":{"conversation":"1"},"wake_reason":"any_message"}"#;
        let Frame::Deliver(deliver) = decode(line).expect("可解码") else {
            panic!("期望 deliver");
        };
        assert_eq!(deliver.notes, "");
        assert_eq!(deliver.ticket, "t1-1");
        assert!(deliver.scope.send_accounts.is_empty());
    }

    #[test]
    fn deliver_without_ticket_is_rejected() {
        // 票是工具调用的唯一身份来源；缺了它这一帧结构非法，不能当成「无票回合」放行。
        let line = r#"{"type":"deliver","id":"d-1","memory":"alice","persona":"p","blocks":[],"scope":{"group":"1"},"wake_reason":"any_message"}"#;
        assert!(matches!(decode(line), Err(DecodeError::BadShape { .. })));
    }

    #[test]
    fn hello_carries_version() {
        let Frame::Hello(hello) =
            decode(r#"{"type":"hello","id":"h-1","version":2,"adapter":"dsh"}"#).unwrap()
        else {
            panic!("期望 hello");
        };
        assert_eq!(hello.version, PROTOCOL_VERSION);
        assert_eq!(hello.adapter, "dsh");
        // 本机形态不写令牌：字段缺省不出现，也不影响解码。
        assert_eq!(hello.token, None);
    }

    #[test]
    fn hello_carries_token_across_machines() {
        // 跨机器形态必须带令牌；字段在同一帧里，不另开一帧。
        let line = r#"{"type":"hello","id":"h-1","version":2,"adapter":"dsh","token":"s3cret"}"#;
        let Frame::Hello(hello) = decode(line).expect("应能解码") else {
            panic!("期望 hello");
        };
        assert_eq!(hello.token.as_deref(), Some("s3cret"));
    }

    #[test]
    fn tool_result_carries_error_body() {
        let line = r#"{"type":"tool_result","id":"t-7","error":{"code":"ticket_expired","message":"票已作废"}}"#;
        let Frame::ToolResult(result) = decode(line).expect("可解码") else {
            panic!("期望 tool_result");
        };
        let error = result.error.expect("应带错误");
        assert_eq!(error.code, ErrorCode::TicketExpired);
    }

    #[test]
    fn error_codes_are_closed_set() {
        assert!(serde_json::from_str::<ErrorCode>(r#""ticket_expired""#).is_ok());
        assert!(serde_json::from_str::<ErrorCode>(r#""made_up""#).is_err());
    }
}
