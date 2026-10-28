//! 桥的核心。
//!
//! 这一层只放跨平台、跨 harness 都成立的东西：订阅、唤醒、流水、水位、笔记、回合调度。
//! 平台语义在适配器里，harness 语义在接入点后面。

pub mod config;
pub mod engine;
pub mod outbound;
pub mod storage;
pub mod tools;

pub use config::{Config, Device, Subscription, WakeDecision, WakeInput, WakePolicy};
pub use engine::{Bridge, BridgeError, Ingest, MAX_DELIVERY};
pub use outbound::{Outbound, OutboundError, Sent};
pub use storage::{
    Cursor, MemoryStore, Note, NoteKey, NoteKind, Notes, RecordDraft, StorageError, Store,
    TopicStatus, Transcript, Watermarks, unread,
};
pub use tools::TOOL_NAMES;

/// 协议类型顺手转出去：下游（接入点、适配器）依赖 kernel 一个 crate 就够。
pub use qqab_proto::{
    Delivery, Element, ErrorCode, Frame, Record, RecordKind, RoomId, Sender, Ticket, ToolError,
    TurnReason,
};
