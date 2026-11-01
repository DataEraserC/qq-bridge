//! SQLite 实现：把事情记在一个文件里，重启接着用。
//!
//! 只有一条规矩值得先说：**流水里存原话，不存旁白**。`redelivery` / `note` 是投递时
//! 才有的东西（见 `engine::open_turn`），不落库；重启后它们由投递计数重新算出来。
//!
//! 表结构见 `docs/07-storage.md`。改动表结构时把 `SCHEMA_VERSION` 加一，并且**先写迁移**
//! 再写代码——现在遇到不认识的版本会直接拒绝打开，而不是往老库里写坏数据。

use std::path::Path;
use std::time::Duration;

use acb_proto::{Element, Record, RecordKind, RoomId, Sender, SenderRole};
use rusqlite::{Connection, OptionalExtension, params};

use super::{
    Cursor, Deliveries, FEEDBACK_CAP, Feedback, FeedbackDraft, Feedbacks, MIN_KEPT, Note, NoteKey,
    NoteKind, Notes, PrunePlan, RecordDraft, Recorded, SessionMap, SessionMaps, StorageError,
    TopicStatus, Transcript, Watermarks,
};

/// 这个程序认识的库结构版本。改表就加一。
///
/// 1 → 2：`records` 加 `reply_to_self` 与 `platform_id`（引用回复的判定要用）。
/// 2 → 3：房间从「平台 + 房间号」改成「连接 + 路径」——`platform` 列改叫 `connection`，
///         `room` 列改叫 `room_key`，并留下 `room_json` 存原始路径结构。
/// 3 → 4：device 概念升级为「harness 会话」——水位/投递计数/乱序确认三张表的 `device`
///         列改叫 `session`（ADR-0017）。
/// 4 → 5：加 `session_maps` 表——acb 会话 ↔ harness 内部会话的映射，桥侧控制台落盘
///         （ADR-0018）。
/// 5 → 6：`records` 加 `sender_role` 列——发送者身份（系统管理员/群主/群管理员），
///         适配器判得出来才填，判不出留空。
/// 6 → 7：加 `feedback` 表——agent 的反馈通道（ADR-0021）。新表由建表语句建，不用搬数据。
pub const SCHEMA_VERSION: i64 = 7;

/// 建表语句。`IF NOT EXISTS`：新库建、老库不动。
const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS records (
    connection  TEXT    NOT NULL,   -- 连接名（哪个号在看这个房间）
    room_key    TEXT    NOT NULL,   -- 房间路径，如 group/158296748
    room_json   TEXT,               -- 路径的原始结构（JSON 数组），留着给以后换形状用
    record_id   INTEGER NOT NULL,
    kind        TEXT    NOT NULL,   -- message | event
    event       TEXT,               -- 仅 kind = event
    sender_id   TEXT    NOT NULL,
    sender_name TEXT    NOT NULL,
    sender_role TEXT,               -- 身份：system_admin | owner | admin，判不出留空
    at          INTEGER NOT NULL,   -- Unix 毫秒
    at_self     INTEGER NOT NULL,   -- 0 / 1
    reply_to_self INTEGER NOT NULL DEFAULT 0, -- 0 / 1，引用的是我发的消息
    platform_id TEXT,               -- 平台自己的消息编号，反查引用对象用
    elements    TEXT    NOT NULL,   -- 内容元素的 JSON 数组
    PRIMARY KEY (connection, room_key, record_id)
);


CREATE TABLE IF NOT EXISTS watermarks (
    session    TEXT    NOT NULL,
    connection TEXT    NOT NULL,
    room_key   TEXT    NOT NULL,
    watermark  INTEGER NOT NULL,
    PRIMARY KEY (session, connection, room_key)
);

-- 乱序确认：水位之上已经被确认、但前面还有缺口的那几条。
CREATE TABLE IF NOT EXISTS acks (
    session    TEXT    NOT NULL,
    connection TEXT    NOT NULL,
    room_key   TEXT    NOT NULL,
    record_id  INTEGER NOT NULL,
    PRIMARY KEY (session, connection, room_key, record_id)
);

