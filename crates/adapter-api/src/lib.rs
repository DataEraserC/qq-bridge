//! 平台适配器 wire 协议（ADR-0027）。
//!
//! 同一份 JSON 消息定义被两种载体共用：
//! - **外置进程**：stdin/stdout 上一行一条 JSON；
//! - **动态库**：C ABI 的 `invoke` 函数收到/返回同样形状的 JSON。
//!
//! core 侧只认识这个形状，不认识平台；适配器实现可以写在任何语言里。
//!
//! 消息方向：
//!
//! ```text
//! core ──▶ adapter   Send / Call / Shutdown
//! adapter ──▶ core   Hello（第一行）/ Event / Reply / Log
//! ```

use acb_proto::{Element, RecordKind, RoomId, Sender};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize as DeriveSerialize};
use serde_json::Value;

/// wire 协议版本。core 与适配器必须对得上。
pub const WIRE_VERSION: u32 = 1;

/// 适配器 manifest 的 wire 形状。
#[derive(Debug, Clone, PartialEq, DeriveSerialize, Deserialize)]
pub struct WireManifest {
    pub id: String,
    pub platform: String,
    pub kind: String,
    #[serde(default)]
    pub transports: Vec<String>,
    pub api_version: u32,
    #[serde(default)]
    pub capabilities: Vec<String>,
}

/// 一条入站记录。字段与 kernel 的 `RecordDraft` 对齐，由 core 负责转换。
#[derive(Debug, Clone, PartialEq, DeriveSerialize, Deserialize)]
pub struct WireRecord {
    pub room: RoomId,
    pub kind: RecordKind,
    pub sender: Sender,
    /// Unix 毫秒。
    pub at: i64,
    #[serde(default)]
    pub elements: Vec<Element>,
    #[serde(default)]
    pub at_self: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform_id: Option<String>,
    #[serde(default)]
    pub reply_to_self: bool,
}

/// 一个方法参数的 wire 形状。
#[derive(Debug, Clone, PartialEq, DeriveSerialize, Deserialize)]
pub struct WireParamSpec {
    pub name: String,
    #[serde(default = "default_param_type")]
    pub ty: String,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub desc: String,
    #[serde(default)]
    pub values: Vec<String>,
}

/// 一个房间目标的 wire 形状。
#[derive(Debug, Clone, PartialEq, DeriveSerialize, Deserialize)]
pub struct WireTargetSpec {
    pub param: String,
    pub room_kind: String,
    #[serde(default = "default_target_role")]
    pub role: String,
}

/// 一个平台方法的 wire 形状。
#[derive(Debug, Clone, PartialEq, DeriveSerialize, Deserialize)]
pub struct WireMethodSpec {
    pub name: String,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default = "default_risk")]
    pub risk: String,
    #[serde(default = "default_scope")]
    pub scope: String,
    #[serde(default)]
    pub params: Vec<WireParamSpec>,
    #[serde(default)]
    pub targets: Vec<WireTargetSpec>,
    #[serde(default = "default_effect")]
    pub effect: String,
    #[serde(default = "default_record")]
    pub record: String,
}

/// 一次发送/调用顺带产生的流水记录。
#[derive(Debug, Clone, PartialEq, DeriveSerialize, Deserialize)]
pub struct WireOutgoingRecord {
    pub kind: WireOutgoingKind,
    #[serde(default)]
    pub elements: Vec<Element>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, DeriveSerialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WireOutgoingKind {
    Message,
    Event { event: String },
}

/// 适配器返回的错误。分类与 kernel 的 `OutboundError` / `CallError` 对齐。
#[derive(Debug, Clone, PartialEq, DeriveSerialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WireError {
    UnknownMethod {
        method: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        suggestion: Option<String>,
    },
    UnsupportedPlatform { detail: String },
    NotImplemented { detail: String },
    Forbidden { detail: String },
    BadParams { detail: String },
    Unavailable { detail: String },
    Rejected { detail: String },
    Unsupported { detail: String },
}

/// core → 适配器。
#[derive(Debug, Clone, PartialEq, DeriveSerialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CoreMessage {
    Send {
        id: u64,
        room: RoomId,
        #[serde(default)]
        elements: Vec<Element>,
    },
    Call {
        id: u64,
        room: RoomId,
        method: String,
        #[serde(default)]
        params: Value,
    },
    Shutdown,
}

