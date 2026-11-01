//! 存储层：流水、水位、笔记、投递计数。
//!
//! 四样东西的关系：
//! - **流水**是每个房间的完整记录，永久保存，投没投过都在；
//! - **水位**是「这个设备处理到哪条了」的游标，只经显式确认往前推（ADR-0010）；
//! - **未读**不是独立存储，是流水里水位之后、再减去乱序确认（`acks`）的部分——
//!   算出来的，不是攒出来的。
//!
//! 粒度不一样，别搞混：
//! - 流水**按房间**，一个房间一条流水，同房间的所有设备看的是同一份历史；
//! - 水位**按「设备 + 房间」**（[`Cursor`]），因为「处理到哪条」是设备自己的进度——
//!   两个设备同时订一个群，各自的进度不能互相踩；
//! - 笔记**按房间**，是「关于这个房间的共识」，同房间的设备共享；
//! - 投递计数**按「设备 + 房间 + 记录」**，只为一件事存在：重投时说得出「第 N 次」。
//!
//! 两份实现：`MemoryStore`（测试、嵌入式）与 [`SqliteStore`]（正式跑，跨重启）。
//! 桥只依赖 [`Store`] 这组约束，换后端不改调度（ADR-0014）。

use std::collections::{BTreeMap, BTreeSet, HashMap};

use acb_proto::{Element, Record, RecordKind, RoomId, Sender};
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
    /// 平台自己的消息编号；事件没有，留 `None`。
    pub platform_id: Option<String>,
    /// 是不是引用了我发的某条消息。由调度层在入库前填（要查库才判得出）。
    pub reply_to_self: bool,
    /// 事件指向的平台对象编号。撤回事件用它指向被撤回的那条消息；
    /// 消息记录不用这个字段。
    pub target_platform_id: Option<String>,
    /// 给目标记录追加的标签（如 `recall`）。只有带 target 的事件才用得到。
    pub target_labels: Vec<String>,
    /// 只进流水，不投递、不唤醒、不计未读。适配器自己决定哪些事件属于这一类。
    pub suppressed: bool,
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
            platform_id: None,
            reply_to_self: false,
            target_platform_id: None,
            target_labels: Vec::new(),
            suppressed: false,
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
                tags: Vec::new(),
                payload: None,
                summary: None,
            },
            sender,
            at,
            elements,
            at_self,
            platform_id: None,
            reply_to_self: false,
            target_platform_id: None,
            target_labels: Vec::new(),
            suppressed: false,
        }
    }

    /// 给事件补语义标签、原始载荷和一句人话。对消息记录无效果。
    #[must_use]
    pub fn with_event_meta(
        mut self,
        tags: Vec<String>,
        payload: Option<serde_json::Value>,
        summary: Option<String>,
    ) -> Self {
        if let RecordKind::Event {
            tags: current_tags,
            payload: current_payload,
            summary: current_summary,
            ..
        } = &mut self.kind
        {
            *current_tags = tags;
            *current_payload = payload;
            *current_summary = summary;
        }
        self
    }

    /// 带上平台消息编号。适配器解析消息时填，用来反查「被引用的是谁」。
    #[must_use]
    pub fn with_platform_id(mut self, platform_id: impl Into<String>) -> Self {
        let platform_id = platform_id.into();
        if !platform_id.is_empty() {
            self.platform_id = Some(platform_id);
        }
        self
    }

    /// 事件指向的平台对象编号（例如撤回事件指向被撤回的消息）。
    #[must_use]
    pub fn with_target_platform_id(mut self, target: impl Into<String>) -> Self {
        let target = target.into();
        if !target.is_empty() {
            self.target_platform_id = Some(target);
        }
        self
    }

    /// 事件给目标记录追加的标签。只有带 target 的事件才用得到。
    #[must_use]
    pub fn with_target_labels(mut self, labels: impl IntoIterator<Item = String>) -> Self {
        self.target_labels = labels.into_iter().collect();
        self
    }

    /// 只存流水，不进未读、不投递、不唤醒。适配器按自己的语义决定。
    #[must_use]
    pub fn suppressed(mut self, suppressed: bool) -> Self {
        self.suppressed = suppressed;
        self
    }

    /// 标记「这是在回复我发的消息」。
    #[must_use]
    pub fn with_reply_to_self(mut self, reply_to_self: bool) -> Self {
        self.reply_to_self = reply_to_self;
        self
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

    /// 有记录的房间（清理由这里出发）。
    fn rooms(&self) -> Result<Vec<RoomId>, StorageError>;

    /// 房间里现有多少条。
    fn count(&self, room: &RoomId) -> Result<u64, StorageError>;

    /// 按 [`PrunePlan`] 删旧记录，返回删掉的条数。指向被删记录的投递计数与确认一起清掉。
    fn prune(&mut self, room: &RoomId, plan: &PrunePlan) -> Result<u64, StorageError>;

    /// 房间里那条平台编号为 `platform_id` 的记录是谁发的。
    ///
    /// 用途只有一个：别人引用了一条消息时，反查被引用的是不是我们自己发的
    /// （见 [`Record::reply_to_self`]）。查不到给 `None`——可能被清理掉了，
    /// 也可能对面没给编号，两种情况都不能瞎猜。
    fn sender_of(&self, room: &RoomId, platform_id: &str) -> Result<Option<Sender>, StorageError> {
        Ok(self.located(room, platform_id)?.map(|record| record.sender))
    }

    /// 按平台消息编号找回那条记录（要它的水位编号，也要它的原话）。
    ///
    /// 两个用处：判「是不是在回我的话」，以及把引用里的平台编号翻成本房间编号
    /// （ADR-0024）。同一编号出现两条时给最新的那条——平台会复用编号，取最近的才合理。
    fn located(&self, room: &RoomId, platform_id: &str) -> Result<Option<Record>, StorageError>;

    /// 给房间里平台编号为 `platform_id` 的最新一条记录追加标签。
    /// 找不到就返回 `false`；已有标签不重复添加。
    fn add_labels(
        &mut self,
        room: &RoomId,
        platform_id: &str,
        labels: &[String],
    ) -> Result<bool, StorageError>;
}

