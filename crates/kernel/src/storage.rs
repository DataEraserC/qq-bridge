//! 存储层：流水、水位、笔记。
//!
//! 三样东西的关系：
//! - **流水**是每个房间的完整记录，永久保存，投没投过都在；
//! - **水位**是「处理到哪条了」的游标，只经显式确认往前推（ADR-0010）；
//! - **未读**不是独立存储，是流水里水位之后的部分——算出来的，不是攒出来的。
//!
//! 笔记按房间隔离，主键带平台，跨平台不串。

use std::collections::{BTreeMap, BTreeSet, HashMap};

use qqab_proto::{Element, Record, RecordKind, RoomId, Sender};
use thiserror::Error;

/// 存储层错误。
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum StorageError {
    /// 后端自身出错（数据库打不开、写入失败等）。
    #[error("存储后端出错：{0}")]
    Backend(String),
    /// 确认了一条不存在的记录——调用方拿错了编号。
    #[error("房间 {room} 没有编号 {id} 的记录")]
    UnknownRecord { room: String, id: u64 },
}

/// 写进流水之前的一条记录：没有编号，编号由存储分配。
#[derive(Debug, Clone, PartialEq)]
pub struct RecordDraft {
    pub room: RoomId,
    pub kind: RecordKind,
    pub sender: Sender,
    pub at: i64,
    pub elements: Vec<Element>,
    pub at_self: bool,
}

impl RecordDraft {
    /// 一条聊天消息。
    pub fn message(
        room: RoomId,
        sender: Sender,
        at: i64,
        elements: Vec<Element>,
        at_self: bool,
    ) -> Self {
        Self {
            room,
            kind: RecordKind::Message,
            sender,
            at,
            elements,
            at_self,
        }
    }

    /// 一个平台事件（拍一拍就是 `event: "poke"`）。
    pub fn event(
        room: RoomId,
        sender: Sender,
        at: i64,
        event: impl Into<String>,
        elements: Vec<Element>,
        at_self: bool,
    ) -> Self {
        Self {
            room,
            kind: RecordKind::Event {
                event: event.into(),
            },
            sender,
            at,
            elements,
            at_self,
        }
    }
}

/// 流水：每个房间的完整记录。
pub trait Transcript {
    /// 追加一条，返回带编号的记录。
    fn append(&mut self, draft: RecordDraft) -> Result<Record, StorageError>;

    /// 取编号大于 `after` 的记录，最多 `limit` 条，按编号升序。
    fn since(&self, room: &RoomId, after: u64, limit: usize) -> Result<Vec<Record>, StorageError>;

    /// 房间里最大的编号；空房间给 `None`。
    fn latest(&self, room: &RoomId) -> Result<Option<u64>, StorageError>;
}

/// 水位：每个房间的「处理到哪条了」。
pub trait Watermarks {
    /// 当前水位。没有记录过就是 0（编号从 1 开始）。
    fn watermark(&self, room: &RoomId) -> Result<u64, StorageError>;

    /// 确认这批编号已处理。只前进、不后退；只推进**连续**的前沿——中间缺一条就停在缺口前。
    fn ack(&mut self, room: &RoomId, ids: &[u64]) -> Result<u64, StorageError>;
}

/// 未经确认的未读：水位之后的流水。
pub fn unread<S>(store: &S, room: &RoomId, limit: usize) -> Result<Vec<Record>, StorageError>
where
    S: Transcript + Watermarks,
{
    let watermark = store.watermark(room)?;
    store.since(room, watermark, limit)
}

/// 笔记的种类。只有话题带状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum NoteKind {
    Term,
    Member,
    Topic,
}

impl NoteKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Term => "term",
            Self::Member => "member",
            Self::Topic => "topic",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "term" => Some(Self::Term),
            "member" => Some(Self::Member),
            "topic" => Some(Self::Topic),
            _ => None,
        }
    }
}

/// 话题状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TopicStatus {
    Open,
    Closed,
}

/// 笔记的主键：房间 + 种类 + 键。房间带平台，所以跨平台不串。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NoteKey {
    pub room: RoomId,
    pub kind: NoteKind,
    pub key: String,
}

impl NoteKey {
    pub fn new(room: RoomId, kind: NoteKind, key: impl Into<String>) -> Self {
        Self {
            room,
            kind,
            key: key.into(),
        }
    }
}

/// 一条笔记。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    pub key: NoteKey,
    pub value: String,
    pub label: Option<String>,
    pub status: Option<TopicStatus>,
    /// 是否由一次纠正写入。纠正过的最后才淘汰。
    pub corrected: bool,
    pub updated_at: i64,
}

