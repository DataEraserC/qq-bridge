//! 协议帧。每帧一行 JSON，`frame` 字段是帧名。
//!
//! 帧集合是封闭的：加帧要改这里并补测试。加帧属于兼容演进（旧的一端会因为不认识的
//! 帧名而报错，所以升级要两端一起），改已有帧的语义则不是。

use serde::{Deserialize, Serialize};

use crate::error::{DecodeError, ProtoError};
use crate::record::Record;
use crate::room::RoomId;
use crate::ticket::Ticket;

/// 协议版本。握手时两端必须一致。
///
/// 2：`ready` 从「绑一台设备」改成「绑一组设备」——一把接入令牌可以代表同一份记忆的
/// 多台设备，`send_message_to` 因此才能跨设备发。这是改语义，不是加帧，所以要升版本。
/// 3：device 概念升级为「harness 会话」——`hello` 加 `harness` 字段，`delivery` 的
/// `device`/`memory` 合并成 `session`，串行键改成「harness 名 + 会话 id」（ADR-0017）。
/// 4：加控制面——`control`（桥→harness 发指令）和 `control_result`（harness 回报），
/// 供桥侧统一控制台触达 harness 内部（ADR-0018）。
pub const PROTOCOL_VERSION: u32 = 4;

/// 单帧上限：1 MiB。超了直接断连，不做截断。
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

/// 一次投递：开一轮时发给 harness 的东西。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Delivery {
    /// 本轮的票。
    pub ticket: Ticket,
    /// 本轮替哪个会话说话。
    pub session: String,
    /// 本轮只涉及这一个房间。
    pub room: RoomId,
    /// 要投递的记录，可能为空（时钟策略的例行检查）。
    pub records: Vec<Record>,
    /// 为什么开这一轮。
    pub reason: TurnReason,
    /// 这个会话引用的记忆：每份的 id + worktree 路径。harness 据此让 agent 操作记忆
    /// （文件 + git），`memory_*` 工具用记忆 id 指认要碰哪一份。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub memories: Vec<MemoryInfo>,
    /// 这个会话的身份信息（名称、归哪个 harness、用哪个模型）。harness 据此知道
    /// 「该以什么身份说话、用哪个模型」。可空是为了兼容旧协议帧。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_info: Option<SessionInfo>,
    /// 还有多少条没投进来（桥按 `delivery_limit` 截断了）。`None` = 全投了，没有截断。
    /// harness 渲染时据此提示模型「用 fetch_records 抓剩下的」。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub more: Option<usize>,
}

/// 一个会话在本轮投递里的身份信息。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionInfo {
    /// 会话 id（与 `Delivery.session` 一致）。
    pub id: String,
    /// 会话显示名（缺省 = id）。
    pub name: String,
    /// 归哪个 harness 接管。
    pub harness: String,
    /// 用哪个模型；没配就是 `None`，由 harness 侧决定。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// 一条「acb 会话 ↔ harness 内部会话」的映射，握手时随 `Ready` 下发，
/// 让 harness 跨进程重启也能复用同一个内部会话（ADR-0018 的 session_maps 表）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMapRef {
    /// acb 会话 id。
    pub session: String,
    /// harness 内部会话 id（dsh 的 sessionId 之类）。
    pub harness_session: String,
}

/// 一份记忆在本轮投递里的信息。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryInfo {
    /// 记忆 id，`memory_*` 工具的 `memory` 参数用它。
    pub id: String,
    /// 这个会话在该记忆上的 worktree 路径（绝对路径）。agent 通过 `memory_*` 工具碰它，
    /// 桥负责把路径锁死在 worktree 内。
    pub worktree: String,
}

/// 开轮原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnReason {
    /// 被事件或消息叫醒（含攒够条数）。
    Wake,
    /// 时钟策略的例行检查。
    Clock,
}

/// 日志级别。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

/// 工具失败的错误码。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// 参数不对。
    BadRequest,
    /// 票不对或已作废。
    Unauthorized,
    /// 这个设备不允许调用该工具。
    ToolNotAllowed,
    /// 目标房间不在订阅范围内。
    RoomNotSubscribed,
    /// 等超时或平台超时。
    Timeout,
    /// 桥自己出毛病了。
    Internal,
}

/// 工具失败的描述。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolError {
    pub code: ErrorCode,
    pub message: String,
}