/// 一次清理的判据。三条**同时**成立才删——这是「宁可少删」的意思。
///
/// 判据由调度层算好（它才看得见水位与配置），存储层只负责按判据执行。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrunePlan {
    /// 编号必须小于它。安全边界：不碰任何在册设备的未读。
    pub below: u64,
    /// 至少留下最新的这么多条。实现里会夹到 >= 1——见 [`MIN_KEPT`]。
    pub keep_newest: u64,
    /// 时间必须早于它（Unix 毫秒）。`None` 表示不按时间判。
    pub older_than: Option<i64>,
}

/// 每个房间至少留一条。
///
/// 这条不是为了「留个纪念」，是为了**编号基准**：`MAX(record_id)+1` 是下一个编号，
/// 全删光了下一个编号就会从 1 重新开始，而水位还停在几百——新消息会被当成已读，
/// 直接消失。留一条就堵死了这个坑。
pub const MIN_KEPT: u64 = 1;

/// 水位：每个「设备 + 房间」的「处理到哪条了」。
pub trait Watermarks {
    /// 当前水位。没有记录过就是 0（编号从 1 开始）。
    fn watermark(&self, cursor: &Cursor) -> Result<u64, StorageError>;

    /// 确认这批编号已处理。只前进、不后退；只推进**连续**的前沿——中间缺一条就停在缺口前。
    fn ack(&mut self, cursor: &Cursor, ids: &[u64]) -> Result<u64, StorageError>;

    /// 水位之上、已经单独确认过的编号（乱序确认留下的读集合）。
    ///
    /// 投递时必须把这些编号过滤掉：它们已经处理过了，只是前面还有缺口没补上，
    /// 水位暂时推不过去。不过滤就会把同一条消息反复投给 agent。
    fn acked(&self, cursor: &Cursor) -> Result<Vec<u64>, StorageError>;
}

/// 一条水位游标：哪个会话、哪个房间。
///
/// 两个会话订同一个房间时各自推进、互不影响。这个键是存储层的，别和
/// [`NoteKey`] 混——笔记按房间共享，不按会话分。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Cursor {
    pub session: String,
    pub room: RoomId,
}

impl Cursor {
    pub fn new(session: impl Into<String>, room: RoomId) -> Self {
        Self {
            session: session.into(),
            room,
        }
    }
}

