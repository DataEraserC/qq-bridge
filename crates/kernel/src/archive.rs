//! 归档：按记忆名分片的、有界的事件记忆。
//!
//! 归档回答「这个记忆名最近发生过什么」。它**只保留还能被用到的部分**——不是聊天记录
//! 数据库，见 `docs/architecture/03-kernel.md#归档`。
//!
//! 四条设计约束在这里落地：
//!
//! - **分片键是记忆名。** 分片是队列与投递的单位；两个设备填同一个记忆名就共享一片。
//! - **水位按群。** 一次投递只覆盖一个群（`TurnContext` 的 `group` 是单值），所以每个
//!   群各有自己的未读水位。用一个记忆名一个水位会把别的群的未读一起划掉。
//! - **自己账号的消息不进未读**（不变量一）。它照样归档——上下文需要它，但水位不因它前进。
//! - **水位只前进。** [`Archive::consume`] 把水位推到某个序号，表示「这之前的被某个回合
//!   消费过」。回退水位等于把已消费的消息再投递一次。
//!
//! 有界与回收：已消费的只留最近 [`Limits::context_messages`] 条供上下文；整片还有条数与
//! 字节双限。未读在硬上限内绝不丢弃——那才是「投递失败不丢消息」。真的超了（队列长时间
//! 忙），丢弃条数记在 [`Archive::dropped`]，投递时据此说明有缺。
//!
//! 序号由归档分配，不在这里读时钟——可测、可重放。
//!
//! 存储格式不在这里决定。归档与笔记共用同一层持久化，见
//! `docs/architecture/03-kernel.md#持久化`；本模块只提供与存储无关的 JSON 往返。

use std::collections::{BTreeMap, VecDeque};

use serde::{Deserialize, Serialize};

use crate::chat::{ConversationKey, ReplyAddress};

/// 每片保留的消息条数上限。
pub const DEFAULT_MAX_MESSAGES: usize = 500;
/// 每片保留的负载字节上限。
pub const DEFAULT_MAX_BYTES: usize = 4 * 1024 * 1024;
/// 已消费的消息里留多少条供后续上下文用。
pub const DEFAULT_CONTEXT_MESSAGES: usize = 50;

/// 分片上限。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub max_messages: usize,
    /// 按消息负载的 UTF-8 字节计，不是进程内存占用。
    pub max_bytes: usize,
    /// 已消费的消息保留多少条。0 表示不留上下文。
    pub context_messages: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_messages: DEFAULT_MAX_MESSAGES,
            max_bytes: DEFAULT_MAX_BYTES,
            context_messages: DEFAULT_CONTEXT_MESSAGES,
        }
    }
}

impl Limits {
    /// 两个硬上限不允许为零：零会让归档一写入就清空，而清空不是「有界」。
    fn normalized(self) -> Self {
        Self {
            max_messages: self.max_messages.max(1),
            max_bytes: self.max_bytes.max(1),
            context_messages: self.context_messages,
        }
    }
}

/// 已落地的媒体。归档只记路径，不下载——媒体在归档之前已由内核落地。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Media {
    Image { path: String, mime: String },
    File { path: String, mime: String },
}

impl Media {
    fn size(&self) -> usize {
        match self {
            Self::Image { path, mime } | Self::File { path, mime } => path.len() + mime.len(),
        }
    }

    /// 投递文本里的占位符。正文只给人看，媒体路径随块另发。
    fn placeholder(&self) -> &'static str {
        match self {
            Self::Image { .. } => "[图片]",
            Self::File { .. } => "[文件]",
        }
    }
}

/// 一条归档消息。
///
/// `seq` 由归档在 [`Archive::append`] 里分配，调用方传进来的值被忽略。其余字段是入口
/// 事件的形状：账号是**哪条连接看到的**（配置里的标识别名，不是 QQ 号），媒体只带已落地
/// 的路径。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    #[serde(default)]
    pub seq: u64,
    pub account: String,
    /// 会话的命名空间。旧数据没有这个字段时视为空——那时只有 QQ 一个平台。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub platform: String,
    /// 命名空间内的会话标识。对群聊平台就是群号。
    pub group: String,
    /// 回复该发到哪儿。`None` 表示就是本会话——多数平台如此。
    ///
    /// 它是**每条消息**的属性而不是会话的属性：Satori 上一个 guild 有多个文本频道，
    /// 回哪一条取决于这条消息从哪个频道来。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply: Option<String>,
    pub message_id: String,
    pub sender_id: String,
    /// 群昵称。显示数据，不参与任何判定。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender_name: Option<String>,
    pub text: String,
    /// 被 @ 或被引用。
    #[serde(default)]
    pub at_self: bool,
    /// 发送者属于设备声明的账号集合（不变量一）。
    #[serde(default)]
    pub own: bool,
    /// Unix 毫秒。
    #[serde(default)]
    pub at: i64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub media: Vec<Media>,
}

