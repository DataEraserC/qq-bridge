//! SQLite 实现：把事情记在一个文件里，重启接着用。
//!
//! 只有一条规矩值得先说：**流水里存原话，不存旁白**。`redelivery` / `note` 是投递时
//! 才有的东西（见 `engine::open_turn`），不落库；重启后它们由投递计数重新算出来。
//!
//! 表结构见 `docs/07-storage.md`。改动表结构时把 `SCHEMA_VERSION` 加一，并且**先写迁移**
//! 再写代码——现在遇到不认识的版本会直接拒绝打开，而不是往老库里写坏数据。

use std::path::Path;
use std::time::Duration;

use qqab_proto::{Element, Record, RecordKind, RoomId, Sender};
use rusqlite::{Connection, OptionalExtension, params};

use super::{
    Cursor, Deliveries, MIN_KEPT, Note, NoteKey, NoteKind, Notes, PrunePlan, RecordDraft,
    StorageError, TopicStatus, Transcript, Watermarks,
};

/// 这个程序认识的库结构版本。改表就加一。
///
/// 1 → 2：`records` 加 `reply_to_self` 与 `platform_id`（引用回复的判定要用）。
pub const SCHEMA_VERSION: i64 = 2;

/// 建表语句。`IF NOT EXISTS`：新库建、老库不动。
const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS records (
    platform    TEXT    NOT NULL,
    room        TEXT    NOT NULL,
    record_id   INTEGER NOT NULL,
    kind        TEXT    NOT NULL,   -- message | event
    event       TEXT,               -- 仅 kind = event
    sender_id   TEXT    NOT NULL,
    sender_name TEXT    NOT NULL,
    at          INTEGER NOT NULL,   -- Unix 毫秒
    at_self     INTEGER NOT NULL,   -- 0 / 1
    reply_to_self INTEGER NOT NULL DEFAULT 0, -- 0 / 1，引用的是我发的消息
    platform_id TEXT,               -- 平台自己的消息编号，反查引用对象用
    elements    TEXT    NOT NULL,   -- 内容元素的 JSON 数组
    PRIMARY KEY (platform, room, record_id)
);

CREATE TABLE IF NOT EXISTS watermarks (
    device    TEXT    NOT NULL,
    platform  TEXT    NOT NULL,
    room      TEXT    NOT NULL,
    watermark INTEGER NOT NULL,
    PRIMARY KEY (device, platform, room)
);

-- 乱序确认：水位之上已经被确认、但前面还有缺口的那几条。
CREATE TABLE IF NOT EXISTS acks (
    device    TEXT    NOT NULL,
    platform  TEXT    NOT NULL,
    room      TEXT    NOT NULL,
    record_id INTEGER NOT NULL,
    PRIMARY KEY (device, platform, room, record_id)
);

CREATE TABLE IF NOT EXISTS deliveries (
    device    TEXT    NOT NULL,
    platform  TEXT    NOT NULL,
    room      TEXT    NOT NULL,
    record_id INTEGER NOT NULL,
    times     INTEGER NOT NULL,
    PRIMARY KEY (device, platform, room, record_id)
);

CREATE TABLE IF NOT EXISTS notes (
    platform   TEXT    NOT NULL,
    room       TEXT    NOT NULL,
    kind       TEXT    NOT NULL,   -- term | member | topic
    note_key   TEXT    NOT NULL,
    value      TEXT    NOT NULL,
    label      TEXT,
    status     TEXT,               -- open | closed，仅话题
    corrected  INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (platform, room, kind, note_key)
);
"#;

/// 库里的家底，启动时打给人看。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Counts {
    pub records: u64,
    pub rooms: u64,
    pub notes: u64,
}

/// SQLite 存储。
///
/// 一条连接、单进程使用。桥的所有访问都在那把互斥锁后面，所以不需要连接池；
/// 多进程同时开同一个文件不在支持范围内（WAL 也只保证读不被写挡住）。
pub struct SqliteStore {
    conn: Connection,
}

impl std::fmt::Debug for SqliteStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SqliteStore")
    }
}