/// 未经确认的未读：水位之后、且不在「乱序确认」读集合里的流水。
///
/// 只按水位截断是不够的：agent 可能先确认了 #3、却还没确认 #1——`ack` 只推连续
/// 前沿，所以水位还停在 0，但 #3 已经处理过了。不过滤就会把它再投一遍。
///
/// `limit` 按**过滤后**的条数算。中间夹着已确认记录时，会往后多取一点，
/// 一直凑够 `limit` 条或读到房间末尾。
pub fn unread<S>(store: &S, cursor: &Cursor, limit: usize) -> Result<Vec<Record>, StorageError>
where
    S: Transcript + Watermarks,
{
    if limit == 0 {
        return Ok(Vec::new());
    }

    let watermark = store.watermark(cursor)?;
    let acked: BTreeSet<u64> = store.acked(cursor)?.into_iter().collect();
    if acked.is_empty() {
        return store.since(&cursor.room, watermark, limit);
    }

    let mut after = watermark;
    let mut unread = Vec::new();
    loop {
        let remaining = limit - unread.len();
        // 多取 acked.len() 条，补上被过滤掉的洞；够不够下一轮循环会再补。
        let take = remaining.saturating_add(acked.len()).max(1);
        let batch = store.since(&cursor.room, after, take)?;
        let exhausted = batch.len() < take;
        for record in batch {
            after = record.id;
            if acked.contains(&record.id) {
                continue;
            }
            unread.push(record);
            if unread.len() == limit {
                return Ok(unread);
            }
        }
        if exhausted {
            return Ok(unread);
        }
    }
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

/// 投递计数：同一条记录第几次投给同一个设备。
///
/// 只服务一个目的——重投时说得清「第 N 次投递」。放在存储层而不是调度层的内存里，
/// 是为了重启之后同一个 N 还能接着数（否则重启一次就从「第 1 次」重来，看着像没投过）。
pub trait Deliveries {
    /// 记一次投递，返回这是第几次（第一次给 1）。
    fn count_delivery(&mut self, cursor: &Cursor, id: u64) -> Result<u32, StorageError>;

    /// 丢掉计数。确认之后这条不会再投了，留着只是垃圾。
    fn forget_delivery(&mut self, cursor: &Cursor, id: u64) -> Result<(), StorageError>;
}

/// 会话映射：acb 会话 id → harness 内部会话 id。
///
/// 桥对映射的理解止于「一个 kv + 元信息」——它知道「这个 acb 会话在 harness 那边对应
/// 哪个 id」，但不知道 harness 内部会话的细节（ADR-0018）。落盘在桥的状态库里，
/// 桥重启不丢、控制台能直接读。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionMap {
    /// acb 会话 id。
    pub session: String,
    /// harness 内部会话 id（dsh 的 sessionId 之类）。
    pub harness_session: String,
    /// 元信息：给控制台展示用（标题、模型等，harness 回报什么存什么）。
    pub meta: serde_json::Value,
    /// 最后更新（Unix 毫秒）。
    pub updated_at: i64,
}

/// 会话映射读写。
pub trait SessionMaps {
    /// 取某个 acb 会话的映射；没有给 `None`。
    fn session_map(&self, session: &str) -> Result<Option<SessionMap>, StorageError>;

    /// 写（或覆盖）某个 acb 会话的映射。
    fn upsert_session_map(&mut self, map: SessionMap) -> Result<(), StorageError>;

    /// 删某个 acb 会话的映射（重置时用）。
    fn delete_session_map(&mut self, session: &str) -> Result<bool, StorageError>;

    /// 全部映射。
    fn session_maps(&self) -> Result<Vec<SessionMap>, StorageError>;
}

/// 一条来自 agent 的反馈（ADR-0021）。
///
/// 形状刻意**不像表单**：一个正文（想说多少说多少）+ 出处编号 + 自称发生次数。
/// 结构化字段都有缺省值，agent 只写正文也能入库；写得越细，看的人越省事。
#[derive(Debug, Clone, PartialEq)]
pub struct Feedback {
    pub id: i64,
    /// 正文第一行，给人扫列表用。
    pub title: String,
    /// 完整正文。
    pub body: String,
    /// 建议怎么改（可空）。
    pub suggestion: Option<String>,
    /// 类型：bug / gap / friction / idea。
    pub kind: String,
    /// 影响：blocker / major / minor。
    pub severity: String,
    /// 出处：水位编号（`#620`）、工具名、接口名、错误码，原样存。
    pub refs: Vec<String>,
    /// agent 自己说撞了几次（每次报告相加）。
    pub occurrences: u32,
    /// 被报告了几次（桥自己数的：同一条指纹去重后累加）。
    pub repeats: u32,
    pub tool: Option<String>,
    pub session: String,
    pub harness: Option<String>,
    pub room: Option<String>,
    /// 出事时的现场：票、模型、时间。看的人据此复现。
    pub context: serde_json::Value,
    /// open / acked / resolved / wontfix。
    pub status: String,
    /// 处理备注（人写的）。
    pub note: Option<String>,
    pub first_at: i64,
    pub last_at: i64,
}