impl Message {
    /// 负载字节数。用于分片的内存预算，不是精确分配量。
    pub fn size(&self) -> usize {
        self.account.len()
            + self.platform.len()
            + self.group.len()
            + self.message_id.len()
            + self.sender_id.len()
            + self.sender_name.as_ref().map_or(0, String::len)
            + self.text.len()
            + self.media.iter().map(Media::size).sum::<usize>()
    }

    /// 这条消息属于哪个会话。
    pub fn conversation(&self) -> ConversationKey {
        ConversationKey::new(self.platform.clone(), self.group.clone())
    }

    /// 回复这条消息该用的地址。
    ///
    /// 优先用消息自己记的 `reply`（Satori 的频道号），没有就是会话本身。回退到会话是
    /// 安全的：群聊平台上两者本来就相等。
    pub fn reply_address(&self) -> ReplyAddress {
        ReplyAddress {
            platform: self.platform.clone(),
            address: self.reply.clone().unwrap_or_else(|| self.group.clone()),
        }
    }

    /// 消息的稳定标识是 `(会话, 消息 ID)`——同一条消息只按这两项认。
    ///
    /// 账号**不**参与。一个群里轮换的多个接收账号看到的是同一条消息；按账号分别归档，
    /// 一次轮换交接就会在未读里留下两条副本。账号只记录「哪条连接先看到」。
    ///
    /// 会话要连**平台**一起比：`qq` 群 `123` 与 `discord` 频道 `123` 是同名的两个会话，
    /// 只按群号认会把它们当成同一条消息。
    fn same_origin(&self, other: &Self) -> bool {
        self.platform == other.platform
            && self.group == other.group
            && self.message_id == other.message_id
    }
}

/// 一次追加的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Appended {
    /// 这条消息在分片里的序号。
    pub seq: u64,
    /// 是否新分配了序号。`false` 表示它已在保留窗口里，这次只是合并视角。
    pub fresh: bool,
}

/// 按记忆名分片的归档。
#[derive(Debug)]
pub struct Archive {
    shards: BTreeMap<String, Shard>,
    limits: Limits,
}

impl Archive {
    pub fn new(limits: Limits) -> Self {
        Self {
            shards: BTreeMap::new(),
            limits: limits.normalized(),
        }
    }

    /// 追加一条消息，返回它在分片里的序号与「是否新分配」。
    ///
    /// 同一个 `(群, 消息 ID)` 还在保留窗口里时不重复追加，返回原序号——上游重连后重发
    /// 同一条消息不该变成两条，轮换交接期间两个账号各看到一次也一样。重复到达时合并视角
    /// （`own` / `at_self` 取或）：同一个群的另一个账号看到的是同一条消息，只是「自己发的」
    /// 判定不同，合并后任一视角说是自己发的就算。被上限淘汰过的消息认不出来，会被当成新
    /// 消息，这是有界的代价。
    ///
    /// 第二个返回值是 `true` 表示这条是新消息。唤醒判定用它分岔：重复到达不该再判一次，
    /// 否则一次轮换交接会唤醒两次。
    pub fn append(&mut self, memory: &str, mut message: Message) -> Appended {
        let limits = self.limits;
        let shard = self.shards.entry(memory.to_owned()).or_default();

        if let Some(existing) = shard.find_mut(&message) {
            existing.own |= message.own;
            existing.at_self |= message.at_self;
            return Appended {
                seq: existing.seq,
                fresh: false,
            };
        }

        shard.next_seq += 1;
        message.seq = shard.next_seq;
        let seq = message.seq;
        shard.bytes += message.size();
        shard.messages.push_back(message);
        shard.enforce(&limits);
        Appended { seq, fresh: true }
    }