impl SqliteStore {
    /// 打开（不存在就新建）。父目录也会顺手建出来。
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StorageError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|error| {
                    backend(format!("建目录 {} 失败：{error}", parent.display()))
                })?;
            }
        }
        let conn = Connection::open(path)
            .map_err(|error| backend(format!("打开 {} 失败：{error}", path.display())))?;
        Self::prepare(conn, true)
    }

    /// 全在内存里的 SQLite。测试用：跑的是真的 SQL，但不落盘。
    pub fn open_memory() -> Result<Self, StorageError> {
        let conn = Connection::open_in_memory().map_err(|error| backend(error.to_string()))?;
        Self::prepare(conn, false)
    }

    fn prepare(conn: Connection, on_disk: bool) -> Result<Self, StorageError> {
        conn.busy_timeout(Duration::from_secs(5))
            .map_err(|error| backend(error.to_string()))?;
        // WAL 让「读」不被「写」挡住；内存库没有意义，跳过。
        if on_disk {
            conn.execute_batch("PRAGMA journal_mode = WAL;")
                .map_err(|error| backend(format!("开 WAL 失败：{error}")))?;
        }
        conn.execute_batch("PRAGMA synchronous = NORMAL;")
            .map_err(|error| backend(error.to_string()))?;
        conn.execute_batch(SCHEMA)
            .map_err(|error| backend(format!("建表失败：{error}")))?;

        let store = Self { conn };
        store.check_version()?;
        Ok(store)
    }

    /// 把库升到 [`SCHEMA_VERSION`]。
    ///
    /// 规矩：**先补列再改版本号**，中途失败下次还能接着补。比当前版本新的库直接拒绝——
    /// 老程序往新库里写，就是在拿不认识的结构瞎猜。
    fn check_version(&self) -> Result<(), StorageError> {
        let found: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| backend(error.to_string()))?;

        // 新库（或被清过版本号的库）：按 0 处理，后面照常补列，不会漏。
        let version = match found {
            None => 0,
            Some(raw) => raw
                .trim()
                .parse::<i64>()
                .map_err(|_| backend(format!("库里的版本号读不懂：{raw}")))?,
        };

        if version > SCHEMA_VERSION {
            return Err(backend(format!(
                "这个库是第 {version} 版的结构，本程序只认识第 {SCHEMA_VERSION} 版。\
                 别拿新库配老程序——先升级程序，或者换一个库文件"
            )));
        }

        // 1 → 2：引用回复要判「被引用的是谁发的」，得留下平台消息编号。
        if version < 2 {
            self.add_column_if_missing("records", "reply_to_self", "INTEGER NOT NULL DEFAULT 0")?;
            self.add_column_if_missing("records", "platform_id", "TEXT")?;
        }

        self.conn
            .execute(
                "INSERT INTO meta (key, value) VALUES ('schema_version', ?1)
                 ON CONFLICT (key) DO UPDATE SET value = excluded.value",
                params![SCHEMA_VERSION.to_string()],
            )
            .map_err(|error| backend(error.to_string()))?;

        self.conn
            .execute_batch(
                "CREATE INDEX IF NOT EXISTS records_platform_id
                     ON records (platform, room, platform_id);",
            )
            .map_err(|error| backend(format!("建索引失败：{error}")))?;
        Ok(())
    }

    /// 列不存在才加。`ALTER TABLE ... ADD COLUMN` 没有 `IF NOT EXISTS`，只能先问一嘴。
    fn add_column_if_missing(
        &self,
        table: &str,
        column: &str,
        definition: &str,
    ) -> Result<(), StorageError> {
        let mut statement = self
            .conn
            .prepare(&format!("PRAGMA table_info({table})"))
            .map_err(|error| backend(error.to_string()))?;
        let names = statement
            .query_map([], |row| row.get::<_, String>(1))
            .map_err(|error| backend(error.to_string()))?;
        for name in names {
            if name.map_err(|error| backend(error.to_string()))? == column {
                return Ok(());
            }
        }
        self.conn
            .execute_batch(&format!(
                "ALTER TABLE {table} ADD COLUMN {column} {definition};"
            ))
            .map_err(|error| backend(format!("给 {table} 补列 {column} 失败：{error}")))
    }

    /// 库里有多少东西。只给启动摘要用，不算进 [`Store`] 约束。
    pub fn counts(&self) -> Result<Counts, StorageError> {
        let records = self.count("SELECT COUNT(*) FROM records")?;
        let rooms =
            self.count("SELECT COUNT(*) FROM (SELECT DISTINCT platform, room FROM records)")?;
        let notes = self.count("SELECT COUNT(*) FROM notes")?;
        Ok(Counts {
            records,
            rooms,
            notes,
        })
    }

    fn count(&self, sql: &str) -> Result<u64, StorageError> {
        let value: i64 = self
            .conn
            .query_row(sql, [], |row| row.get(0))
            .map_err(|error| backend(error.to_string()))?;
        Ok(value.max(0) as u64)
    }
}