/// 写一条反馈要的东西。指纹相同的会被合并（见 [`Feedbacks::record_feedback`]）。
#[derive(Debug, Clone, PartialEq)]
pub struct FeedbackDraft {
    /// 去重指纹：同一件事写了两遍，合成一条、次数加一。
    pub fingerprint: String,
    pub title: String,
    pub body: String,
    pub suggestion: Option<String>,
    pub kind: String,
    pub severity: String,
    pub refs: Vec<String>,
    /// 这次报告自称发生了几次。
    pub occurrences: u32,
    pub tool: Option<String>,
    pub session: String,
    pub harness: Option<String>,
    pub room: Option<String>,
    pub context: serde_json::Value,
    pub at: i64,
}

/// 记完一条之后回给调用方的话。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recorded {
    pub id: i64,
    /// 这是第几次被报告（1 = 第一次）。
    pub repeats: u32,
    /// 累计自称发生次数。
    pub occurrences: u32,
    /// 是不是合并到已有的一条里了。
    pub duplicate: bool,
}

/// 反馈库。条数有上限（[`FEEDBACK_CAP`]），满了先删最久没被提过的。
pub trait Feedbacks {
    /// 记一条。指纹已存在就合并：报告次数 +1、发生次数相加、`last_at` 刷新，
    /// 如果原来是 resolved / wontfix，重新打开（它又出现了，说明没修好）。
    fn record_feedback(&mut self, draft: FeedbackDraft) -> Result<Recorded, StorageError>;

    /// 列反馈，按最近一次被提的时间倒序。`status` 给了就只列那个状态。
    fn feedback(&self, status: Option<&str>, limit: usize) -> Result<Vec<Feedback>, StorageError>;

    /// 改状态 + 备注。`id` 不存在返回 `false`。
    fn set_feedback_status(
        &mut self,
        id: i64,
        status: &str,
        note: Option<&str>,
    ) -> Result<bool, StorageError>;
}

/// 反馈最多留多少条。一条最多几 KB，五千条是十几 MB 的量级——够留很久，也不至于失控。
/// 来源：实现默认。
pub const FEEDBACK_CAP: usize = 5000;

/// 六样东西合起来。桥只依赖这一个约束，换后端只换实现。
pub trait Store: Transcript + Watermarks + Notes + Deliveries + SessionMaps + Feedbacks {}

impl<T> Store for T where
    T: Transcript + Watermarks + Notes + Deliveries + SessionMaps + Feedbacks
{
}

mod sqlite;

pub use sqlite::{Counts, SCHEMA_VERSION, SqliteStore};

/// 内存实现。测试与开发用；正式跑用 [`SqliteStore`]。
#[derive(Debug, Default)]
pub struct MemoryStore {
    next_id: HashMap<RoomId, u64>,
    records: HashMap<RoomId, Vec<Record>>,
    watermark: HashMap<Cursor, u64>,
    acked: HashMap<Cursor, BTreeSet<u64>>,
    notes: BTreeMap<NoteKey, Note>,
    deliveries: HashMap<(Cursor, u64), u32>,
    session_maps: HashMap<String, SessionMap>,
    feedback: Vec<Feedback>,
    /// 反馈 id → 去重指纹。只给内存实现用：合并时得知道哪条是同一条。
    feedback_keys: HashMap<i64, String>,
    next_feedback: i64,
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
            reply_to_self: draft.reply_to_self,
            platform_id: draft.platform_id,
            // 引用对齐是投递时算的（要查库），流水里只留事实本身。
            quote_ref: None,
            labels: Vec::new(),
            redelivery: None,
            // 流水里存的是原话，旁白只在投递时由调度层补（见 engine::open_turn）。
            note: None,
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

    fn rooms(&self) -> Result<Vec<RoomId>, StorageError> {
        let mut rooms: Vec<RoomId> = self
            .records
            .iter()
            .filter(|(_, records)| !records.is_empty())
            .map(|(room, _)| room.clone())
            .collect();
        rooms.sort();
        Ok(rooms)
    }

    fn count(&self, room: &RoomId) -> Result<u64, StorageError> {
        Ok(self
            .records
            .get(room)
            .map_or(0, |records| records.len() as u64))
    }

    fn prune(&mut self, room: &RoomId, plan: &PrunePlan) -> Result<u64, StorageError> {
        let Some(records) = self.records.get_mut(room) else {
            return Ok(0);
        };

        let keep = plan.keep_newest.max(MIN_KEPT) as usize;
        let keep_from = records.len().saturating_sub(keep);

        let mut removed = Vec::new();
        let mut kept = Vec::with_capacity(records.len());
        for (index, record) in records.drain(..).enumerate() {
            let dead = index < keep_from
                && record.id < plan.below
                && plan.older_than.is_none_or(|before| record.at < before);
            if dead {
                removed.push(record.id);
            } else {
                kept.push(record);
            }
        }
        *records = kept;

        if !removed.is_empty() {
            self.deliveries
                .retain(|(cursor, id), _| cursor.room != *room || !removed.contains(id));
            for (cursor, acked) in self.acked.iter_mut() {
                if cursor.room == *room {
                    for id in &removed {
                        acked.remove(id);
                    }
                }
            }
        }

        Ok(removed.len() as u64)
    }