CREATE TABLE IF NOT EXISTS deliveries (
    session    TEXT    NOT NULL,
    connection TEXT    NOT NULL,
    room_key   TEXT    NOT NULL,
    record_id  INTEGER NOT NULL,
    times      INTEGER NOT NULL,
    PRIMARY KEY (session, connection, room_key, record_id)
);

CREATE TABLE IF NOT EXISTS notes (
    connection TEXT    NOT NULL,
    room_key   TEXT    NOT NULL,
    kind       TEXT    NOT NULL,   -- term | member | topic
    note_key   TEXT    NOT NULL,
    value      TEXT    NOT NULL,
    label      TEXT,
    status     TEXT,               -- open | closed，仅话题
    corrected  INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (connection, room_key, kind, note_key)
);

CREATE TABLE IF NOT EXISTS session_maps (
    session         TEXT    NOT NULL,   -- acb 会话 id
    harness_session TEXT    NOT NULL,   -- harness 内部会话 id（dsh 的 sessionId）
    meta            TEXT    NOT NULL,   -- 元信息 JSON（标题/模型等，harness 回报什么存什么）
    updated_at      INTEGER NOT NULL,
    PRIMARY KEY (session)
);

-- agent 的反馈（ADR-0021）。指纹相同的合并成一条，次数累加。
CREATE TABLE IF NOT EXISTS feedback (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    fingerprint TEXT    NOT NULL UNIQUE,  -- 去重指纹（类型 + 标题归一化）
    title       TEXT    NOT NULL,         -- 正文第一行，列表里扫这个
    body        TEXT    NOT NULL,         -- 完整正文
    suggestion  TEXT,                     -- 建议怎么改
    kind        TEXT    NOT NULL,         -- bug | gap | friction | idea
    severity    TEXT    NOT NULL,         -- blocker | major | minor
    refs        TEXT    NOT NULL,         -- JSON 数组：水位编号、工具名、错误码
    occurrences INTEGER NOT NULL,         -- agent 自称发生了多少次
    repeats     INTEGER NOT NULL,         -- 被报告了几次
    tool        TEXT,                     -- 出事时在调哪个桥工具
    session     TEXT    NOT NULL,
    harness     TEXT,
    room        TEXT,
    context     TEXT    NOT NULL,         -- 现场 JSON：票、模型、时间
    status      TEXT    NOT NULL,         -- open | acked | resolved | wontfix
    note        TEXT,
    first_at    INTEGER NOT NULL,
    last_at     INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS feedback_by_status ON feedback (status, last_at DESC);
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
        // 索引必须等结构补齐之后再建：第 1 版的库里还没有 `connection` / `platform_id`
        // 这两列，先建索引会直接报「没有这一列」。
        store.ensure_indexes()?;
        Ok(store)
    }

    /// 补索引。`IF NOT EXISTS` 所以每次打开都跑一遍也没成本；老库打开时自己就有。
    fn ensure_indexes(&self) -> Result<(), StorageError> {
        self.conn
            .execute_batch(
                // 引用对齐要按平台编号反查（ADR-0024）。没这个索引就得扫完整个房间。
                "CREATE INDEX IF NOT EXISTS records_platform
                     ON records (connection, room_key, platform_id);",
            )
            .map_err(|error| backend(format!("建索引失败：{error}")))
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

        // 2 → 3：房间从「平台 + 房间号」改成「连接 + 路径」。
        //
        // 只改名、不搬数据：老库里 `platform` 那一列本来就是连接名（以前叫平台名），
        // `room` 那一列本来就是路径（以前叫房间号），语义没变，只是名字对上了。
        // `room_json` 是新留的后路——以后房间要换成更自由的形状，原始结构还在这儿。
        if version < 3 {
            for table in ["records", "watermarks", "acks", "deliveries", "notes"] {
                self.rename_column_if_needed(table, "platform", "connection")?;
                self.rename_column_if_needed(table, "room", "room_key")?;
            }
            self.add_column_if_missing("records", "room_json", "TEXT")?;
            // 第 1 版留下了 `records_platform_id`，改名之后它跟下面要建的索引完全重复。
            // 留着只是白占写入开销，删掉。
            self.conn
                .execute_batch("DROP INDEX IF EXISTS records_platform_id;")
                .map_err(|error| backend(format!("清理旧索引失败：{error}")))?;
        }

        // 3 → 4：device 概念升级为「harness 会话」。只改名不搬数据——`device` 那一列
        // 本来就是「谁的水位/谁的重投计数」，语义没变，只是名字从设备换成会话。
        if version < 4 {
            for table in ["watermarks", "acks", "deliveries"] {
                self.rename_column_if_needed(table, "device", "session")?;
            }
        }

        // 5 → 6：发送者身份。可空，老记录判不出就留空，不必回填。
        if version < 6 {
            self.add_column_if_missing("records", "sender_role", "TEXT")?;
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
                "CREATE INDEX IF NOT EXISTS records_by_platform_id
                     ON records (connection, room_key, platform_id);",
            )
            .map_err(|error| backend(format!("建索引失败：{error}")))?;
        Ok(())
    }

    /// 老列在才改名。新库建表时用的已经是新名字，这里就该什么都不做。
    fn rename_column_if_needed(
        &self,
        table: &str,
        old: &str,
        new: &str,
    ) -> Result<(), StorageError> {
        let names = self.columns_of(table)?;
        if !names.iter().any(|name| name == old) {
            return Ok(());
        }
        self.conn
            .execute_batch(&format!(
                "ALTER TABLE {table} RENAME COLUMN {old} TO {new};"
            ))
            .map_err(|error| backend(format!("把 {table}.{old} 改名成 {new} 失败：{error}")))
    }

    /// 表里有哪些列。
    fn columns_of(&self, table: &str) -> Result<Vec<String>, StorageError> {
        let mut statement = self
            .conn
            .prepare(&format!("PRAGMA table_info({table})"))
            .map_err(|error| backend(error.to_string()))?;
        let names = statement
            .query_map([], |row| row.get::<_, String>(1))
            .map_err(|error| backend(error.to_string()))?;
        let mut found = Vec::new();
        for name in names {
            found.push(name.map_err(|error| backend(error.to_string()))?);
        }
        Ok(found)
    }

    /// 列不存在才加。`ALTER TABLE ... ADD COLUMN` 没有 `IF NOT EXISTS`，只能先问一嘴。
    fn add_column_if_missing(
        &self,
        table: &str,
        column: &str,
        definition: &str,
    ) -> Result<(), StorageError> {
        let names = self.columns_of(table)?;
        if names.iter().any(|name| name == column) {
            return Ok(());
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
            self.count("SELECT COUNT(*) FROM (SELECT DISTINCT connection, room_key FROM records)")?;
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

        // 路径的原始结构也存一份：以后房间要换成更自由的形状，原始数据还在，
        // 重新算键是一次脚本的事，不用重导。
        let room_json = serde_json::to_string(&draft.room.path)
            .map_err(|error| backend(format!("房间路径存不进库：{error}")))?;

        let transaction = self.conn.transaction().map_err(io_error)?;
        // 编号是「房间内」的：同一个房间接着往上数，不同连接同号互不相干。
        let next: i64 = transaction
            .query_row(
                "SELECT COALESCE(MAX(record_id), 0) + 1 FROM records WHERE connection = ?1 AND room_key = ?2",
                params![draft.room.connection, draft.room.key()],
                |row| row.get(0),
            )
            .map_err(io_error)?;

        transaction
            .execute(
                "INSERT INTO records
                 (connection, room_key, room_json, record_id, kind, event, sender_id, sender_name,
                  sender_role, at, at_self, reply_to_self, platform_id, elements)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
                params![
                    draft.room.connection,
                    draft.room.key(),
                    room_json,
                    next,
                    kind,
                    event,
                    draft.sender.id,
                    draft.sender.name,
                    draft.sender.role.map(|role| role_str(&role)),
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
            quote_ref: None,
            // 流水里只有原话；重投的旁白由投递时补（见 engine::open_turn）。
            redelivery: None,
            note: None,
        })
    }

    fn since(&self, room: &RoomId, after: u64, limit: usize) -> Result<Vec<Record>, StorageError> {
        let mut statement = self
            .conn
            .prepare(
                "SELECT record_id, kind, event, sender_id, sender_name, sender_role, at, at_self,
                        reply_to_self, platform_id, elements
                 FROM records
                 WHERE connection = ?1 AND room_key = ?2 AND record_id > ?3
                 ORDER BY record_id
                 LIMIT ?4",
            )
            .map_err(io_error)?;

        let rows = statement
            .query_map(
                params![room.connection, room.key(), after as i64, limit as i64],
                |row| {
                    Ok(RawRow {
                        id: row.get::<_, i64>(0)?.max(0) as u64,
                        kind: row.get(1)?,
                        event: row.get(2)?,
                        sender_id: row.get(3)?,
                        sender_name: row.get(4)?,
                        sender_role: row.get(5)?,
                        at: row.get(6)?,
                        at_self: row.get::<_, i64>(7)? != 0,
                        reply_to_self: row.get::<_, i64>(8)? != 0,
                        platform_id: row.get(9)?,
                        elements: row.get(10)?,
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
                "SELECT MAX(record_id) FROM records WHERE connection = ?1 AND room_key = ?2",
                params![room.connection, room.key()],
                |row| row.get(0),
            )
            .map_err(io_error)?;
        Ok(max.map(|value| value.max(0) as u64))
    }

    fn rooms(&self) -> Result<Vec<RoomId>, StorageError> {
        let mut statement = self
            .conn
            .prepare(
                "SELECT DISTINCT connection, room_key FROM records ORDER BY connection, room_key",
            )
            .map_err(io_error)?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(io_error)?;

        let mut rooms = Vec::new();
        for row in rows {
            let (connection, room_key) = row.map_err(io_error)?;
            let room = RoomId::new(&connection, &room_key).map_err(|error| {
                backend(format!(
                    "库里的房间读不懂：{connection}:{room_key}（{error}）"
                ))
            })?;
            rooms.push(room);
        }
        Ok(rooms)
    }

    fn count(&self, room: &RoomId) -> Result<u64, StorageError> {
        let value: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM records WHERE connection = ?1 AND room_key = ?2",
                params![room.connection, room.key()],
                |row| row.get(0),
            )
            .map_err(io_error)?;
        Ok(value.max(0) as u64)
    }

    fn located(&self, room: &RoomId, platform_id: &str) -> Result<Option<Record>, StorageError> {
        if platform_id.is_empty() {
            return Ok(None);
        }
        let row = self
            .conn
            .query_row(
                // 强制走 platform 索引：不指定的话 SQLite 会选主键索引（省掉排序），
                // 于是引用一条很老的消息时要一条条往回扫。这里相等条件命中的通常只有
                // 一行，多一次微小的排序换「不扫全房间」是划算的。
                "SELECT record_id, kind, event, sender_id, sender_name, sender_role, at, at_self,
                        reply_to_self, platform_id, elements
                 FROM records INDEXED BY records_platform
                 WHERE connection = ?1 AND room_key = ?2 AND platform_id = ?3
                 ORDER BY record_id DESC
                 LIMIT 1",
                params![room.connection, room.key(), platform_id],
                |row| {
                    Ok(RawRow {
                        id: row.get::<_, i64>(0)?.max(0) as u64,
                        kind: row.get(1)?,
                        event: row.get(2)?,
                        sender_id: row.get(3)?,
                        sender_name: row.get(4)?,
                        sender_role: row.get(5)?,
                        at: row.get(6)?,
                        at_self: row.get::<_, i64>(7)? != 0,
                        reply_to_self: row.get::<_, i64>(8)? != 0,
                        platform_id: row.get(9)?,
                        elements: row.get(10)?,
                    })
                },
            )
            .optional()
            .map_err(io_error)?;

        match row {
            Some(raw) => Ok(Some(raw.into_record(room)?)),
            None => Ok(None),
        }
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
                 WHERE connection = ?1 AND room_key = ?2
                   AND record_id < ?3
                   AND (?4 IS NULL OR at < ?4)
                   AND record_id NOT IN (
                       SELECT record_id FROM records
                       WHERE connection = ?1 AND room_key = ?2
                       ORDER BY record_id DESC
                       LIMIT ?5
                   )",
                params![
                    room.connection,
                    room.key(),
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
                             WHERE connection = ?1 AND room_key = ?2
                               AND record_id NOT IN (
                                   SELECT record_id FROM records
                                   WHERE connection = ?1 AND room_key = ?2
                               )"
                        ),
                        params![room.connection, room.key()],
                    )
                    .map_err(io_error)?;
            }
        }

        transaction.commit().map_err(io_error)?;
        Ok(deleted as u64)
    }
}