/// 笔记读写。
pub trait Notes {
    fn note(&self, key: &NoteKey) -> Result<Option<Note>, StorageError>;
    fn upsert_note(&mut self, note: Note) -> Result<(), StorageError>;
    fn delete_note(&mut self, key: &NoteKey) -> Result<bool, StorageError>;
    /// 某个房间的全部笔记，按键排序。
    fn notes_in(&self, room: &RoomId) -> Result<Vec<Note>, StorageError>;
}

/// 内存实现。测试与开发用；正式存储（SQLite）按同一组 trait 实现。
#[derive(Debug, Default)]
pub struct MemoryStore {
    next_id: HashMap<RoomId, u64>,
    records: HashMap<RoomId, Vec<Record>>,
    watermark: HashMap<RoomId, u64>,
    acked: HashMap<RoomId, BTreeSet<u64>>,
    notes: BTreeMap<NoteKey, Note>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Transcript for MemoryStore {
    fn append(&mut self, draft: RecordDraft) -> Result<Record, StorageError> {
        let next = self.next_id.get(&draft.room).copied().unwrap_or(0) + 1;
        let record = Record {
            id: next,
            room: draft.room.clone(),
            kind: draft.kind,
            sender: draft.sender,
            at: draft.at,
            elements: draft.elements,
            at_self: draft.at_self,
            redelivery: None,
        };
        self.next_id.insert(draft.room.clone(), next);
        self.records
            .entry(draft.room)
            .or_default()
            .push(record.clone());
        Ok(record)
    }

    fn since(&self, room: &RoomId, after: u64, limit: usize) -> Result<Vec<Record>, StorageError> {
        let Some(records) = self.records.get(room) else {
            return Ok(Vec::new());
        };
        Ok(records
            .iter()
            .filter(|record| record.id > after)
            .take(limit)
            .cloned()
            .collect())
    }

    fn latest(&self, room: &RoomId) -> Result<Option<u64>, StorageError> {
        Ok(self.next_id.get(room).copied())
    }
}

impl Watermarks for MemoryStore {
    fn watermark(&self, room: &RoomId) -> Result<u64, StorageError> {
        Ok(self.watermark.get(room).copied().unwrap_or(0))
    }

    fn ack(&mut self, room: &RoomId, ids: &[u64]) -> Result<u64, StorageError> {
        let latest = self.next_id.get(room).copied().unwrap_or(0);
        let current = self.watermark.get(room).copied().unwrap_or(0);

        for &id in ids {
            if id > latest {
                return Err(StorageError::UnknownRecord {
                    room: room.to_string(),
                    id,
                });
            }
            if id > current {
                self.acked.entry(room.clone()).or_default().insert(id);
            }
        }

        let acked = self.acked.entry(room.clone()).or_default();
        let mut frontier = current;
        while acked.remove(&(frontier + 1)) {
            frontier += 1;
        }
        self.watermark.insert(room.clone(), frontier);
        Ok(frontier)
    }
}

impl Notes for MemoryStore {
    fn note(&self, key: &NoteKey) -> Result<Option<Note>, StorageError> {
        Ok(self.notes.get(key).cloned())
    }

    fn upsert_note(&mut self, note: Note) -> Result<(), StorageError> {
        self.notes.insert(note.key.clone(), note);
        Ok(())
    }

    fn delete_note(&mut self, key: &NoteKey) -> Result<bool, StorageError> {
        Ok(self.notes.remove(key).is_some())
    }