impl ToolError {
    /// 常见错误码的构造捷径。
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// 一帧。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "frame", rename_all = "snake_case")]
pub enum Frame {
    /// harness → 桥：握手。
    Hello {
        version: u32,
        /// harness 的名字。串行键 = harness 名 + 会话 id（ADR-0017）。
        harness: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        token: Option<String>,
    },
    /// 桥 → harness：握手成功，绑定到这个 harness 接管的一组会话。
    ///
    /// 顺序即配置里的顺序。单会话时只有一个元素。
    Ready {
        sessions: Vec<String>,
        /// 已落盘的会话映射（acb 会话 ↔ harness 内部会话）。harness 用它复用旧会话，
        /// 跨进程重启也不丢上下文。没有就是空列表。
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        maps: Vec<SessionMapRef>,
    },
    /// 桥 → harness：开一轮。
    TurnStart { delivery: Delivery },
    /// harness → 桥：本轮收工。
    TurnEnd {
        ticket: Ticket,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// harness → 桥：调用工具。
    ToolCall {
        id: String,
        ticket: Ticket,
        tool: String,
        #[serde(default)]
        args: serde_json::Value,
    },
    /// 桥 → harness：工具结果。
    ToolResult {
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<serde_json::Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<ToolError>,
    },
    /// 桥 → harness：日志。
    Log { level: LogLevel, message: String },
    /// 桥 → harness：控制指令。控制台发起的、要 harness 自己执行的动作（如重置会话）。
    /// 桥只转发，不替 harness 做——具体语义由 harness 决定（ADR-0018）。
    Control {
        /// 控制指令 id，harness 回报时原样带回来，好配对。
        id: String,
        /// 作用于哪个会话。
        session: String,
        /// 指令名。目前只有 `reset`（重置该会话的映射）。
        command: String,
        /// 指令参数。
        #[serde(default)]
        args: serde_json::Value,
    },
    /// harness → 桥：控制指令的结果。
    ControlResult {
        /// 对应哪条 `control` 的 id。
        id: String,
        session: String,
        command: String,
        /// 执行结果。失败时是字符串错误，成功时是 harness 回的结构。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<serde_json::Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// 保活。
    Ping,
    /// 保活回应。
    Pong,
}

impl Frame {
    /// 帧名，和 JSON 里的 `frame` 字段一致。
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Hello { .. } => "hello",
            Self::Ready { .. } => "ready",
            Self::TurnStart { .. } => "turn_start",
            Self::TurnEnd { .. } => "turn_end",
            Self::ToolCall { .. } => "tool_call",
            Self::ToolResult { .. } => "tool_result",
            Self::Log { .. } => "log",
            Self::Control { .. } => "control",
            Self::ControlResult { .. } => "control_result",
            Self::Ping => "ping",
            Self::Pong => "pong",
        }
    }
}

/// 协议版本是否兼容。现在只认完全相等。
pub fn version_compatible(version: u32) -> bool {
    version == PROTOCOL_VERSION
}

/// 编码成一帧（含行尾换行）。
pub fn encode(frame: &Frame) -> Result<String, ProtoError> {
    let mut line =
        serde_json::to_string(frame).map_err(|error| ProtoError::Encode(error.to_string()))?;
    line.push('\n');
    Ok(line)
}

/// 解开一行。容忍行尾的 `\n` / `\r`，其余一律严格。
pub fn decode(line: &str) -> Result<Frame, DecodeError> {
    let trimmed = line.trim_end_matches(['\n', '\r']);
    if trimmed.len() > MAX_FRAME_BYTES {
        return Err(DecodeError::TooLarge {
            size: trimmed.len(),
            max: MAX_FRAME_BYTES,
        });
    }
    let value: serde_json::Value =
        serde_json::from_str(trimmed).map_err(|error| DecodeError::NotJson {
            detail: error.to_string(),
        })?;
    let tag = value
        .get("frame")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| DecodeError::MissingField {
            detail: "缺少 frame 字段".into(),
        })?;

