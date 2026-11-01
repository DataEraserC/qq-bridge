//! 桥与 harness 之间的协议帧。
//!
//! 这一层只回答三个问题：投递什么、允许调用什么、返回什么。桥不关心 harness 内部，
//! harness 也不关心平台细节。
//!
//! 传输形态见 `docs/04-protocol.md`：一条长连接，每帧一行 JSON。

pub mod element;
pub mod error;
pub mod frame;
pub mod record;
pub mod room;
pub mod ticket;

pub use element::{Element, parse_elements};
pub use error::{DecodeError, ProtoError};
pub use frame::{
    Delivery, ErrorCode, Frame, LogLevel, MemoryInfo, Omitted, PersonaInfo, SessionInfo,
    SessionMapRef, ToolError, TurnReason,
};
pub use record::{QuoteRef, Record, RecordKind, Sender, SenderRole};
pub use room::RoomId;
pub use ticket::Ticket;