    fn notes_in(&self, room: &RoomId) -> Result<Vec<Note>, StorageError> {
        Ok(self
            .notes
            .iter()
            .filter(|(key, _)| &key.room == room)
            .map(|(_, note)| note.clone())
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use qqab_proto::Element;

    fn room(platform: &str, id: &str) -> RoomId {
        RoomId::new(platform, id).expect("合法")
    }

    fn draft(room: RoomId, text: &str) -> RecordDraft {
        RecordDraft::message(
            room,
            Sender {
                id: "10001".into(),
                name: "老王".into(),
            },
            1_793_000_000_000,
            vec![Element::text(text)],
            false,
        )
    }

    #[test]
    fn ids_are_per_room_and_start_at_one() {
        let mut store = MemoryStore::new();
        let qq = room("qq", "123");
        let discord = room("discord", "123");

        let first = store.append(draft(qq.clone(), "a")).expect("可写");
        let second = store.append(draft(qq.clone(), "b")).expect("可写");
        let other = store.append(draft(discord.clone(), "c")).expect("可写");

        assert_eq!(first.id, 1);
        assert_eq!(second.id, 2);
        assert_eq!(other.id, 1, "不同房间各自编号");
    }

    #[test]
    fn unread_is_everything_after_the_watermark() {
        let mut store = MemoryStore::new();
        let qq = room("qq", "1");
        store.append(draft(qq.clone(), "a")).expect("可写");
        store.append(draft(qq.clone(), "b")).expect("可写");
        store.append(draft(qq.clone(), "c")).expect("可写");

        assert_eq!(unread(&store, &qq, 100).expect("可读").len(), 3);
        store.ack(&qq, &[1]).expect("可确认");
        assert_eq!(unread(&store, &qq, 100).expect("可读").len(), 2);
    }

    #[test]
    fn watermark_only_moves_over_a_contiguous_frontier() {
        let mut store = MemoryStore::new();
        let qq = room("qq", "1");
        for text in ["a", "b", "c"] {
            store.append(draft(qq.clone(), text)).expect("可写");
        }

        let frontier = store.ack(&qq, &[1, 3]).expect("可确认");
        assert_eq!(frontier, 1, "缺了 2，只能推到 1");

        let frontier = store.ack(&qq, &[2]).expect("可确认");
        assert_eq!(frontier, 3, "补上 2 之后一次推到 3");
    }

    #[test]
    fn watermark_never_moves_backwards() {
        let mut store = MemoryStore::new();
        let qq = room("qq", "1");
        store.append(draft(qq.clone(), "a")).expect("可写");
        store.append(draft(qq.clone(), "b")).expect("可写");

        assert_eq!(store.ack(&qq, &[1, 2]).expect("可确认"), 2);
        assert_eq!(store.ack(&qq, &[1]).expect("可确认"), 2, "旧编号不影响");
    }

    #[test]
    fn acking_a_missing_record_is_an_error() {
        let mut store = MemoryStore::new();
        let qq = room("qq", "1");
        store.append(draft(qq.clone(), "a")).expect("可写");

        let error = store.ack(&qq, &[9]).expect_err("应当报错");
        assert!(matches!(error, StorageError::UnknownRecord { id: 9, .. }));
    }

    #[test]
    fn rooms_on_different_platforms_never_share_watermarks() {
        let mut store = MemoryStore::new();
        let qq = room("qq", "123");
        let discord = room("discord", "123");

        store.append(draft(qq.clone(), "a")).expect("可写");
        store.append(draft(discord.clone(), "b")).expect("可写");
        store.ack(&qq, &[1]).expect("可确认");

        assert_eq!(store.watermark(&qq).expect("可读"), 1);
        assert_eq!(
            store.watermark(&discord).expect("可读"),
            0,
            "另一个平台的 123 不能被带着走"
        );
    }

    #[test]
    fn notes_are_isolated_per_room_and_platform() {
        let mut store = MemoryStore::new();
        let qq = room("qq", "123");
        let discord = room("discord", "123");

        let qq_key = NoteKey::new(qq.clone(), NoteKind::Term, "部署");
        store
            .upsert_note(Note {
                key: qq_key.clone(),
                value: "蓝绿发布".into(),
                label: None,
                status: None,
                corrected: false,
                updated_at: 1,
            })
            .expect("可写");

        assert!(store.note(&qq_key).expect("可读").is_some());
        assert!(
            store
                .note(&NoteKey::new(discord.clone(), NoteKind::Term, "部署"))
                .expect("可读")
                .is_none(),
            "同名同号但不同平台，不能串"
        );
        assert_eq!(store.notes_in(&qq).expect("可读").len(), 1);
        assert!(store.notes_in(&discord).expect("可读").is_empty());
    }

    #[test]
    fn events_and_messages_share_the_same_stream() {
        let mut store = MemoryStore::new();
        let qq = room("qq", "1");

        store.append(draft(qq.clone(), "在吗")).expect("可写");
        store
            .append(RecordDraft::event(
                qq.clone(),
                Sender {
                    id: "10001".into(),
                    name: "老王".into(),
                },
                1_793_000_000_001,
                "poke",
                vec![Element::text("[拍了拍我]")],
                true,
            ))
            .expect("可写");

        let all = unread(&store, &qq, 100).expect("可读");
        assert_eq!(all.len(), 2, "拍一拍和平常消息在同一条流水里");
        assert!(all[1].is_event("poke"));
        assert!(all[1].at_self);
    }
}
