//! 桥的核心。
//!
//! 这一层只放跨平台、跨 harness 都成立的东西：订阅、唤醒、流水、水位、笔记、回合调度。
//! 平台语义在适配器里，harness 语义在接入点后面。

pub mod config;
pub mod endpoint;
pub mod engine;
pub mod memory;
pub mod outbound;
pub mod query;
pub mod relay;
pub mod render;
pub mod sanitize;
pub mod storage;
pub mod tools;

pub use config::{
    Config, Connection, ConsoleConfig, DEFAULT_MAX_AGE_DAYS, DEFAULT_MAX_RECORDS_PER_ROOM,
    DEFAULT_STORAGE_PATH, DEFAULT_WORKTREES_ROOT, EndpointConfig, Harness, KNOWN_ADAPTERS,
    KNOWN_MEMORY_KINDS, Memory, Persona, RetentionConfig, Session, StorageConfig, Subscription,
    Transport, WakeDecision, WakeInput, WakePolicy,
};
pub use endpoint::{Conn, ControlRegistry, EndpointError, handshake, run_connection, serve};
pub use relay::{FileRelay, RelayError};
pub use engine::{Bridge, BridgeError, Ingest, MAX_DELIVERY, Pruned};
pub use outbound::{
    CallError, MethodSpec, Outbound, OutboundError, Outbounds, ParamSpec, ParamType, Risk, Scope, Sent,
};
pub use storage::{
    Counts, Cursor, Deliveries, FEEDBACK_CAP, Feedback, FeedbackDraft, Feedbacks, MIN_KEPT,
    MemoryStore, Note, NoteKey, NoteKind, Notes, PrunePlan, RecordDraft, Recorded, SCHEMA_VERSION,
    SessionMap, SessionMaps, SqliteStore, StorageError, Store, TopicStatus, Transcript, Watermarks,
    unread,
};
pub use tools::TOOL_NAMES;

/// 协议类型顺手转出去：下游（接入点、适配器）依赖 kernel 一个 crate 就够。
pub use acb_proto::{
    Delivery, Element, ErrorCode, Frame, Record, RecordKind, RoomId, Sender, SenderRole, Ticket,
    ToolError, TurnReason,
};