    /// 某个会话自上次投递以来的未读。
    ///
    /// 自己发的消息不在其中。同一条消息在一个分片里只有一条——见 `Message::same_origin`。
    pub fn unread(&self, memory: &str, conversation: &ConversationKey) -> Vec<&Message> {
        let Some(shard) = self.shards.get(memory) else {
            return Vec::new();
        };
        shard.unread(conversation).collect()
    }

    /// 某个会话最近 `limit` 条**已消费**的消息，供投递时补上下文。
    pub fn context(
        &self,
        memory: &str,
        conversation: &ConversationKey,
        limit: usize,
    ) -> Vec<&Message> {
        let Some(shard) = self.shards.get(memory) else {
            return Vec::new();
        };
        let mut out: Vec<&Message> = shard.seen(conversation).collect();
        let start = out.len().saturating_sub(limit);
        out.drain(..start);
        out
    }

    /// 把某个会话的水位推到 `through`：这之前的消息算被某个回合消费过。
    ///
    /// 水位只前进。回退等于把已消费的消息再投递一次，所以回退是调用方的错误，不是可忽略
    /// 的输入。
    pub fn consume(
        &mut self,
        memory: &str,
        conversation: &ConversationKey,
        through: u64,
    ) -> Result<(), WatermarkError> {
        let limits = self.limits;
        let Some(shard) = self.shards.get_mut(memory) else {
            return Err(WatermarkError::UnknownMemory);
        };

        let current = shard.watermark(conversation);
        if through <= current {
            return Err(WatermarkError::Backwards { current });
        }
        let latest = shard.latest_seq(conversation);
        if through > latest {
            return Err(WatermarkError::Ahead { latest });
        }

        shard.watermarks.insert(conversation.clone(), through);
        shard.enforce(&limits);
        Ok(())
    }

    /// 某个会话的水位。没有投递过就是 0。
    pub fn watermark(&self, memory: &str, conversation: &ConversationKey) -> u64 {
        self.shards
            .get(memory)
            .map_or(0, |shard| shard.watermark(conversation))
    }

    /// 某分片里因上限被丢弃的条数。诊断用，投递时据此说明有缺。
    pub fn dropped(&self, memory: &str) -> u64 {
        self.shards.get(memory).map_or(0, |shard| shard.dropped)
    }

    /// 某分片当前保留的消息条数（含上下文与未读）。
    pub fn retained(&self, memory: &str) -> usize {
        self.shards
            .get(memory)
            .map_or(0, |shard| shard.messages.len())
    }

    /// 某分片分配过的最大序号。
    pub fn latest_seq(&self, memory: &str) -> u64 {
        self.shards.get(memory).map_or(0, |shard| shard.next_seq)
    }

    /// 有分片的记忆名，按名字排序。
    pub fn memories(&self) -> impl Iterator<Item = &str> {
        self.shards.keys().map(String::as_str)
    }

    /// 序列化为一段 JSON。
    ///
    /// 只做序列化，不碰文件系统——落盘方式（SQLite 或文件）由内核持久化层决定，见
    /// `docs/architecture/03-kernel.md#持久化`。
    pub fn to_json(&self) -> Result<String, ArchiveError> {
        let wire = ArchiveWire {
            shards: self
                .shards
                .iter()
                .map(|(memory, shard)| (memory.clone(), ShardWire::from(shard)))
                .collect(),
        };
        serde_json::to_string_pretty(&wire)
            .map_err(|error| ArchiveError::Corrupt(error.to_string()))
    }

    /// 从 JSON 恢复。
    ///
    /// 读不出来就是错误，不返回空归档——「悄悄清空」会让部署者以为一切正常。
    /// 恢复后按当前上限重新收口，所以收紧上限不需要迁移文件。
    pub fn from_json(text: &str, limits: Limits) -> Result<Self, ArchiveError> {
        let wire: ArchiveWire = serde_json::from_str(text)
            .map_err(|error| ArchiveError::Corrupt(format!("解析失败：{error}")))?;

        let mut archive = Self::new(limits);
        for (memory, shard) in wire.shards {
            archive.shards.insert(memory, Shard::from_wire(shard)?);
        }
        for shard in archive.shards.values_mut() {
            shard.enforce(&archive.limits);
        }
        Ok(archive)
    }
}

/// 水位被拒绝的原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatermarkError {
    /// 这个记忆名没有归档。
    UnknownMemory,
    /// 想往回推水位。
    Backwards { current: u64 },
    /// 想推到还没分配的序号。
    Ahead { latest: u64 },
}