fn backend(detail: impl Into<String>) -> StorageError {
    StorageError::Backend(detail.into())
}

fn io_error(error: rusqlite::Error) -> StorageError {
    backend(error.to_string())
}

impl Transcript for SqliteStore {
    fn append(&mut self, draft: RecordDraft) -> Result<Record, StorageError> {
        let (kind, event) = match &draft.kind {
            RecordKind::Message => ("message", None),
            RecordKind::Event { event } => ("event", Some(event.clone())),
        };
        let elements = serde_json::to_string(&draft.elements)
            .map_err(|error| backend(format!("内容元素存不进库：{error}")))?;

        let transaction = self.conn.transaction().map_err(io_error)?;
        // 编号是「房间内」的：同一个房间接着往上数，不同平台同号互不相干。
        let next: i64 = transaction
            .query_row(
                "SELECT COALESCE(MAX(record_id), 0) + 1 FROM records WHERE platform = ?1 AND room = ?2",
                params![draft.room.platform, draft.room.room],
                |row| row.get(0),
            )
            .map_err(io_error)?;

        transaction
            .execute(
                "INSERT INTO records
                 (platform, room, record_id, kind, event, sender_id, sender_name, at, at_self,
                  reply_to_self, platform_id, elements)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                params![
                    draft.room.platform,
                    draft.room.room,
                    next,
                    kind,
                    event,
                    draft.sender.id,
                    draft.sender.name,
                    draft.at,
                    if draft.at_self { 1 } else { 0 },
                    if draft.reply_to_self { 1 } else { 0 },
                    draft.platform_id,
                    elements,
                ],
            )
            .map_err(io_error)?;
        transaction.commit().map_err(io_error)?;

        Ok(Record {
            id: next.max(0) as u64,
            room: draft.room,
            kind: draft.kind,
            sender: draft.sender,
            at: draft.at,
            elements: draft.elements,
            at_self: draft.at_self,
            reply_to_self: draft.reply_to_self,
            platform_id: draft.platform_id,
            // 流水里只有原话；重投的旁白由投递时补（见 engine::open_turn）。
            redelivery: None,
            note: None,
        })
    }

    fn since(&self, room: &RoomId, after: u64, limit: usize) -> Result<Vec<Record>, StorageError> {
        let mut statement = self
            .conn
            .prepare(
                "SELECT record_id, kind, event, sender_id, sender_name, at, at_self,
                        reply_to_self, platform_id, elements
                 FROM records
                 WHERE platform = ?1 AND room = ?2 AND record_id > ?3
                 ORDER BY record_id
                 LIMIT ?4",
            )
            .map_err(io_error)?;

        let rows = statement
            .query_map(
                params![room.platform, room.room, after as i64, limit as i64],
                |row| {
                    Ok(RawRow {
                        id: row.get::<_, i64>(0)?.max(0) as u64,
                        kind: row.get(1)?,
                        event: row.get(2)?,
                        sender_id: row.get(3)?,
                        sender_name: row.get(4)?,
                        at: row.get(5)?,
                        at_self: row.get::<_, i64>(6)? != 0,
                        reply_to_self: row.get::<_, i64>(7)? != 0,
                        platform_id: row.get(8)?,
                        elements: row.get(9)?,
                    })
                },
            )
            .map_err(io_error)?;

        let mut records = Vec::new();
        for row in rows {
            let raw = row.map_err(io_error)?;
            records.push(raw.into_record(room)?);
        }
        Ok(records)
    }

    fn latest(&self, room: &RoomId) -> Result<Option<u64>, StorageError> {
        let max: Option<i64> = self
            .conn
            .query_row(
                "SELECT MAX(record_id) FROM records WHERE platform = ?1 AND room = ?2",
                params![room.platform, room.room],
                |row| row.get(0),
            )
            .map_err(io_error)?;
        Ok(max.map(|value| value.max(0) as u64))
    }