/// 身份 → 库里的字符串。跟 [`SenderRole`] 的 serde 表示保持一致。
fn role_str(role: &SenderRole) -> &'static str {
    match role {
        SenderRole::SystemAdmin => "system_admin",
        SenderRole::Owner => "owner",
        SenderRole::Admin => "admin",
    }
}

/// 库里的字符串 → 身份。不认识的就当没判出来（宁可少标，别瞎标）。
fn role_from_str(text: &str) -> Option<SenderRole> {
    match text {
        "system_admin" => Some(SenderRole::SystemAdmin),
        "owner" => Some(SenderRole::Owner),
        "admin" => Some(SenderRole::Admin),
        _ => None,
    }
}

/// 从库里读出来的一行，还没变回 [`Record`]。
struct RawRow {
    id: u64,
    kind: String,
    event: Option<String>,
    sender_id: String,
    sender_name: String,
    sender_role: Option<String>,
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
                role: self.sender_role.as_deref().and_then(role_from_str),
            },
            at: self.at,
            elements,
            at_self: self.at_self,
            reply_to_self: self.reply_to_self,
            platform_id: self.platform_id,
            quote_ref: None,
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
                 WHERE session = ?1 AND connection = ?2 AND room_key = ?3",
                params![cursor.session, cursor.room.connection, cursor.room.key()],
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
                 WHERE session = ?1 AND connection = ?2 AND room_key = ?3",
                params![cursor.session, cursor.room.connection, cursor.room.key()],
                |row| row.get(0),
            )
            .optional()
            .map_err(io_error)?;
        let current = current.unwrap_or(0).max(0) as u64;

        let latest: Option<i64> = transaction
            .query_row(
                "SELECT MAX(record_id) FROM records WHERE connection = ?1 AND room_key = ?2",
                params![cursor.room.connection, cursor.room.key()],
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
                        "INSERT OR IGNORE INTO acks (session, connection, room_key, record_id)
                         VALUES (?1, ?2, ?3, ?4)",
                        params![
                            cursor.session,
                            cursor.room.connection,
                            cursor.room.key(),
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
                     WHERE session = ?1 AND connection = ?2 AND room_key = ?3 AND record_id = ?4",
                    params![
                        cursor.session,
                        cursor.room.connection,
                        cursor.room.key(),
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
                     WHERE session = ?1 AND connection = ?2 AND room_key = ?3 AND record_id = ?4",
                    params![
                        cursor.session,
                        cursor.room.connection,
                        cursor.room.key(),
                        next as i64
                    ],
                )
                .map_err(io_error)?;
            frontier = next;
        }

        transaction
            .execute(
                "INSERT INTO watermarks (session, connection, room_key, watermark)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (session, connection, room_key) DO UPDATE SET watermark = excluded.watermark",
                params![
                    cursor.session,
                    cursor.room.connection,
                    cursor.room.key(),
                    frontier as i64
                ],
            )
            .map_err(io_error)?;
        transaction.commit().map_err(io_error)?;

        Ok(frontier)
    }

    fn acked(&self, cursor: &Cursor) -> Result<Vec<u64>, StorageError> {
        let mut statement = self
            .conn
            .prepare(
                "SELECT record_id FROM acks
                 WHERE session = ?1 AND connection = ?2 AND room_key = ?3
                 ORDER BY record_id",
            )
            .map_err(io_error)?;
        let rows = statement
            .query_map(
                params![cursor.session, cursor.room.connection, cursor.room.key()],
                |row| row.get::<_, i64>(0),
            )
            .map_err(io_error)?;

        let mut ids = Vec::new();
        for row in rows {
            ids.push(row.map_err(io_error)?.max(0) as u64);
        }
        Ok(ids)
    }
}