impl std::fmt::Display for WatermarkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownMemory => f.write_str("这个记忆名没有归档"),
            Self::Backwards { current } => write!(f, "水位只前进，当前是 {current}"),
            Self::Ahead { latest } => write!(f, "序号 {latest} 之后还没有消息"),
        }
    }
}

impl std::error::Error for WatermarkError {}

/// 归档读写失败的原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArchiveError {
    /// 存储内容读不出来。
    ///
    /// 不静默重置：归档丢了会丢消息，悄悄清空比报错更糟。
    Corrupt(String),
}

impl std::fmt::Display for ArchiveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Corrupt(message) => write!(f, "归档存储损坏：{message}"),
        }
    }
}

impl std::error::Error for ArchiveError {}

/// 一个记忆名的全部消息，以及每个会话的未读水位。
#[derive(Debug, Default)]
struct Shard {
    messages: VecDeque<Message>,
    bytes: usize,
    /// 会话 → 水位。消息的会话没有水位时视为 0。
    ///
    /// 键是 [`ConversationKey`] 而不是群号：`qq` 群 `123` 与 `discord` 频道 `123` 在同
    /// 一个记忆名下是两个会话，各有一条水位，否则一边投递就会把另一边的未读一起划掉。
    watermarks: BTreeMap<ConversationKey, u64>,
    dropped: u64,
    next_seq: u64,
}

impl Shard {
    fn find_mut(&mut self, message: &Message) -> Option<&mut Message> {
        self.messages
            .iter_mut()
            .find(|existing| existing.same_origin(message))
    }

    fn watermark(&self, conversation: &ConversationKey) -> u64 {
        self.watermarks.get(conversation).copied().unwrap_or(0)
    }

    fn latest_seq(&self, conversation: &ConversationKey) -> u64 {
        self.messages
            .iter()
            .filter(|message| &message.conversation() == conversation)
            .map(|message| message.seq)
            .max()
            .unwrap_or(0)
    }

    fn unread(&self, conversation: &ConversationKey) -> impl Iterator<Item = &Message> {
        let through = self.watermark(conversation);
        self.messages.iter().filter(move |message| {
            &message.conversation() == conversation && is_unread(message, through)
        })
    }

    fn seen(&self, conversation: &ConversationKey) -> impl Iterator<Item = &Message> {
        let through = self.watermark(conversation);
        self.messages.iter().filter(move |message| {
            &message.conversation() == conversation && !is_unread(message, through)
        })
    }

    /// 回收与收口。先丢已消费的上下文，再谈硬上限——顺序决定「先保未读」。
    fn enforce(&mut self, limits: &Limits) {
        let mut seen = self
            .messages
            .iter()
            .filter(|message| !is_unread(message, self.watermark(&message.conversation())))
            .count();
        while seen > limits.context_messages {
            let Some(front) = self.messages.front() else {
                break;
            };
            if is_unread(front, self.watermark(&front.conversation())) {
                // 最旧的还没被投递过，不能为了上下文丢掉它。
                break;
            }
            self.pop_front();
            seen -= 1;
        }

        while self.messages.len() > limits.max_messages || self.bytes > limits.max_bytes {
            if self.messages.is_empty() {
                break;
            }
            self.pop_front();
        }
    }

    fn pop_front(&mut self) {
        let Some(message) = self.messages.pop_front() else {
            return;
        };
        self.bytes = self.bytes.saturating_sub(message.size());
        if is_unread(&message, self.watermark(&message.conversation())) {
            self.dropped += 1;
        }
    }

    fn from_wire(wire: ShardWire) -> Result<Self, ArchiveError> {
        let mut shard = Self {
            messages: VecDeque::with_capacity(wire.messages.len()),
            bytes: 0,
            // 线上形态是数组：JSON 的 map 键只能是字符串，而会话键有两段。
            watermarks: wire
                .watermarks
                .into_iter()
                .map(|entry| {
                    (
                        ConversationKey::new(entry.platform, entry.conversation),
                        entry.through,
                    )
                })
                .collect(),
            dropped: wire.dropped,
            next_seq: 0,
        };

        let mut previous = 0;
        for message in wire.messages {
            if message.seq <= previous {
                return Err(ArchiveError::Corrupt(format!(
                    "序号必须严格递增，{} 出现在 {} 之后",
                    message.seq, previous
                )));
            }
            previous = message.seq;
            shard.bytes += message.size();
            shard.next_seq = message.seq;
            shard.messages.push_back(message);
        }

        // 水位指向已被淘汰的序号是合法的（那条消息被上限丢过）；指向未来不是。
        for (conversation, watermark) in &shard.watermarks {
            let latest = shard.latest_seq(conversation);
            if *watermark > latest && latest != 0 {
                return Err(ArchiveError::Corrupt(format!(
                    "会话 {} 的水位 {watermark} 超过最大序号 {latest}",
                    conversation.conversation
                )));
            }
        }

        Ok(shard)
    }
}