    fn rooms(&self) -> Result<Vec<RoomId>, StorageError> {
        let mut statement = self
            .conn
            .prepare("SELECT DISTINCT platform, room FROM records ORDER BY platform, room")
            .map_err(io_error)?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(io_error)?;

        let mut rooms = Vec::new();
        for row in rows {
            let (platform, room) = row.map_err(io_error)?;
            let room = RoomId::new(&platform, &room).map_err(|error| {
                backend(format!("库里的房间读不懂：{platform}:{room}（{error}）"))
            })?;
            rooms.push(room);
        }
        Ok(rooms)
    }

    fn count(&self, room: &RoomId) -> Result<u64, StorageError> {
        let value: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM records WHERE platform = ?1 AND room = ?2",
                params![room.platform, room.room],
                |row| row.get(0),
            )
            .map_err(io_error)?;
        Ok(value.max(0) as u64)
    }

    fn sender_of(&self, room: &RoomId, platform_id: &str) -> Result<Option<Sender>, StorageError> {
        if platform_id.is_empty() {
            return Ok(None);
        }
        self.conn
            .query_row(
                "SELECT sender_id, sender_name FROM records
                 WHERE platform = ?1 AND room = ?2 AND platform_id = ?3
                 ORDER BY record_id DESC
                 LIMIT 1",
                params![room.platform, room.room, platform_id],
                |row| {
                    Ok(Sender {
                        id: row.get(0)?,
                        name: row.get(1)?,
                    })
                },
            )
            .optional()
            .map_err(io_error)
    }

    fn prune(&mut self, room: &RoomId, plan: &PrunePlan) -> Result<u64, StorageError> {
        let keep = plan.keep_newest.max(MIN_KEPT) as i64;
        let transaction = self.conn.transaction().map_err(io_error)?;

        // 三个条件同时成立才删：早于时间线、低于编号安全线、排不进最新的 keep 条。
        // `LIMIT -1` 在 SQLite 里表示「不设上限」，所以 keep_newest 传 0 时会退化成一条都不删，
        // 但上面的 `max(MIN_KEPT)` 已经把它抬到 1 了。
        let deleted = transaction
            .execute(
                "DELETE FROM records
                 WHERE platform = ?1 AND room = ?2
                   AND record_id < ?3
                   AND (?4 IS NULL OR at < ?4)
                   AND record_id NOT IN (
                       SELECT record_id FROM records
                       WHERE platform = ?1 AND room = ?2
                       ORDER BY record_id DESC
                       LIMIT ?5
                   )",
                params![
                    room.platform,
                    room.room,
                    plan.below as i64,
                    plan.older_than,
                    keep
                ],
            )
            .map_err(io_error)?;

        if deleted > 0 {
            // 指向已删记录的账要一起清：投递计数没意义了；乱序确认也不可能再推进水位
            // （它前面那条已经不在了，前沿永远等不到）。
            for table in ["deliveries", "acks"] {
                transaction
                    .execute(
                        &format!(
                            "DELETE FROM {table}
                             WHERE platform = ?1 AND room = ?2
                               AND record_id NOT IN (
                                   SELECT record_id FROM records
                                   WHERE platform = ?1 AND room = ?2
                               )"
                        ),
                        params![room.platform, room.room],
                    )
                    .map_err(io_error)?;
            }
        }

        transaction.commit().map_err(io_error)?;
        Ok(deleted as u64)
    }
}

/// 从库里读出来的一行，还没变回 [`Record`]。
struct RawRow {
    id: u64,
    kind: String,
    event: Option<String>,
    sender_id: String,
    sender_name: String,
    at: i64,
    at_self: bool,
    reply_to_self: bool,
    platform_id: Option<String>,
    elements: String,
}

impl RawRow {
    fn into_record(self, room: &RoomId) -> Result<Record, StorageError> {
        let kind = match self.kind.as_str() {
            "message" => RecordKind::Message,
            "event" => RecordKind::Event {
                event: self.event.unwrap_or_default(),
            },
            other => return Err(backend(format!("库里有不认识的记录种类：{other}"))),
        };
        let elements: Vec<Element> = serde_json::from_str(&self.elements)
            .map_err(|error| backend(format!("内容元素读不出来：{error}")))?;

        Ok(Record {
            id: self.id,
            room: room.clone(),
            kind,
            sender: Sender {
                id: self.sender_id,
                name: self.sender_name,
            },
            at: self.at,
            elements,
            at_self: self.at_self,
            reply_to_self: self.reply_to_self,
            platform_id: self.platform_id,
            redelivery: None,
            note: None,
        })
    }
}