impl Notes for SqliteStore {
    fn note(&self, key: &NoteKey) -> Result<Option<Note>, StorageError> {
        let row = self
            .conn
            .query_row(
                "SELECT value, label, status, corrected, updated_at FROM notes
                 WHERE connection = ?1 AND room_key = ?2 AND kind = ?3 AND note_key = ?4",
                params![
                    key.room.connection,
                    key.room.key(),
                    key.kind.as_str(),
                    key.key
                ],
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
                 (connection, room_key, kind, note_key, value, label, status, corrected, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT (connection, room_key, kind, note_key) DO UPDATE SET
                     value = excluded.value,
                     label = excluded.label,
                     status = excluded.status,
                     corrected = excluded.corrected,
                     updated_at = excluded.updated_at",
                params![
                    note.key.room.connection,
                    note.key.room.key(),
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
                "DELETE FROM notes WHERE connection = ?1 AND room_key = ?2 AND kind = ?3 AND note_key = ?4",
                params![
                    key.room.connection,
                    key.room.key(),
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
                 FROM notes WHERE connection = ?1 AND room_key = ?2
                 ORDER BY kind, note_key",
            )
            .map_err(io_error)?;

        let rows = statement
            .query_map(params![room.connection, room.key()], |row| {
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
                "INSERT INTO deliveries (session, connection, room_key, record_id, times)
                 VALUES (?1, ?2, ?3, ?4, 1)
                 ON CONFLICT (session, connection, room_key, record_id)
                 DO UPDATE SET times = times + 1",
                params![
                    cursor.session,
                    cursor.room.connection,
                    cursor.room.key(),
                    id as i64
                ],
            )
            .map_err(io_error)?;

        let times: i64 = self
            .conn
            .query_row(
                "SELECT times FROM deliveries
                 WHERE session = ?1 AND connection = ?2 AND room_key = ?3 AND record_id = ?4",
                params![
                    cursor.session,
                    cursor.room.connection,
                    cursor.room.key(),
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
                 WHERE session = ?1 AND connection = ?2 AND room_key = ?3 AND record_id = ?4",
                params![
                    cursor.session,
                    cursor.room.connection,
                    cursor.room.key(),
                    id as i64
                ],
            )
            .map_err(io_error)?;
        Ok(())
    }
}

impl SessionMaps for SqliteStore {
    fn session_map(&self, session: &str) -> Result<Option<SessionMap>, StorageError> {
        let row = self
            .conn
            .query_row(
                "SELECT harness_session, meta, updated_at FROM session_maps WHERE session = ?1",
                params![session],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                },
            )
            .optional()
            .map_err(io_error)?;

        let Some((harness_session, meta, updated_at)) = row else {
            return Ok(None);
        };
        let meta: serde_json::Value = serde_json::from_str(&meta)
            .map_err(|error| backend(format!("会话映射的元信息读不懂：{error}")))?;
        Ok(Some(SessionMap {
            session: session.to_owned(),
            harness_session,
            meta,
            updated_at,
        }))
    }

    fn upsert_session_map(&mut self, map: SessionMap) -> Result<(), StorageError> {
        let meta = serde_json::to_string(&map.meta)
            .map_err(|error| backend(format!("会话映射的元信息存不进库：{error}")))?;
        self.conn
            .execute(
                "INSERT INTO session_maps (session, harness_session, meta, updated_at)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (session) DO UPDATE SET
                     harness_session = excluded.harness_session,
                     meta = excluded.meta,
                     updated_at = excluded.updated_at",
                params![map.session, map.harness_session, meta, map.updated_at],
            )
            .map_err(io_error)?;
        Ok(())
    }

    fn delete_session_map(&mut self, session: &str) -> Result<bool, StorageError> {
        let removed = self
            .conn
            .execute("DELETE FROM session_maps WHERE session = ?1", params![session])
            .map_err(io_error)?;
        Ok(removed > 0)
    }

    fn session_maps(&self) -> Result<Vec<SessionMap>, StorageError> {
        let mut statement = self
            .conn
            .prepare(
                "SELECT session, harness_session, meta, updated_at FROM session_maps
                 ORDER BY session",
            )
            .map_err(io_error)?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })
            .map_err(io_error)?;

        let mut maps = Vec::new();
        for row in rows {
            let (session, harness_session, meta, updated_at) = row.map_err(io_error)?;
            let meta: serde_json::Value = serde_json::from_str(&meta)
                .map_err(|error| backend(format!("会话映射的元信息读不懂：{error}")))?;
            maps.push(SessionMap {
                session,
                harness_session,
                meta,
                updated_at,
            });
        }
        Ok(maps)
    }
}

impl Feedbacks for SqliteStore {
    fn record_feedback(&mut self, draft: FeedbackDraft) -> Result<Recorded, StorageError> {
        let refs = serde_json::to_string(&draft.refs)
            .map_err(|error| backend(format!("反馈的出处存不进库：{error}")))?;
        let context = serde_json::to_string(&draft.context)
            .map_err(|error| backend(format!("反馈的现场存不进库：{error}")))?;

        // 先看指纹在不在。在就合并，不在就新建——比 upsert 多一次查询，但两种情况的
        // 返回不一样（新建要给新 id，合并要给累计次数），分开写更好读。
        let existing: Option<(i64, i64)> = self
            .conn
            .query_row(
                "SELECT id, occurrences FROM feedback WHERE fingerprint = ?1",
                params![draft.fingerprint],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()
            .map_err(io_error)?;

        if let Some((id, occurrences)) = existing {
            self.conn
                .execute(
                    "UPDATE feedback
                        SET repeats = repeats + 1,
                            occurrences = ?2,
                            last_at = ?3,
                            -- 类型/严重度按 ADR-0021 保留第一次的（人已经看过那一行）。
                            -- 但「空着的字段」不是分类，是缺信息：第一次没填、
                            -- 或者那次参数被解析器吞了，后来的报告带全了就该补上。
                            refs = CASE WHEN refs IN ('[]', '') THEN ?4 ELSE refs END,
                            suggestion = COALESCE(NULLIF(suggestion, ''), ?5),
                            tool = COALESCE(NULLIF(tool, ''), ?6),
                            -- 又出现了：原来标了已修/不修说明没修好，重新打开。
                            status = CASE WHEN status IN ('resolved','wontfix') THEN 'open' ELSE status END
                      WHERE id = ?1",
                    params![id, occurrences + draft.occurrences as i64, draft.at, refs, draft.suggestion, draft.tool],
                )
                .map_err(io_error)?;
            let (repeats, occurrences): (i64, i64) = self
                .conn
                .query_row(
                    "SELECT repeats, occurrences FROM feedback WHERE id = ?1",
                    params![id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .map_err(io_error)?;
            return Ok(Recorded {
                id,
                repeats: repeats as u32,
                occurrences: occurrences as u32,
                duplicate: true,
            });
        }

        self.conn
            .execute(
                "INSERT INTO feedback
                    (fingerprint, title, body, suggestion, kind, severity, refs,
                     occurrences, repeats, tool, session, harness, room, context,
                     status, note, first_at, last_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 1, ?9, ?10, ?11, ?12, ?13,
                         'open', NULL, ?14, ?14)",
                params![
                    draft.fingerprint,
                    draft.title,
                    draft.body,
                    draft.suggestion,
                    draft.kind,
                    draft.severity,
                    refs,
                    draft.occurrences as i64,
                    draft.tool,
                    draft.session,
                    draft.harness,
                    draft.room,
                    context,
                    draft.at,
                ],
            )
            .map_err(io_error)?;

        let id = self.conn.last_insert_rowid();
        self.cap_feedback()?;
        Ok(Recorded {
            id,
            repeats: 1,
            occurrences: draft.occurrences,
            duplicate: false,
        })
    }

    fn feedback(&self, status: Option<&str>, limit: usize) -> Result<Vec<Feedback>, StorageError> {
        // 不筛状态时把 `?1` 绑成空串，条件恒真——比按条件拼两条 SQL 更容易看出差别在哪。
        let filter = status.unwrap_or("");
        let mut statement = self
            .conn
            .prepare(
                "SELECT id, title, body, suggestion, kind, severity, refs, occurrences, repeats,
                        tool, session, harness, room, context, status, note, first_at, last_at
                   FROM feedback
                  WHERE (?1 = '' OR status = ?1)
                  ORDER BY last_at DESC
                  LIMIT ?2",
            )
            .map_err(io_error)?;
        let rows = statement
            .query_map(params![filter, limit as i64], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, i64>(7)?,
                    row.get::<_, i64>(8)?,
                    row.get::<_, Option<String>>(9)?,
                    row.get::<_, String>(10)?,
                    row.get::<_, Option<String>>(11)?,
                    row.get::<_, Option<String>>(12)?,
                    row.get::<_, String>(13)?,
                    row.get::<_, String>(14)?,
                    row.get::<_, Option<String>>(15)?,
                    row.get::<_, i64>(16)?,
                    row.get::<_, i64>(17)?,
                ))
            })
            .map_err(io_error)?;

        let mut items = Vec::new();
        for row in rows {
            let row = row.map_err(io_error)?;
            items.push(Feedback {
                id: row.0,
                title: row.1,
                body: row.2,
                suggestion: row.3,
                kind: row.4,
                severity: row.5,
                refs: serde_json::from_str(&row.6).unwrap_or_default(),
                occurrences: row.7.max(0) as u32,
                repeats: row.8.max(0) as u32,
                tool: row.9,
                session: row.10,
                harness: row.11,
                room: row.12,
                context: serde_json::from_str(&row.13).unwrap_or(serde_json::Value::Null),
                status: row.14,
                note: row.15,
                first_at: row.16,
                last_at: row.17,
            });
        }
        Ok(items)
    }

    fn set_feedback_status(
        &mut self,
        id: i64,
        status: &str,
        note: Option<&str>,
    ) -> Result<bool, StorageError> {
        let changed = self
            .conn
            .execute(
                "UPDATE feedback SET status = ?2, note = COALESCE(?3, note) WHERE id = ?1",
                params![id, status, note],
            )
            .map_err(io_error)?;
        Ok(changed > 0)
    }
}

impl SqliteStore {
    /// 反馈条数别无限长：超了先扔最久没被提过的。
    fn cap_feedback(&self) -> Result<(), StorageError> {
        let total: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM feedback", [], |row| row.get(0))
            .map_err(io_error)?;
        if total <= FEEDBACK_CAP as i64 {
            return Ok(());
        }
        self.conn
            .execute(
                "DELETE FROM feedback WHERE id IN (
                     SELECT id FROM feedback ORDER BY last_at ASC LIMIT ?1
                 )",
                params![total - FEEDBACK_CAP as i64],
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