/// 适配器 → core 的一次动作回执。
#[derive(Debug, Clone, PartialEq, DeriveSerialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum WireResult {
    Sent {
        platform_id: String,
        at: i64,
    },
    Call {
        #[serde(default)]
        value: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        record: Option<WireOutgoingRecord>,
    },
    Error {
        error: WireError,
    },
}

/// 适配器 → core。
#[derive(Debug, Clone, PartialEq, DeriveSerialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AdapterMessage {
    /// 第一行。声明适配器名字和平台方法目录。
    Hello {
        #[serde(default)]
        name: String,
        #[serde(default)]
        catalog: Vec<WireMethodSpec>,
    },
    Event {
        record: WireRecord,
    },
    Reply {
        id: u64,
        result: WireResult,
    },
    Log {
        #[serde(default)]
        level: String,
        message: String,
    },
}

/// 把一条消息编码成一行 JSON（不含换行）。
pub fn encode_line<T: Serialize>(value: &T) -> Result<String, serde_json::Error> {
    serde_json::to_string(value)
}

/// 从一行 JSON 里解码。
pub fn decode_line<T: DeserializeOwned>(line: &str) -> Result<T, serde_json::Error> {
    serde_json::from_str(line)
}

fn default_param_type() -> String {
    "string".to_owned()
}

fn default_target_role() -> String {
    "room".to_owned()
}

fn default_risk() -> String {
    "read".to_owned()
}

fn default_scope() -> String {
    "room".to_owned()
}

fn default_effect() -> String {
    "read".to_owned()
}

fn default_record() -> String {
    "none".to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use acb_proto::RecordKind;

    #[test]
    fn core_messages_round_trip() {
        let send = CoreMessage::Send {
            id: 7,
            room: RoomId::new("demo", "group/1").expect("合法"),
            elements: vec![Element::text("你好")],
        };
        let line = encode_line(&send).expect("能编码");
        assert!(line.contains("\"type\":\"send\""));
        let back: CoreMessage = decode_line(&line).expect("能解码");
        assert_eq!(back, send);

        let shutdown: CoreMessage = decode_line(r#"{"type":"shutdown"}"#).expect("能解码");
        assert_eq!(shutdown, CoreMessage::Shutdown);
    }

    #[test]
    fn adapter_messages_round_trip() {
        let hello = AdapterMessage::Hello {
            name: "fixture".into(),
            catalog: vec![WireMethodSpec {
                name: "fixture.echo".into(),
                summary: "回显".into(),
                tags: vec!["send".into()],
                risk: "read".into(),
                scope: "room".into(),
                params: vec![WireParamSpec {
                    name: "text".into(),
                    ty: "string".into(),
                    required: true,
                    desc: "正文".into(),
                    values: vec![],
                }],
                targets: vec![],
                effect: "read".into(),
                record: "none".into(),
            }],
        };
        let line = encode_line(&hello).expect("能编码");
        assert!(line.contains("\"type\":\"hello\""));
        let back: AdapterMessage = decode_line(&line).expect("能解码");
        assert_eq!(back, hello);

        let event = AdapterMessage::Event {
            record: WireRecord {
                room: RoomId::new("demo", "group/1").expect("合法"),
                kind: RecordKind::Message,
                sender: Sender {
                    id: "10001".into(),
                    name: "老王".into(),
                    role: None,
                },
                at: 1,
                elements: vec![Element::text("在吗")],
                at_self: false,
                platform_id: Some("m1".into()),
                reply_to_self: false,
            },
        };
        let line = encode_line(&event).expect("能编码");
        let back: AdapterMessage = decode_line(&line).expect("能解码");
        assert_eq!(back, event);
    }

    #[test]
    fn reply_shape_is_stable() {
        let reply = AdapterMessage::Reply {
            id: 3,
            result: WireResult::Error {
                error: WireError::BadParams {
                    detail: "缺参数".into(),
                },
            },
        };
        let line = encode_line(&reply).expect("能编码");
        assert!(line.contains("\"outcome\":\"error\""), "{line}");
        assert!(line.contains("\"kind\":\"bad_params\""), "{line}");
        let back: AdapterMessage = decode_line(&line).expect("能解码");
        assert_eq!(back, reply);
    }
}