fn is_unread(message: &Message, through: u64) -> bool {
    message.seq > through && !message.own
}

/// 落盘形态：分片按记忆名拍平。
#[derive(Serialize, Deserialize)]
struct ArchiveWire {
    #[serde(default)]
    shards: BTreeMap<String, ShardWire>,
}

#[derive(Serialize, Deserialize)]
struct ShardWire {
    #[serde(default)]
    watermarks: Vec<WatermarkWire>,
    #[serde(default)]
    dropped: u64,
    #[serde(default)]
    messages: Vec<Message>,
}

/// 一条水位记录。
///
/// 不用 `{"qq/20001": 34}` 那种单键 map：JSON 的对象键只能是字符串，而会话键有两段，
/// 拼成字符串就退回到「分隔符会撞车」的老问题。数组形式两个字段各自独立，也没有歧义。
#[derive(Serialize, Deserialize)]
struct WatermarkWire {
    #[serde(default)]
    platform: String,
    conversation: String,
    through: u64,
}

impl From<&Shard> for ShardWire {
    fn from(shard: &Shard) -> Self {
        Self {
            watermarks: shard
                .watermarks
                .iter()
                .map(|(conversation, through)| WatermarkWire {
                    platform: conversation.platform.clone(),
                    conversation: conversation.conversation.clone(),
                    through: *through,
                })
                .collect(),
            dropped: shard.dropped,
            messages: shard.messages.iter().cloned().collect(),
        }
    }
}