impl Watermarks for SqliteStore {
    fn watermark(&self, cursor: &Cursor) -> Result<u64, StorageError> {
        let value: Option<i64> = self
            .conn
            .query_row(
                "SELECT watermark FROM watermarks
                 WHERE device = ?1 AND platform = ?2 AND room = ?3",
                params![cursor.device, cursor.room.platform, cursor.room.room],
                |row| row.get(0),
            )
            .optional()
            .map_err(io_error)?;
        Ok(value.unwrap_or(0).max(0) as u64)
    }

    fn ack(&mut self, cursor: &Cursor, ids: &[u64]) -> Result<u64, StorageError> {
        let transaction = self.conn.transaction().map_err(io_error)?;
        let current: Option<i64> = transaction
            .query_row(
                "SELECT watermark FROM watermarks
                 WHERE device = ?1 AND platform = ?2 AND room = ?3",
                params![cursor.device, cursor.room.platform, cursor.room.room],
                |row| row.get(0),
            )
            .optional()
            .map_err(io_error)?;
        let current = current.unwrap_or(0).max(0) as u64;

        let latest: Option<i64> = transaction
            .query_row(
                "SELECT MAX(record_id) FROM records WHERE platform = ?1 AND room = ?2",
                params![cursor.room.platform, cursor.room.room],
                |row| row.get(0),
            )
            .map_err(io_error)?;
        let latest = latest.unwrap_or(0).max(0) as u64;

        // 先全验一遍再写：拿错编号就整批拒绝，别把一半写进去、留个半截状态。
        for &id in ids {
            if id > latest {
                return Err(StorageError::UnknownRecord {
                    room: cursor.room.to_string(),
                    id,
                });
            }
        }

        for &id in ids {
            if id > current {
                transaction
                    .execute(
                        "INSERT OR IGNORE INTO acks (device, platform, room, record_id)
                         VALUES (?1, ?2, ?3, ?4)",
                        params![
                            cursor.device,
                            cursor.room.platform,
                            cursor.room.room,
                            id as i64
                        ],
                    )
                    .map_err(io_error)?;
            }
        }

        // 水位只推连续前沿：中间缺一条就停在缺口前。
        let mut frontier = current;
        loop {
            let next = frontier + 1;
            let present: Option<i64> = transaction
                .query_row(
                    "SELECT record_id FROM acks
                     WHERE device = ?1 AND platform = ?2 AND room = ?3 AND record_id = ?4",
                    params![
                        cursor.device,
                        cursor.room.platform,
                        cursor.room.room,
                        next as i64
                    ],
                    |row| row.get(0),
                )
                .optional()
                .map_err(io_error)?;
            if present.is_none() {
                break;
            }
            transaction
                .execute(
                    "DELETE FROM acks
                     WHERE device = ?1 AND platform = ?2 AND room = ?3 AND record_id = ?4",
                    params![
                        cursor.device,
                        cursor.room.platform,
                        cursor.room.room,
                        next as i64
                    ],
                )
                .map_err(io_error)?;
            frontier = next;
        }

        transaction
            .execute(
                "INSERT INTO watermarks (device, platform, room, watermark)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (device, platform, room) DO UPDATE SET watermark = excluded.watermark",
                params![
                    cursor.device,
                    cursor.room.platform,
                    cursor.room.room,
                    frontier as i64
                ],
            )
            .map_err(io_error)?;
        transaction.commit().map_err(io_error)?;

        Ok(frontier)
    }
}