    fn sender_of(&self, room: &RoomId, platform_id: &str) -> Result<Option<Sender>, StorageError> {
        Ok(self.located(room, platform_id)?.map(|record| record.sender))
    }

    fn located(&self, room: &RoomId, platform_id: &str) -> Result<Option<Record>, StorageError> {
        if platform_id.is_empty() {
            return Ok(None);
        }
        Ok(self.records.get(room).and_then(|records| {
            records
                .iter()
                .rev()
                .find(|record| record.platform_id.as_deref() == Some(platform_id))
                .cloned()
        }))
    }

    fn add_labels(
        &mut self,
        room: &RoomId,
        platform_id: &str,
        labels: &[String],
    ) -> Result<bool, StorageError> {
        if platform_id.is_empty() || labels.is_empty() {
            return Ok(false);
        }
        let Some(records) = self.records.get_mut(room) else {
            return Ok(false);
        };
        let Some(index) = records
            .iter()
            .rposition(|record| record.platform_id.as_deref() == Some(platform_id))
        else {
            return Ok(false);
        };
        let before = records[index].labels.len();
        for label in labels {
            if !records[index].labels.contains(label) {
                records[index].labels.push(label.clone());
            }
        }
        Ok(records[index].labels.len() > before)
    }
}

impl Watermarks for MemoryStore {
    fn watermark(&self, cursor: &Cursor) -> Result<u64, StorageError> {
        Ok(self.watermark.get(cursor).copied().unwrap_or(0))
    }

    fn ack(&mut self, cursor: &Cursor, ids: &[u64]) -> Result<u64, StorageError> {
        let latest = self.next_id.get(&cursor.room).copied().unwrap_or(0);
        let current = self.watermark.get(cursor).copied().unwrap_or(0);

        for &id in ids {
            if id > latest {
                return Err(StorageError::UnknownRecord {
                    room: cursor.room.to_string(),
                    id,
                });
            }
            if id > current {
                self.acked.entry(cursor.clone()).or_default().insert(id);
            }
        }

        let acked = self.acked.entry(cursor.clone()).or_default();
        let mut frontier = current;
        while acked.remove(&(frontier + 1)) {
            frontier += 1;
        }
        self.watermark.insert(cursor.clone(), frontier);
        Ok(frontier)
    }

