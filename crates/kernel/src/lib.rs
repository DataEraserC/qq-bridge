//! 桥的核心。
//!
//! 这一层只放跨平台、跨 harness 都成立的东西：订阅、唤醒、流水、水位、笔记。
//! 平台语义在适配器里，harness 语义在接入点后面。

pub mod config;
pub mod storage;

pub use config::{Config, Device, Subscription, WakeDecision, WakeInput, WakePolicy};
pub use storage::{
    MemoryStore, Note, NoteKey, NoteKind, Notes, RecordDraft, StorageError, Transcript, Watermarks,
    unread,
};