impl Notes for SqliteStore {
    fn note(&self, key: &NoteKey) -> Result<Option<Note>, StorageError> {
        let row = self
            .conn
            .query_row(
                "SELECT value, label, status, corrected, updated_at FROM notes
                 WHERE platform = ?1 AND room = ?2 AND kind = ?3 AND note_key = ?4",
                params![key.room.platform, key.room.room, key.kind.as_str(), key.key],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, i64>(3)? != 0,
                        row.get::<_, i64>(4)?,
                    ))
                },
            )
            .optional()
            .map_err(io_error)?;

        let Some((value, label, status, corrected, updated_at)) = row else {
            return Ok(None);
        };
        Ok(Some(Note {
            key: key.clone(),
            value,
            label,
            status: status.as_deref().and_then(parse_status),
            corrected,
            updated_at,
        }))
    }

    fn upsert_note(&mut self, note: Note) -> Result<(), StorageError> {
        self.conn
            .execute(
                "INSERT INTO notes
                 (platform, room, kind, note_key, value, label, status, corrected, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT (platform, room, kind, note_key) DO UPDATE SET
                     value = excluded.value,
                     label = excluded.label,
                     status = excluded.status,
                     corrected = excluded.corrected,
                     updated_at = excluded.updated_at",
                params![
                    note.key.room.platform,
                    note.key.room.room,
                    note.key.kind.as_str(),
                    note.key.key,
                    note.value,
                    note.label,
                    note.status.map(status_str),
                    if note.corrected { 1 } else { 0 },
                    note.updated_at,
                ],
            )
            .map_err(io_error)?;
        Ok(())
    }

    fn delete_note(&mut self, key: &NoteKey) -> Result<bool, StorageError> {
        let removed = self
            .conn
            .execute(
                "DELETE FROM notes WHERE platform = ?1 AND room = ?2 AND kind = ?3 AND note_key = ?4",
                params![
                    key.room.platform,
                    key.room.room,
                    key.kind.as_str(),
                    key.key
                ],
            )
            .map_err(io_error)?;
        Ok(removed > 0)
    }

    fn notes_in(&self, room: &RoomId) -> Result<Vec<Note>, StorageError> {
        let mut statement = self
            .conn
            .prepare(
                "SELECT kind, note_key, value, label, status, corrected, updated_at
                 FROM notes WHERE platform = ?1 AND room = ?2
                 ORDER BY kind, note_key",
            )
            .map_err(io_error)?;

        let rows = statement
            .query_map(params![room.platform, room.room], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, i64>(5)? != 0,
                    row.get::<_, i64>(6)?,
                ))
            })
            .map_err(io_error)?;

        let mut notes = Vec::new();
        for row in rows {
            let (kind, key, value, label, status, corrected, updated_at) = row.map_err(io_error)?;
            let Some(kind) = NoteKind::parse(&kind) else {
                return Err(backend(format!("库里有不认识的笔记种类：{kind}")));
            };
            notes.push(Note {
                key: NoteKey::new(room.clone(), kind, key),
                value,
                label,
                status: status.as_deref().and_then(parse_status),
                corrected,
                updated_at,
            });
        }
        Ok(notes)
    }
}

impl Deliveries for SqliteStore {
    fn count_delivery(&mut self, cursor: &Cursor, id: u64) -> Result<u32, StorageError> {
        self.conn
            .execute(
                "INSERT INTO deliveries (device, platform, room, record_id, times)
                 VALUES (?1, ?2, ?3, ?4, 1)
                 ON CONFLICT (device, platform, room, record_id)
                 DO UPDATE SET times = times + 1",
                params![
                    cursor.device,
                    cursor.room.platform,
                    cursor.room.room,
                    id as i64
                ],
            )
            .map_err(io_error)?;

        let times: i64 = self
            .conn
            .query_row(
                "SELECT times FROM deliveries
                 WHERE device = ?1 AND platform = ?2 AND room = ?3 AND record_id = ?4",
                params![
                    cursor.device,
                    cursor.room.platform,
                    cursor.room.room,
                    id as i64
                ],
                |row| row.get(0),
            )
            .map_err(io_error)?;
        Ok(times.max(1) as u32)
    }

    fn forget_delivery(&mut self, cursor: &Cursor, id: u64) -> Result<(), StorageError> {
        self.conn
            .execute(
                "DELETE FROM deliveries
                 WHERE device = ?1 AND platform = ?2 AND room = ?3 AND record_id = ?4",
                params![
                    cursor.device,
                    cursor.room.platform,
                    cursor.room.room,
                    id as i64
                ],
            )
            .map_err(io_error)?;
        Ok(())
    }
}

fn status_str(status: TopicStatus) -> &'static str {
    match status {
        TopicStatus::Open => "open",
        TopicStatus::Closed => "closed",
    }
}

fn parse_status(text: &str) -> Option<TopicStatus> {
    match text {
        "open" => Some(TopicStatus::Open),
        "closed" => Some(TopicStatus::Closed),
        _ => None,
    }
}