    match tag {
        "hello" | "ready" | "turn_start" | "turn_end" | "tool_call" | "tool_result" | "log"
        | "control" | "control_result" | "ping" | "pong" => {
            serde_json::from_value(value).map_err(|error| DecodeError::MissingField {
                detail: error.to_string(),
            })
        }
        other => Err(DecodeError::UnknownFrame {
            frame: other.to_owned(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::element::Element;
    use crate::record::{RecordKind, Sender};

    fn sample_delivery() -> Delivery {
        Delivery {
            ticket: Ticket::new("t-1").expect("合法"),
            session: "main".into(),
            room: RoomId::new("qq", "group/123").expect("合法"),
            records: vec![Record {
                id: 1,
                room: RoomId::new("qq", "group/123").expect("合法"),
                kind: RecordKind::Message,
                sender: Sender {
                    id: "10001".into(),
                    name: "老王".into(),
                    role: None,
                },
                at: 1_793_000_000_000,
                elements: vec![Element::text("在吗")],
                at_self: true,
                reply_to_self: false,
                platform_id: None,
                quote_ref: None,
                redelivery: None,
                note: None,
            }],
            reason: TurnReason::Wake,
            memories: Vec::new(),
            session_info: Some(SessionInfo {
                id: "main".into(),
                name: "main".into(),
                harness: "dsh".into(),
                model: None,
            }),
            more: None,
        }
    }

    fn samples() -> Vec<Frame> {
        vec![
            Frame::Hello {
                version: PROTOCOL_VERSION,
                harness: "dsh".into(),
                token: Some("secret".into()),
            },
            Frame::Ready {
                sessions: vec!["main".into(), "second".into()],
                maps: vec![SessionMapRef {
                    session: "main".into(),
                    harness_session: "session-acb-main".into(),
                }],
            },
            Frame::TurnStart {
                delivery: sample_delivery(),
            },
            Frame::TurnEnd {
                ticket: Ticket::new("t-1").expect("合法"),
                error: None,
            },
            Frame::ToolCall {
                id: "c-1".into(),
                ticket: Ticket::new("t-1").expect("合法"),
                tool: "send_message".into(),
                args: serde_json::json!({"elements": [{"type": "text", "text": "hi"}]}),
            },
            Frame::ToolResult {
                id: "c-1".into(),
                result: Some(serde_json::json!({"ok": true})),
                error: None,
            },
            Frame::ToolResult {
                id: "c-2".into(),
                result: None,
                error: Some(ToolError::new(
                    ErrorCode::RoomNotSubscribed,
                    "没有订阅这个房间",
                )),
            },
            Frame::Log {
                level: LogLevel::Warn,
                message: "上游拒绝".into(),
            },
            Frame::Control {
                id: "ctl-1".into(),
                session: "main".into(),
                command: "reset".into(),
                args: serde_json::json!({}),
            },
            Frame::ControlResult {
                id: "ctl-1".into(),
                session: "main".into(),
                command: "reset".into(),
                result: Some(serde_json::json!({"new_session": "session-abc"})),
                error: None,
            },
            Frame::Ping,
            Frame::Pong,
        ]
    }

    #[test]
    fn every_frame_round_trips_through_one_line() {
        for frame in samples() {
            let line = encode(&frame).expect("可编码");
            assert!(line.ends_with('\n'), "每帧一行");
            assert_eq!(line.matches('\n').count(), 1, "帧内不能有裸换行");
            let back = decode(&line).expect("可解码");
            assert_eq!(back, frame, "往返应当相等：{line}");
        }
    }

    #[test]
    fn frame_tag_matches_kind() {
        for frame in samples() {
            let line = encode(&frame).expect("可编码");
            let value: serde_json::Value = serde_json::from_str(line.trim()).expect("是 JSON");
            assert_eq!(value["frame"], frame.kind());
        }
    }

    #[test]
    fn clock_delivery_may_carry_no_records() {
        let mut delivery = sample_delivery();
        delivery.records.clear();
        delivery.reason = TurnReason::Clock;
        let line = encode(&Frame::TurnStart { delivery }).expect("可编码");
        let back = decode(&line).expect("可解码");
        match back {
            Frame::TurnStart { delivery } => {
                assert!(delivery.records.is_empty());
                assert_eq!(delivery.reason, TurnReason::Clock);
            }
            other => panic!("应当解成 turn_start，实际是 {}", other.kind()),
        }
    }

    #[test]
    fn decode_rejects_garbage() {
        assert!(matches!(
            decode("not json"),
            Err(DecodeError::NotJson { .. })
        ));
        assert!(matches!(
            decode(r#"{"no_frame":true}"#),
            Err(DecodeError::MissingField { .. })
        ));
        assert!(matches!(
            decode(r#"{"frame":"teleport"}"#),
            Err(DecodeError::UnknownFrame { .. })
        ));
    }

    #[test]
    fn decode_reports_oversized_frames() {
        let huge = "x".repeat(MAX_FRAME_BYTES + 1);
        assert!(matches!(decode(&huge), Err(DecodeError::TooLarge { .. })));
    }

    #[test]
    fn version_must_match_exactly() {
        assert!(version_compatible(PROTOCOL_VERSION));
        assert!(!version_compatible(PROTOCOL_VERSION + 1));
    }

    #[test]
    fn tool_result_error_is_optional_in_wire_form() {
        let frame = Frame::ToolResult {
            id: "c-1".into(),
            result: Some(serde_json::json!(1)),
            error: None,
        };
        let line = encode(&frame).expect("可编码");
        assert!(!line.contains("error"), "没有错误时不该出现 error 字段");
    }
}