/// 把一批消息渲染成投递文本。
///
/// 每行一条：`发送者号码（群昵称）：正文`。号码才是身份，括号里的东西随时能改，见
/// `docs/architecture/02-domain-model.md#群昵称不是身份`。
pub fn render(messages: &[&Message]) -> String {
    let mut out = String::new();
    for message in messages {
        if !out.is_empty() {
            out.push('\n');
        }
        if message.own {
            out.push('我');
        } else {
            out.push_str(&message.sender_id);
            if let Some(name) = &message.sender_name {
                out.push('（');
                out.push_str(name);
                out.push('）');
            }
        }
        out.push('：');
        if message.at_self && !message.own {
            out.push_str("@你 ");
        }
        out.push_str(&message.text);
        for media in &message.media {
            out.push_str(media.placeholder());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn archive() -> Archive {
        Archive::new(Limits::default())
    }

    /// 测试里的会话键。多数用例只有 QQ 一个平台，写成短形式省得每处都铺开。
    fn qq(conversation: &str) -> ConversationKey {
        ConversationKey::new("qq", conversation)
    }

    fn message(id: &str, sender: &str, text: &str) -> Message {
        Message {
            account: "main".to_owned(),
            platform: "qq".to_owned(),
            group: "20001".to_owned(),
            message_id: id.to_owned(),
            sender_id: sender.to_owned(),
            text: text.to_owned(),
            at: 1_792_561_540_000,
            ..Message::default()
        }
    }

    fn seqs(messages: Vec<&Message>) -> Vec<u64> {
        messages.iter().map(|message| message.seq).collect()
    }

    #[test]
    fn append_assigns_increasing_seq_per_shard() {
        let mut archive = archive();
        assert_eq!(archive.append("alice", message("1", "30002", "一")).seq, 1);
        assert_eq!(archive.append("alice", message("2", "30002", "二")).seq, 2);
        // 另一个记忆名从自己的 1 开始：序号是分片内的。
        assert_eq!(archive.append("bob", message("1", "30002", "一")).seq, 1);
        assert_eq!(archive.latest_seq("alice"), 2);
        assert_eq!(archive.retained("alice"), 2);
    }

    #[test]
    fn unread_starts_at_watermark() {
        let mut archive = archive();
        for (id, text) in [("1", "一"), ("2", "二"), ("3", "三")] {
            archive.append("alice", message(id, "30002", text));
        }
        assert_eq!(seqs(archive.unread("alice", &qq("20001"))), vec![1, 2, 3]);

        archive
            .consume("alice", &qq("20001"), 2)
            .expect("应能推进水位");
        assert_eq!(seqs(archive.unread("alice", &qq("20001"))), vec![3]);
        assert_eq!(archive.watermark("alice", &qq("20001")), 2);
    }

    #[test]
    fn own_message_is_archived_but_never_unread() {
        // 不变量一：自己账号发言写入归档，不进未读。
        let mut archive = archive();
        let mut own = message("1", "10001", "我自己说的");
        own.own = true;
        archive.append("alice", own);

        assert!(archive.unread("alice", &qq("20001")).is_empty());
        assert_eq!(archive.retained("alice"), 1, "它仍然在归档里");
        // 上下文里要有它，否则模型看不到自己刚说过什么。
        assert_eq!(seqs(archive.context("alice", &qq("20001"), 10)), vec![1]);
    }

    #[test]
    fn send_only_account_message_is_archived_once() {
        // 纯发送号不在 receive 里：它自己说的话照样进这台设备的记忆，只是不进未读。
        // 否则同一句话在不在归档里，会取决于当时哪条连接在线。
        let mut archive = archive();
        let mut from_relay = message("1", "10001", "我用发送号说的");
        from_relay.account = "relay".to_owned();
        from_relay.own = true;
        let Appended { seq, fresh } = archive.append("alice", from_relay);
        assert!(fresh, "发送号独占看到的消息是新消息");

        assert_eq!(archive.retained("alice"), 1, "发送号独占看到的消息也归档");
        assert!(
            archive.unread("alice", &qq("20001")).is_empty(),
            "own 不进未读，也就不触发唤醒"
        );

        // 接收号也看到了这一句：合并且仍算自己发的。
        let mut from_main = message("1", "10001", "我用发送号说的");
        from_main.own = true;
        let again = archive.append("alice", from_main);
        assert_eq!(again.seq, seq, "同一句只归档一份");
        assert!(!again.fresh, "重复到达不是新消息");
        assert_eq!(archive.retained("alice"), 1);
        assert!(archive.unread("alice", &qq("20001")).is_empty());
        assert_eq!(seqs(archive.context("alice", &qq("20001"), 10)), vec![seq]);
    }

    #[test]
    fn watermarks_are_per_group() {
        let mut archive = archive();
        archive.append("alice", message("1", "30002", "A 群一"));
        let mut elsewhere = message("2", "30002", "B 群一");
        elsewhere.group = "20003".to_owned();
        archive.append("alice", elsewhere);

        archive
            .consume("alice", &qq("20001"), 1)
            .expect("应能推进 A 群水位");
        // 推进 A 群不能划掉 B 群的未读。
        assert_eq!(seqs(archive.unread("alice", &qq("20003"))), vec![2]);
        assert_eq!(archive.watermark("alice", &qq("20001")), 1);
        assert_eq!(archive.watermark("alice", &qq("20003")), 0);
    }

    #[test]
    fn consume_backwards_is_rejected() {
        let mut archive = archive();
        archive.append("alice", message("1", "30002", "一"));
        archive.append("alice", message("2", "30002", "二"));
        archive.consume("alice", &qq("20001"), 2).expect("应能推进");

        assert_eq!(
            archive.consume("alice", &qq("20001"), 1),
            Err(WatermarkError::Backwards { current: 2 })
        );
    }

    #[test]
    fn consume_beyond_latest_is_rejected() {
        let mut archive = archive();
        archive.append("alice", message("1", "30002", "一"));
        assert_eq!(
            archive.consume("alice", &qq("20001"), 2),
            Err(WatermarkError::Ahead { latest: 1 })
        );
        // 群还没收到消息时推向未来同样非法。
        assert_eq!(
            archive.consume("alice", &qq("20003"), 1),
            Err(WatermarkError::Ahead { latest: 0 })
        );
    }

    #[test]
    fn consume_unknown_memory_is_rejected() {
        let mut archive = archive();
        assert_eq!(
            archive.consume("nobody", &qq("20001"), 1),
            Err(WatermarkError::UnknownMemory)
        );
    }

    #[test]
    fn same_origin_is_not_appended_twice() {
        let mut archive = archive();
        let first = archive.append("alice", message("1", "30002", "一"));
        // 上游重连后重发同一条：标识相同，不该变成两条。
        let again = archive.append("alice", message("1", "30002", "一"));
        assert_eq!(first.seq, again.seq);
        assert!(first.fresh && !again.fresh);
        assert_eq!(archive.retained("alice"), 1);
    }

    #[test]
    fn same_message_from_two_accounts_is_merged() {
        // 轮换的多个账号看到同一条消息：一条归档，视角取或。
        let mut archive = archive();
        let first = archive.append("alice", message("1", "30002", "一"));
        assert_eq!(
            archive.unread("alice", &qq("20001")).len(),
            1,
            "先到的那份不是自己发的"
        );

        let mut rotated = message("1", "30002", "一");
        rotated.account = "backup".to_owned();
        rotated.own = true;

        assert_eq!(archive.append("alice", rotated).seq, first.seq);
        assert_eq!(archive.retained("alice"), 1, "轮换看到同一条，只归档一份");
        assert!(
            archive.unread("alice", &qq("20001")).is_empty(),
            "own 取或：另一个账号视角认作自己发的，合并后不再进未读"
        );
    }

    #[test]
    fn memories_are_isolated() {
        let mut archive = archive();
        archive.append("alice", message("1", "30002", "给爱丽丝"));
        archive.append("bob", message("2", "30002", "给鲍勃"));

        archive.consume("alice", &qq("20001"), 1).expect("应能推进");
        assert!(archive.unread("alice", &qq("20001")).is_empty());
        assert_eq!(seqs(archive.unread("bob", &qq("20001"))), vec![1]);
        assert_eq!(archive.memories().collect::<Vec<_>>(), vec!["alice", "bob"]);
    }

    #[test]
    fn context_keeps_only_recent_window() {
        let mut archive = Archive::new(Limits {
            context_messages: 2,
            ..Limits::default()
        });
        for id in ["1", "2", "3", "4", "5"] {
            archive.append("alice", message(id, "30002", "x"));
        }
        archive.consume("alice", &qq("20001"), 5).expect("应能推进");

        // 已消费的只留最近两条。
        assert_eq!(seqs(archive.context("alice", &qq("20001"), 10)), vec![4, 5]);
        assert_eq!(archive.retained("alice"), 2);
    }

    #[test]
    fn hard_message_cap_drops_oldest_unread_and_counts() {
        let mut archive = Archive::new(Limits {
            max_messages: 3,
            ..Limits::default()
        });
        for id in ["1", "2", "3", "4", "5"] {
            archive.append("alice", message(id, "30002", "x"));
        }

        // 一条都没投递过，硬上限还是生效——但丢弃要留痕。
        assert_eq!(archive.retained("alice"), 3);
        assert_eq!(archive.dropped("alice"), 2);
        assert_eq!(seqs(archive.unread("alice", &qq("20001"))), vec![3, 4, 5]);
    }

    #[test]
    fn byte_cap_is_enforced() {
        let limits = Limits {
            max_messages: 100,
            max_bytes: 200,
            context_messages: 50,
        };
        let mut archive = Archive::new(limits);
        let text = "长".repeat(60); // 每条约 180 字节
        for id in ["1", "2", "3", "4"] {
            archive.append("alice", message(id, "30002", &text));
        }

        assert!(archive.retained("alice") < 4);
        assert!(archive.retained("alice") >= 1, "至少留一条，不能清空");
        assert!(archive.dropped("alice") >= 1);
    }

    #[test]
    fn json_round_trip_preserves_watermark_and_dropped() {
        let mut archive = Archive::new(Limits {
            max_messages: 5,
            ..Limits::default()
        });
        for id in ["1", "2", "3", "4", "5", "6"] {
            archive.append("alice", message(id, "30002", "x"));
        }
        archive.append("bob", message("9", "30002", "y"));
        archive.consume("alice", &qq("20001"), 6).expect("应能推进");

        let json = archive.to_json().expect("应能序列化");
        let restored = Archive::from_json(&json, Limits::default()).expect("应能再解析");

        assert_eq!(restored.watermark("alice", &qq("20001")), 6);
        assert_eq!(restored.dropped("alice"), archive.dropped("alice"));
        assert_eq!(restored.latest_seq("alice"), archive.latest_seq("alice"));
        assert_eq!(restored.retained("bob"), 1);
        assert_eq!(restored.to_json().expect("再序列化"), json, "往返应稳定");
    }

    #[test]
    fn json_rejects_out_of_order_seqs() {
        let json = r#"{
            "shards": {
                "alice": {
                    "messages": [
                        {"seq": 5, "account": "main", "group": "20001", "message_id": "1", "sender_id": "2", "text": "x"},
                        {"seq": 3, "account": "main", "group": "20001", "message_id": "2", "sender_id": "2", "text": "y"}
                    ]
                }
            }
        }"#;
        assert!(matches!(
            Archive::from_json(json, Limits::default()),
            Err(ArchiveError::Corrupt(_))
        ));
    }

    #[test]
    fn render_writes_one_line_per_message() {
        let mut first = message("1", "30002", "在吗");
        first.sender_name = Some("小明".to_owned());
        first.at_self = true;
        let mut second = message("2", "10001", "在");
        second.own = true;

        let mut third = message("3", "30003", "看图");
        third.media.push(Media::Image {
            path: "/tmp/a.jpg".to_owned(),
            mime: "image/jpeg".to_owned(),
        });

        let rendered = render(&[&first, &second, &third]);
        assert_eq!(
            rendered,
            "30002（小明）：@你 在吗\n我：在\n30003：看图[图片]"
        );
    }

    #[test]
    fn render_of_empty_batch_is_empty() {
        assert_eq!(render(&[]), "");
    }

    #[test]
    fn same_conversation_id_on_two_platforms_has_two_watermarks() {
        // `qq` 群 123 与 `discord` 频道 123 同名而不同。只按群号做水位键，
        // 推进一边就会把另一边的未读一起划掉——静默丢消息。
        let mut archive = archive();
        let mut qq_message = message("1", "30002", "QQ 群里的");
        qq_message.group = "123".to_owned();
        let mut discord_message = message("1", "30002", "Discord 频道里的");
        discord_message.platform = "discord".to_owned();
        discord_message.group = "123".to_owned();

        let qq = archive.append("alice", qq_message);
        let discord = archive.append("alice", discord_message);

        let qq_key = ConversationKey::new("qq", "123");
        let discord_key = ConversationKey::new("discord", "123");

        assert_eq!(archive.unread("alice", &qq_key).len(), 1);
        assert_eq!(archive.unread("alice", &discord_key).len(), 1);

        // 推进 QQ 的水位。
        archive.consume("alice", &qq_key, qq.seq).expect("应能推进");

        assert!(archive.unread("alice", &qq_key).is_empty(), "QQ 已消费");
        assert_eq!(
            archive.unread("alice", &discord_key).len(),
            1,
            "Discord 的未读不能被 QQ 的水位划掉"
        );
        assert_eq!(archive.watermark("alice", &discord_key), 0);
        assert_eq!(discord.seq, 2, "两条都在同一个分片里，序号各自分配");
    }

    #[test]
    fn the_same_message_id_on_two_platforms_is_two_messages() {
        // 消息标识按 (会话, 消息 ID) 认，会话含平台。若只按消息 ID 合并，
        // 两条不同平台的消息会被当成同一条，其中一条永远进不了未读。
        let mut archive = archive();
        let mut qq_message = message("same-id", "30002", "QQ 的");
        qq_message.group = "123".to_owned();
        let mut discord_message = message("same-id", "30002", "Discord 的");
        discord_message.platform = "discord".to_owned();
        discord_message.group = "123".to_owned();

        let first = archive.append("alice", qq_message);
        let second = archive.append("alice", discord_message);

        assert!(first.fresh, "第一条是新的");
        assert!(second.fresh, "不同平台的同号消息也是新的，不是重复");
        assert_ne!(first.seq, second.seq);
    }

    #[test]
    fn reply_address_falls_back_to_the_conversation() {
        let mut plain = message("1", "30002", "x");
        plain.platform = "qq".to_owned();
        plain.group = "123".to_owned();
        assert_eq!(
            plain.reply_address(),
            ReplyAddress {
                platform: "qq".to_owned(),
                address: "123".to_owned(),
            },
            "多数平台上回信目标就是会话本身"
        );

        let mut satori = message("2", "30002", "y");
        satori.platform = "satori".to_owned();
        satori.group = "guild-1".to_owned();
        satori.reply = Some("channel-9".to_owned());
        assert_eq!(
            satori.reply_address(),
            ReplyAddress {
                platform: "satori".to_owned(),
                address: "channel-9".to_owned(),
            },
            "Satori 上回信道频道，不是 guild"
        );
    }
}