    fn acked(&self, cursor: &Cursor) -> Result<Vec<u64>, StorageError> {
        Ok(self
            .acked
            .get(cursor)
            .map(|ids| ids.iter().copied().collect())
            .unwrap_or_default())
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

impl Deliveries for MemoryStore {
    fn count_delivery(&mut self, cursor: &Cursor, id: u64) -> Result<u32, StorageError> {
        let count = self.deliveries.entry((cursor.clone(), id)).or_insert(0);
        *count += 1;
        Ok(*count)
    }

    fn forget_delivery(&mut self, cursor: &Cursor, id: u64) -> Result<(), StorageError> {
        self.deliveries.remove(&(cursor.clone(), id));
        Ok(())
    }
}

impl SessionMaps for MemoryStore {
    fn session_map(&self, session: &str) -> Result<Option<SessionMap>, StorageError> {
        Ok(self.session_maps.get(session).cloned())
    }

    fn upsert_session_map(&mut self, map: SessionMap) -> Result<(), StorageError> {
        self.session_maps.insert(map.session.clone(), map);
        Ok(())
    }

    fn delete_session_map(&mut self, session: &str) -> Result<bool, StorageError> {
        Ok(self.session_maps.remove(session).is_some())
    }

    fn session_maps(&self) -> Result<Vec<SessionMap>, StorageError> {
        let mut maps: Vec<SessionMap> = self.session_maps.values().cloned().collect();
        maps.sort_by(|a, b| a.session.cmp(&b.session));
        Ok(maps)
    }
}

impl Feedbacks for MemoryStore {
    fn record_feedback(&mut self, draft: FeedbackDraft) -> Result<Recorded, StorageError> {
        let known = self
            .feedback_keys
            .iter()
            .find(|(_, fingerprint)| **fingerprint == draft.fingerprint)
            .map(|(id, _)| *id);
        if let Some(id) = known {
            let existing = self
                .feedback
                .iter_mut()
                .find(|item| item.id == id)
                .expect("指纹表里有的 id 一定在向量里");
            existing.repeats += 1;
            existing.occurrences = existing.occurrences.saturating_add(draft.occurrences);
            existing.last_at = draft.at;
            // 类型/严重度保留第一次的（ADR-0021：人已经看过那一行，不该被后来的报告改分类）。
            // 但空着的字段不是分类，是缺信息——第一次没填、或者那次参数被吞了，后来带全了就补上。
            if existing.refs.is_empty() && !draft.refs.is_empty() {
                existing.refs = draft.refs;
            }
            if existing.suggestion.is_none() && draft.suggestion.is_some() {
                existing.suggestion = draft.suggestion;
            }
            if existing.tool.is_none() && draft.tool.is_some() {
                existing.tool = draft.tool;
            }
            // 又出现了：原来是「已修/不修」，说明没修好，重新打开。
            if existing.status == "resolved" || existing.status == "wontfix" {
                existing.status = "open".to_owned();
            }
            return Ok(Recorded {
                id: existing.id,
                repeats: existing.repeats,
                occurrences: existing.occurrences,
                duplicate: true,
            });
        }

        self.next_feedback += 1;
        let feedback = Feedback {
            id: self.next_feedback,
            title: draft.title,
            body: draft.body,
            suggestion: draft.suggestion,
            kind: draft.kind,
            severity: draft.severity,
            refs: draft.refs,
            occurrences: draft.occurrences,
            repeats: 1,
            tool: draft.tool,
            session: draft.session,
            harness: draft.harness,
            room: draft.room,
            context: draft.context,
            status: "open".to_owned(),
            note: None,
            first_at: draft.at,
            last_at: draft.at,
        };
        // 满了就先扔最久没被提过的（保留 id 递增，编号不复用）。
        while self.feedback.len() >= FEEDBACK_CAP {
            let oldest = self
                .feedback
                .iter()
                .enumerate()
                .min_by_key(|(_, item)| item.last_at)
                .map(|(index, _)| index);
            match oldest {
                Some(index) => {
                    let gone = self.feedback.remove(index);
                    self.feedback_keys.remove(&gone.id);
                }
                None => break,
            }
        }
        let recorded = Recorded {
            id: feedback.id,
            repeats: 1,
            occurrences: feedback.occurrences,
            duplicate: false,
        };
        self.feedback_keys
            .insert(feedback.id, draft.fingerprint.clone());
        self.feedback.push(feedback);
        Ok(recorded)
    }

    fn feedback(&self, status: Option<&str>, limit: usize) -> Result<Vec<Feedback>, StorageError> {
        let mut items: Vec<Feedback> = self
            .feedback
            .iter()
            .filter(|item| status.is_none_or(|wanted| item.status == wanted))
            .cloned()
            .collect();
        items.sort_by(|a, b| b.last_at.cmp(&a.last_at));
        items.truncate(limit);
        Ok(items)
    }

    fn set_feedback_status(
        &mut self,
        id: i64,
        status: &str,
        note: Option<&str>,
    ) -> Result<bool, StorageError> {
        let Some(item) = self.feedback.iter_mut().find(|item| item.id == id) else {
            return Ok(false);
        };
        item.status = status.to_owned();
        if let Some(note) = note {
            item.note = Some(note.to_owned());
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use acb_proto::Element;

    fn room(platform: &str, id: &str) -> RoomId {
        RoomId::new(platform, id).expect("合法")
    }

    fn cursor(device: &str, room: &RoomId) -> Cursor {
        Cursor::new(device, room.clone())
    }

    fn draft(room: RoomId, text: &str) -> RecordDraft {
        RecordDraft::message(
            room,
            Sender {
                id: "10001".into(),
                name: "老王".into(),
                role: None,
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
        let main = cursor("main", &qq);
        store.append(draft(qq.clone(), "a")).expect("可写");
        store.append(draft(qq.clone(), "b")).expect("可写");
        store.append(draft(qq.clone(), "c")).expect("可写");

        assert_eq!(unread(&store, &main, 100).expect("可读").len(), 3);
        store.ack(&main, &[1]).expect("可确认");
        assert_eq!(unread(&store, &main, 100).expect("可读").len(), 2);
    }

    #[test]
    fn watermark_only_moves_over_a_contiguous_frontier() {
        let mut store = MemoryStore::new();
        let qq = room("qq", "1");
        let main = cursor("main", &qq);
        for text in ["a", "b", "c"] {
            store.append(draft(qq.clone(), text)).expect("可写");
        }

        let frontier = store.ack(&main, &[1, 3]).expect("可确认");
        assert_eq!(frontier, 1, "缺了 2，只能推到 1");

        let frontier = store.ack(&main, &[2]).expect("可确认");
        assert_eq!(frontier, 3, "补上 2 之后一次推到 3");
    }

    /// 乱序确认过的记录不能再投：水位没跨过它，但它已经处理过了。
    #[test]
    fn unread_skips_out_of_order_acks() {
        let mut store = MemoryStore::new();
        let qq = room("qq", "1");
        let main = cursor("main", &qq);
        for text in ["a", "b", "c", "d"] {
            store.append(draft(qq.clone(), text)).expect("可写");
        }

        assert_eq!(store.ack(&main, &[3]).expect("可确认"), 0, "缺 1、2，水位不动");
        let pending = unread(&store, &main, 100).expect("可读");
        assert_eq!(
            pending.iter().map(|record| record.id).collect::<Vec<_>>(),
            vec![1, 2, 4],
            "#3 已经确认过，不能再投"
        );
    }

    /// `limit` 按过滤后的条数算：中间夹着已确认的记录时，要继续往后取够。
    #[test]
    fn unread_limit_counts_only_returned_records() {
        let mut store = MemoryStore::new();
        let qq = room("qq", "1");
        let main = cursor("main", &qq);
        for index in 1..=6 {
            store
                .append(draft(qq.clone(), &format!("m{index}")))
                .expect("可写");
        }
        store.ack(&main, &[2, 4]).expect("可确认");

        let pending = unread(&store, &main, 2).expect("可读");
        assert_eq!(
            pending.iter().map(|record| record.id).collect::<Vec<_>>(),
            vec![1, 3]
        );

        let pending = unread(&store, &main, 4).expect("可读");
        assert_eq!(
            pending.iter().map(|record| record.id).collect::<Vec<_>>(),
            vec![1, 3, 5, 6]
        );
    }

    #[test]
    fn watermark_never_moves_backwards() {
        let mut store = MemoryStore::new();
        let qq = room("qq", "1");
        let main = cursor("main", &qq);
        store.append(draft(qq.clone(), "a")).expect("可写");
        store.append(draft(qq.clone(), "b")).expect("可写");

        assert_eq!(store.ack(&main, &[1, 2]).expect("可确认"), 2);
        assert_eq!(store.ack(&main, &[1]).expect("可确认"), 2, "旧编号不影响");
    }

    #[test]
    fn acking_a_missing_record_is_an_error() {
        let mut store = MemoryStore::new();
        let qq = room("qq", "1");
        let main = cursor("main", &qq);
        store.append(draft(qq.clone(), "a")).expect("可写");

        let error = store.ack(&main, &[9]).expect_err("应当报错");
        assert!(matches!(error, StorageError::UnknownRecord { id: 9, .. }));
    }

    #[test]
    fn rooms_on_different_platforms_never_share_watermarks() {
        let mut store = MemoryStore::new();
        let qq = room("qq", "123");
        let discord = room("discord", "123");

        store.append(draft(qq.clone(), "a")).expect("可写");
        store.append(draft(discord.clone(), "b")).expect("可写");
        store.ack(&cursor("main", &qq), &[1]).expect("可确认");

        assert_eq!(store.watermark(&cursor("main", &qq)).expect("可读"), 1);
        assert_eq!(
            store.watermark(&cursor("main", &discord)).expect("可读"),
            0,
            "另一个平台的 123 不能被带着走"
        );
    }

    #[test]
    fn two_devices_in_one_room_keep_separate_progress() {
        let mut store = MemoryStore::new();
        let qq = room("qq", "123");
        let alice = cursor("alice", &qq);
        let bob = cursor("bob", &qq);

        store.append(draft(qq.clone(), "a")).expect("可写");
        store.append(draft(qq.clone(), "b")).expect("可写");

        store.ack(&alice, &[1, 2]).expect("可确认");
        assert_eq!(store.watermark(&alice).expect("可读"), 2);
        assert_eq!(
            store.watermark(&bob).expect("可读"),
            0,
            "同一房间的另一个设备有自己的进度"
        );
        assert_eq!(unread(&store, &bob, 100).expect("可读").len(), 2);
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
                role: None,
                },
                1_793_000_000_001,
                "poke",
                vec![Element::text("[拍了拍我]")],
                true,
            ))
            .expect("可写");

        let all = unread(&store, &cursor("main", &qq), 100).expect("可读");
        assert_eq!(all.len(), 2, "拍一拍和平常消息在同一条流水里");
        assert!(all[1].is_event("poke"));
        assert!(all[1].at_self);
    }

    // ---- 清理 ----

    fn at(room: RoomId, text: &str, at: i64) -> RecordDraft {
        let mut draft = draft(room, text);
        draft.at = at;
        draft
    }

    fn ids(store: &MemoryStore, room: &RoomId) -> Vec<u64> {
        store
            .since(room, 0, 100)
            .expect("可读")
            .iter()
            .map(|record| record.id)
            .collect()
    }

    #[test]
    fn prune_takes_only_what_is_below_the_line() {
        let mut store = MemoryStore::new();
        let qq = room("qq", "1");
        for text in ["a", "b", "c", "d"] {
            store.append(draft(qq.clone(), text)).expect("可写");
        }

        let plan = PrunePlan {
            below: 3,
            keep_newest: 1,
            older_than: None,
        };
        assert_eq!(store.prune(&qq, &plan).expect("可清"), 2);
        assert_eq!(ids(&store, &qq), vec![3, 4]);
    }

    #[test]
    fn prune_keeps_the_newest_even_if_the_line_is_high() {
        let mut store = MemoryStore::new();
        let qq = room("qq", "1");
        for text in ["a", "b", "c", "d", "e"] {
            store.append(draft(qq.clone(), text)).expect("可写");
        }

        // 水位已经推到最新，但要求每房间至少留 2 条：只能删 1、2、3。
        let plan = PrunePlan {
            below: 6,
            keep_newest: 2,
            older_than: None,
        };
        assert_eq!(store.prune(&qq, &plan).expect("可清"), 3);
        assert_eq!(ids(&store, &qq), vec![4, 5]);
    }

    #[test]
    fn prune_respects_the_time_line() {
        let mut store = MemoryStore::new();
        let qq = room("qq", "1");
        let now = 1_793_000_000_000;
        store
            .append(at(qq.clone(), "很久以前", now - 60 * 86_400_000))
            .expect("可写");
        store.append(at(qq.clone(), "刚才", now)).expect("可写");

        let plan = PrunePlan {
            below: 3,
            keep_newest: 1,
            older_than: Some(now - 30 * 86_400_000),
        };
        assert_eq!(
            store.prune(&qq, &plan).expect("可清"),
            1,
            "只删 60 天前那条"
        );
        assert_eq!(ids(&store, &qq), vec![2]);
    }

    #[test]
    fn prune_never_empties_a_room() {
        let mut store = MemoryStore::new();
        let qq = room("qq", "1");
        for text in ["a", "b"] {
            store.append(draft(qq.clone(), text)).expect("可写");
        }

        // keep_newest 传 0 也只会被抬到 1：编号基准不能没有。
        let plan = PrunePlan {
            below: 3,
            keep_newest: 0,
            older_than: None,
        };
        assert_eq!(store.prune(&qq, &plan).expect("可清"), 1);
        assert_eq!(ids(&store, &qq), vec![2]);

        let next = store.append(draft(qq.clone(), "c")).expect("可写");
        assert_eq!(next.id, 3, "清完之后编号接着 2 往上，不能回到 1");
    }

    #[test]
    fn prune_drops_delivery_counts_of_removed_records() {
        let mut store = MemoryStore::new();
        let qq = room("qq", "1");
        let main = cursor("main", &qq);
        store.append(draft(qq.clone(), "a")).expect("可写");
        store.append(draft(qq.clone(), "b")).expect("可写");

        store.count_delivery(&main, 1).expect("能记");
        store.count_delivery(&main, 1).expect("能记");

        store
            .prune(
                &qq,
                &PrunePlan {
                    below: 2,
                    keep_newest: 1,
                    older_than: None,
                },
            )
            .expect("可清");

        assert_eq!(
            store.count_delivery(&main, 1).expect("能记"),
            1,
            "旧计数跟着记录一起没了，不是接着 3 往上数"
        );
        assert_eq!(
            store.count_delivery(&main, 2).expect("能记"),
            1,
            "没删的那条不受影响"
        );
    }

    #[test]
    fn rooms_lists_only_rooms_with_records() {
        let mut store = MemoryStore::new();
        assert!(store.rooms().expect("可读").is_empty());

        let qq = room("qq", "1");
        let discord = room("discord", "1");
        store.append(draft(discord.clone(), "b")).expect("可写");
        store.append(draft(qq.clone(), "a")).expect("可写");

        assert_eq!(store.rooms().expect("可读"), vec![discord, qq]);
    }
}
