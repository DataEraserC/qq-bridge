//! 回合调度：订阅 → 唤醒 → 投递 → 工具 → 确认。
//!
//! 一条入站记录进来，走的是同一条路（ADR-0005）：
//! 1. 写流水——永远发生，不管唤不唤醒；
//! 2. 对每个订阅了这个房间的会话，按它的唤醒策略判定要不要叫醒；
//! 3. 要叫醒就排进队列，**该会话**闲下来就开一轮，把未读打包投给 harness。
//!
//! 串行的单位是**会话**（harness 名 + 会话 id，ADR-0017）：同一会话同一时刻只有一轮，
//! 不同会话各自并发。记忆不再当串行锁——一个会话引用多份记忆、多份记忆可被多个会话
//! 共享，最终一致，互不排队。
//!
//! 一个 harness 连上来时声明「我是谁、接管哪些会话」。`send_message_to` 的可发目标 =
//! 当前会话订阅的所有房间；**跨会话发送被砍掉**——一个会话不能替别的人格发言。

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::time::{SystemTime, UNIX_EPOCH};

use acb_proto::element::flatten_text;
use acb_proto::{
    Delivery, Element, ErrorCode, MemoryInfo, Record, RecordKind, RoomId, Sender, Ticket,
    ToolError, TurnReason,
};
use serde::Deserialize;
use serde_json::{Value, json};
use thiserror::Error;

use crate::config::{Config, Session, WakeDecision, WakeInput, WakePolicy};
use crate::memory::{
    MemoryRepo, git_add, git_blame, git_branch, git_commit, git_diff, git_log, git_push,
    git_status, list_dir, read_file, search, write_file,
};
use crate::outbound::{Outbound, OutboundError};
use crate::storage::{
    Cursor, MIN_KEPT, Note, NoteKey, NoteKind, PrunePlan, RecordDraft, StorageError, Store,
    TopicStatus, unread,
};
use crate::tools::{
    AckArgs, DelNoteArgs, EventsArgs, HistoryArgs, MemoryAddArgs, MemoryArgs, MemoryBlameArgs,
    MemoryCommitArgs, MemoryLogArgs, MemorySearchArgs, MemoryWriteArgs, NotesArgs, SendArgs,
    SendToArgs, SetNoteArgs, is_known,
};

/// 一轮最多投多少条。剩下的下一轮再投，免得一次塞爆上下文。
pub const MAX_DELIVERY: usize = 200;

/// 查历史时的默认条数与上限。
const DEFAULT_HISTORY: usize = 20;
const MAX_HISTORY: usize = 200;

/// 调度层错误：存储坏了，或者拿了一张不存在的票。
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum BridgeError {
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error("这张票无效或本轮已结束：{0}")]
    UnknownTicket(String),
}

/// 一条入站记录的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ingest {
    /// 记下来了。`woke` 是被这条叫醒的会话（按配置顺序）。
    Recorded { id: u64, woke: Vec<String> },
    /// 没有任何会话订阅这个房间——什么都不做，连流水都不写。
    NotSubscribed { room: RoomId },
}

/// 一次清理里某个房间的战果，给日志用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pruned {
    pub room: RoomId,
    /// 删掉多少条。
    pub deleted: u64,
    /// 还剩多少条。
    pub kept: u64,
}

/// 等着开轮次的（会话，房间）。
#[derive(Debug, Clone)]
struct Pending {
    session: String,
    room: RoomId,
    reason: TurnReason,
}

/// 正在跑的一轮。挂在会话上：一个会话同一时刻只有这一轮。
#[derive(Debug, Clone)]
struct ActiveTurn {
    /// 这一轮替哪个会话说话（决定水位、决定默认发到哪个身份）。
    session: String,
    room: RoomId,
    /// 本轮投出去的编号。只有这些能被 `ack_messages` 确认。
    delivered: BTreeSet<u64>,
}

/// 某个房间的一个订阅者，判定唤醒时要的东西。
#[derive(Debug, Clone)]
struct Subscriber {
    session: String,
    /// 这条连接登录的账号：用来认出「这句是我自己说的」。
    account: String,
    wake: WakePolicy,
}

/// 桥的核心：配置 + 存储 + 出站，加上调度状态。
pub struct Bridge<S: Store> {
    config: Config,
    store: S,
    outbound: Box<dyn Outbound>,
    pending: VecDeque<Pending>,
    /// 键是会话 id：一个会话同时只跑一轮。
    active: HashMap<String, ActiveTurn>,
    /// 票 → 会话。票是开轮时签发的，收工作废。
    by_ticket: HashMap<Ticket, String>,
    ticket_seq: u64,
    /// 记忆仓库管理器：负责裸仓库 init 与 worktree checkout。
    memory: MemoryRepo,
}

impl<S: Store> Bridge<S> {
    pub fn new(config: Config, store: S, outbound: Box<dyn Outbound>) -> Self {
        let memory = MemoryRepo::new(config.worktrees_root.clone());
        Self {
            config,
            store,
            outbound,
            pending: VecDeque::new(),
            active: HashMap::new(),
            by_ticket: HashMap::new(),
            ticket_seq: 0,
            memory,
        }
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// 只读看存储，给管理页和测试用。
    pub fn store(&self) -> &S {
        &self.store
    }

    /// 哪几个会话正跑着轮次。
    pub fn busy_sessions(&self) -> Vec<String> {
        let mut sessions: Vec<String> = self.active.keys().cloned().collect();
        sessions.sort();
        sessions
    }

    /// 队列里还压着几个待开轮次（给日志看，没人接的时候提醒用）。
    pub fn queued_turns(&self) -> usize {
        self.pending.len()
    }

    /// 清一次旧流水，返回真的删了东西的房间。
    ///
    /// `now` 是「现在」的 Unix 毫秒——传进来而不是自己取，是为了让测试能造出「很久以前」。
    ///
    /// 三条规矩：
    /// 1. **只删读过的**：安全线取所有在册订阅者水位的最小值。有一个设备一条都没读到过，
    ///    这个房间这次就一条都不删——未读是投递的原料，删了就是真丢了。
    /// 2. **只删还在订阅的房间**：没人订的房间不归这里管，判据（谁在读）也不成立了。
    /// 3. **每房间至少留一条**：编号靠 `MAX+1` 推，删光了下一个又回到 1，而水位还在几百，
    ///    新消息会被当成已读直接消失（[`MIN_KEPT`]）。
    pub fn prune(&mut self, now: i64) -> Result<Vec<Pruned>, BridgeError> {
        let retention = self.config.retention;
        if !retention.enabled() {
            return Ok(Vec::new());
        }

        let older_than = (retention.max_age_days > 0).then(|| {
            now.saturating_sub((retention.max_age_days as i64).saturating_mul(86_400_000))
        });

        let mut report = Vec::new();
        for room in self.store.rooms()? {
            let sessions: Vec<String> = self
                .config
                .subscribers_of(&room)
                .into_iter()
                .map(|session| session.id.clone())
                .collect();
            if sessions.is_empty() {
                continue;
            }

            // 安全线：所有在册会话都已读到的那个位置。初值是「一条都没删」的边界。
            let mut below = self.store.latest(&room)?.map_or(0, |latest| latest + 1);
            for session in sessions {
                let cursor = Cursor::new(session, room.clone());
                below = below.min(self.store.watermark(&cursor)?);
            }
            if below == 0 {
                continue;
            }

            let before = self.store.count(&room)?;
            let deleted = self.store.prune(
                &room,
                &PrunePlan {
                    below,
                    keep_newest: retention.max_records_per_room.max(MIN_KEPT),
                    older_than,
                },
            )?;
            if deleted > 0 {
                report.push(Pruned {
                    room: room.clone(),
                    deleted,
                    kept: before - deleted,
                });
            }
        }
        Ok(report)
    }

    /// 一条入站记录进来。
    pub fn ingest(&mut self, draft: RecordDraft) -> Result<Ingest, BridgeError> {
        let room = draft.room.clone();
        let subscribers = self.subscribers_of(&room);

        if subscribers.is_empty() {
            return Ok(Ingest::NotSubscribed { room });
        }

        // 「引用的是不是我发的」要查库，所以入库前先判。判据是本房间那条连接的登录账号——
        // 被引用的那条如果是这个号发的，这条就算冲我来的（WakePolicy::reply）。
        let account = self.config.account_of_room(&room);
        let reply_to_self = self.quotes_own_message(&draft, &account)?;
        let draft = draft.with_reply_to_self(reply_to_self);

        let record = self.store.append(draft)?;
        let text = flatten_text(&record.elements);
        let event = match &record.kind {
            RecordKind::Event { event } => Some(event.as_str()),
            RecordKind::Message => None,
        };

        let mut woke = Vec::new();
        for subscriber in subscribers {
            let cursor = Cursor::new(subscriber.session.clone(), room.clone());

            // 平台把我们自己说的话回传回来：记下来，但不是未读，也不叫醒自己。
            if record.sender.id == subscriber.account {
                self.store.ack(&cursor, &[record.id])?;
                continue;
            }

            let pending = self
                .pending_records(&subscriber.session, &room, &subscriber.account)?
                .len() as u32;
            let input = WakeInput {
                sender_id: &record.sender.id,
                text: &text,
                at_self: record.at_self,
                reply_to_self: record.reply_to_self,
                event,
                unread: pending,
            };
            if subscriber.wake.decide(&input) == WakeDecision::Wake {
                woke.push(subscriber.session.clone());
                self.enqueue(subscriber.session, room.clone(), TurnReason::Wake);
            }
        }

        Ok(Ingest::Recorded {
            id: record.id,
            woke,
        })
    }

    /// 这条消息里有没有「引用本合同一个账号发过的消息」。
    ///
    /// 查不到被引用那条（被清理了、对面没给编号）就当不是——宁可少叫醒一次，
    /// 也不要凭编号长相猜。平台把自己的消息编号给全了，这件事才判得准。
    fn quotes_own_message(&self, draft: &RecordDraft, account: &str) -> Result<bool, StorageError> {
        if account.is_empty() {
            return Ok(false);
        }
        for element in &draft.elements {
            let Element::Quote { message_id } = element else {
                continue;
            };
            let Some(sender) = self.store.sender_of(&draft.room, message_id)? else {
                continue;
            };
            if sender.id == account {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// 取下一轮，不管是谁的。没有就返回 `None`。
    pub fn poll_turn(&mut self) -> Result<Option<Delivery>, BridgeError> {
        let index = (0..self.pending.len())
            .find(|&index| !self.active.contains_key(&self.pending[index].session));
        let Some(index) = index else {
            return Ok(None);
        };
        let pending = self.pending.remove(index).expect("刚刚找到的");
        self.open_turn(pending).map(Some)
    }

    /// 只给这一组会话取活。接入点用这个——一条连接只代表它自己那组会话。
    pub fn poll_turn_for(&mut self, sessions: &[String]) -> Result<Option<Delivery>, BridgeError> {
        let index = (0..self.pending.len()).find(|&index| {
            let pending = &self.pending[index];
            if !sessions.iter().any(|session| session == &pending.session) {
                return false;
            }
            !self.active.contains_key(&pending.session)
        });
        let Some(index) = index else {
            return Ok(None);
        };
        let pending = self.pending.remove(index).expect("刚刚找到的");
        self.open_turn(pending).map(Some)
    }

    /// 一轮收工。票作废，会话空出来。
    ///
    /// **不推水位**：没确认的消息仍然算未读，下一轮接着投。
    pub fn end_turn(&mut self, ticket: &Ticket) -> Result<(), BridgeError> {
        let Some(session) = self.by_ticket.remove(ticket) else {
            return Err(BridgeError::UnknownTicket(ticket.to_string()));
        };
        self.active.remove(&session);
        Ok(())
    }

    /// 调工具。
    ///
    /// `sessions` 是这次接入代表的会话组（同一 harness）。发到哪儿、读哪儿的判据都从这里来：
    /// 能碰的房间 = 当前会话订阅的房间，越界就拒。跨会话发送被砍掉（ADR-0017）。
    pub fn tool_call(
        &mut self,
        ticket: &Ticket,
        sessions: &[String],
        tool: &str,
        args: Value,
    ) -> Result<Value, ToolError> {
        let session = self
            .by_ticket
            .get(ticket)
            .cloned()
            .ok_or_else(|| ToolError::new(ErrorCode::Unauthorized, "这张票无效或本轮已结束"))?;
        let (turn_session, room, delivered) = {
            let turn = self.active.get(&session).expect("有票就一定有活跃轮次");
            (
                turn.session.clone(),
                turn.room.clone(),
                turn.delivered.clone(),
            )
        };

        if !is_known(tool) {
            return Err(ToolError::new(
                ErrorCode::ToolNotAllowed,
                format!("没有这个工具：{tool}"),
            ));
        }

        match tool {
            // 不给目标：接着说当前这一轮的房间，用本轮的身份。
            "send_message" => {
                let args: SendArgs = parse(args)?;
                self.deliver_out(&turn_session, &room, args.elements)
            }
            // 给了目标：只能落在当前会话订阅的房间范围内（跨会话发送已砍）。
            "send_message_to" => {
                let args: SendToArgs = parse(args)?;
                let target = resolve_room(&args.to)?;
                self.ensure_session_subscribes(&turn_session, &target)?;
                self.deliver_out(&turn_session, &target, args.elements)
            }
            "ack_messages" => {
                let args: AckArgs = parse(args)?;
                self.ack(&turn_session, &room, &delivered, args.ids)
            }
            "get_history" => {
                let args: HistoryArgs = parse(args)?;
                self.history(sessions, &room, args)
            }
            "list_rooms" => self.list_rooms(sessions),
            "get_notes" => {
                let args: NotesArgs = parse(args)?;
                self.read_notes(sessions, &room, args)
            }
            "set_note" => {
                let args: SetNoteArgs = parse(args)?;
                self.write_note(sessions, &room, args)
            }
            "del_note" => {
                let args: DelNoteArgs = parse(args)?;
                self.delete_note(sessions, &room, args)
            }
            "get_platform_events" => {
                let args: EventsArgs = parse(args)?;
                self.events(sessions, &room, args)
            }
            // 记忆：文件层。
            "memory_read" => {
                let args: MemoryArgs = parse(args)?;
                self.memory_read(&turn_session, args)
            }
            "memory_write" => {
                let args: MemoryWriteArgs = parse(args)?;
                self.memory_write(&turn_session, args)
            }
            "memory_list" => {
                let args: MemoryArgs = parse(args)?;
                self.memory_list(&turn_session, args)
            }
            "memory_search" => {
                let args: MemorySearchArgs = parse(args)?;
                self.memory_search(&turn_session, args)
            }
            // 记忆：git 层。
            "memory_git_status" => {
                let args: MemoryArgs = parse(args)?;
                self.memory_git_status(&turn_session, args)
            }
            "memory_git_log" => {
                let args: MemoryLogArgs = parse(args)?;
                self.memory_git_log(&turn_session, args)
            }
            "memory_git_diff" => {
                let args: MemoryArgs = parse(args)?;
                self.memory_git_diff(&turn_session, args)
            }
            "memory_git_blame" => {
                let args: MemoryBlameArgs = parse(args)?;
                self.memory_git_blame(&turn_session, args)
            }
            "memory_git_add" => {
                let args: MemoryAddArgs = parse(args)?;
                self.memory_git_add(&turn_session, args)
            }
            "memory_git_commit" => {
                let args: MemoryCommitArgs = parse(args)?;
                self.memory_git_commit(&turn_session, args)
            }
            "memory_git_push" => {
                let args: MemoryArgs = parse(args)?;
                self.memory_git_push(&turn_session, args)
            }
            "memory_git_branch" => {
                let args: MemoryArgs = parse(args)?;
                self.memory_git_branch(&turn_session, args)
            }
            other => Err(ToolError::new(
                ErrorCode::ToolNotAllowed,
                format!("没有这个工具：{other}"),
            )),
        }
    }

    // ---- 内部 ----

    /// 这个会话在这个房间里真正要处理的未读：水位之后、且不是自己说的。
    fn pending_records(
        &self,
        session: &str,
        room: &RoomId,
        account: &str,
    ) -> Result<Vec<Record>, StorageError> {
        let cursor = Cursor::new(session, room.clone());
        let mut records = unread(&self.store, &cursor, MAX_DELIVERY)?;
        records.retain(|record| record.sender.id != account);
        Ok(records)
    }

    fn subscribers_of(&self, room: &RoomId) -> Vec<Subscriber> {
        let account = self.config.account_of_room(room);
        self.config
            .sessions
            .iter()
            .filter_map(|session| {
                session
                    .subscriptions
                    .iter()
                    .find(|subscription| &subscription.room == room)
                    .map(|subscription| Subscriber {
                        session: session.id.clone(),
                        account: account.clone(),
                        wake: subscription.wake.clone(),
                    })
            })
            .collect()
    }

    /// 这一组里，谁订了这个房间。
    fn sessions_subscribing(&self, sessions: &[String], room: &RoomId) -> Vec<&Session> {
        self.config
            .sessions
            .iter()
            .filter(|session| sessions.iter().any(|id| id == &session.id))
            .filter(|session| {
                session
                    .subscriptions
                    .iter()
                    .any(|subscription| &subscription.room == room)
            })
            .collect()
    }

    /// 当前会话是否订阅了这个房间。没订阅就拒——这是 `send_message_to` 的边界。
    fn ensure_session_subscribes(&self, session: &str, room: &RoomId) -> Result<(), ToolError> {
        let subscribed = self.config.session(session).is_some_and(|session| {
            session
                .subscriptions
                .iter()
                .any(|subscription| &subscription.room == room)
        });
        if subscribed {
            Ok(())
        } else {
            Err(ToolError::new(
                ErrorCode::RoomNotSubscribed,
                format!("会话 {session} 没有订阅 {room}；只能发到本会话订阅的房间"),
            ))
        }
    }

    /// 读的工具只要求「本组有人订了这个房间」。
    fn ensure_readable(&self, sessions: &[String], room: &RoomId) -> Result<(), ToolError> {
        if self.sessions_subscribing(sessions, room).is_empty() {
            return Err(ToolError::new(
                ErrorCode::RoomNotSubscribed,
                format!("你这组会话都没订阅 {room}"),
            ));
        }
        Ok(())
    }

    fn enqueue(&mut self, session: String, room: RoomId, reason: TurnReason) {
        let already = self
            .pending
            .iter()
            .any(|pending| pending.session == session && pending.room == room);
        if already {
            return;
        }
        self.pending.push_back(Pending {
            session,
            room,
            reason,
        });
    }

    fn open_turn(&mut self, pending: Pending) -> Result<Delivery, BridgeError> {
        let account = self.config.account_of_room(&pending.room);
        let mut records = self.pending_records(&pending.session, &pending.room, &account)?;

        let mut delivered = BTreeSet::new();
        let cursor = Cursor::new(pending.session.clone(), pending.room.clone());
        for record in &mut records {
            // 计数落在存储里：重启之后同一个 N 接着数，不然看着像从没投过。
            let count = self.store.count_delivery(&cursor, record.id)?;
            if count > 1 {
                record.redelivery = Some(count);
                // 「第 N 次投递」和「怎么让它别再重投」必须跟着记录一起到模型眼前。
                // 这件事放在桥里做，不指望每个 harness 都记得补：harness 的职责只是
                // 把 records 塞进上下文，多一句话都不该由它发明（ADR-0010）。
                //
                // 注意写在 `note` 上，不塞进 elements：elements 是发送者说的话，
                // 系统的旁白混进去会让人（和模型）以为是对方打的字。
                record.note = Some(format!(
                    "第 {} 次投递；调 ack_messages 确认 #{} 之后就不再重复投递",
                    count, record.id
                ));
            }
            delivered.insert(record.id);
        }

        self.ticket_seq += 1;
        let ticket =
            Ticket::new(format!("{}-{}", pending.session, self.ticket_seq)).expect("票面非空");

        self.by_ticket
            .insert(ticket.clone(), pending.session.clone());
        self.active.insert(
            pending.session.clone(),
            ActiveTurn {
                session: pending.session.clone(),
                room: pending.room.clone(),
                delivered,
            },
        );

        // 把这个会话引用的记忆连同 worktree 路径一起带给 harness。惰性初始化：worktree
        // 第一次用到才 checkout，之后复用。
        let memories = self.session_memories(&pending.session);

        Ok(Delivery {
            ticket,
            session: pending.session,
            room: pending.room,
            records,
            reason: pending.reason,
            memories,
        })
    }

    /// 会话引用的记忆列表（id + worktree 路径）。worktree 惰性 checkout，失败这一轮就
    /// 开不成——记忆配错了要早点暴露，别让 harness 拿到一个碰不了的路径。
    fn session_memories(&self, session: &str) -> Vec<MemoryInfo> {
        let Some(session_cfg) = self.config.session(session) else {
            return Vec::new();
        };
        session_cfg
            .memories
            .iter()
            .filter_map(|memory_id| {
                let memory = self.config.memory(memory_id)?;
                let worktree = self
                    .memory
                    .ensure_worktree(&memory.location, memory_id, session)
                    .ok()?;
                Some(MemoryInfo {
                    id: memory_id.clone(),
                    worktree: worktree.display().to_string(),
                })
            })
            .collect()
    }

    fn deliver_out(
        &mut self,
        session: &str,
        room: &RoomId,
        elements: Vec<Element>,
    ) -> Result<Value, ToolError> {
        if elements.is_empty() {
            return Err(ToolError::new(ErrorCode::BadRequest, "没有内容可发"));
        }
        let account = self.config.account_of_room(room);
        if account.is_empty() {
            return Err(ToolError::new(
                ErrorCode::Internal,
                format!("{room} 所属的连接没有配登录账号，不知道自己是谁"),
            ));
        }

        let sent = self
            .outbound
            .send(room, &elements)
            .map_err(outbound_error)?;

        let record = self
            .store
            .append(
                RecordDraft::message(
                    room.clone(),
                    Sender {
                        id: account.clone(),
                        name: account,
                    },
                    sent.at,
                    elements,
                    false,
                )
                // 记下平台编号：下次有人引用这条，桥才反查得出「是我的话」。
                .with_platform_id(sent.platform_id.clone()),
            )
            .map_err(internal)?;

        // 自己说的话不是未读：写进流水的同时确认掉。
        let cursor = Cursor::new(session, room.clone());
        let watermark = self.store.ack(&cursor, &[record.id]).map_err(internal)?;

        Ok(json!({
            "id": record.id,
            "platform_id": sent.platform_id,
            "watermark": watermark,
        }))
    }

    fn ack(
        &mut self,
        session: &str,
        room: &RoomId,
        delivered: &BTreeSet<u64>,
        ids: Vec<u64>,
    ) -> Result<Value, ToolError> {
        if ids.is_empty() {
            return Err(ToolError::new(ErrorCode::BadRequest, "ids 不能为空"));
        }
        for id in &ids {
            if !delivered.contains(id) {
                return Err(ToolError::new(
                    ErrorCode::BadRequest,
                    format!(
                        "编号 {id} 不是本轮投递给你的记录，不能确认；\
                         先把本轮投给你的都处理完再确认"
                    ),
                ));
            }
        }

        let cursor = Cursor::new(session, room.clone());
        let watermark = self.store.ack(&cursor, &ids).map_err(internal)?;
        for id in &ids {
            self.store.forget_delivery(&cursor, *id).map_err(internal)?;
        }

        Ok(json!({ "watermark": watermark }))
    }

    fn history(
        &self,
        sessions: &[String],
        turn_room: &RoomId,
        args: HistoryArgs,
    ) -> Result<Value, ToolError> {
        let room = self.resolve(&args.room, turn_room)?;
        self.ensure_readable(sessions, &room)?;

        let limit = args.limit.unwrap_or(DEFAULT_HISTORY).min(MAX_HISTORY);
        let mut records: Vec<Record> = self
            .store
            .since(&room, 0, usize::MAX)
            .map_err(internal)?
            .into_iter()
            .filter(|record| args.before.is_none_or(|before| record.id < before))
            .collect();

        let skip = records.len().saturating_sub(limit);
        records.drain(..skip);
        Ok(json!({ "room": room, "records": records }))
    }

    /// 这组会话订阅的所有房间，按（会话，房间）各列一行。
    ///
    /// 两个会话订同一个房间时会出现两行——这是实话：水位本来就是各记各的。
    fn list_rooms(&self, sessions: &[String]) -> Result<Value, ToolError> {
        let mut rooms = Vec::new();
        for id in sessions {
            let Some(session) = self.config.session(id) else {
                continue;
            };
            for subscription in &session.subscriptions {
                let room = &subscription.room;
                let account = self.config.account_of_room(room);
                let cursor = Cursor::new(id.clone(), room.clone());
                let watermark = self.store.watermark(&cursor).map_err(internal)?;
                let unread = self
                    .pending_records(id, room, &account)
                    .map_err(internal)?
                    .len();
                rooms.push(json!({
                    "room": room,
                    "session": id,
                    "watermark": watermark,
                    "unread": unread,
                }));
            }
        }
        Ok(json!({ "rooms": rooms }))
    }

    fn read_notes(
        &self,
        sessions: &[String],
        turn_room: &RoomId,
        args: NotesArgs,
    ) -> Result<Value, ToolError> {
        let room = self.resolve(&args.room, turn_room)?;
        self.ensure_readable(sessions, &room)?;

        let kind = match args.kind.as_deref() {
            Some(text) => Some(parse_note_kind(text)?),
            None => None,
        };
        let notes: Vec<Note> = self
            .store
            .notes_in(&room)
            .map_err(internal)?
            .into_iter()
            .filter(|note| kind.is_none_or(|wanted| note.key.kind == wanted))
            .collect();

        Ok(json!({ "room": room, "notes": notes.iter().map(note_json).collect::<Vec<_>>() }))
    }

    fn write_note(
        &mut self,
        sessions: &[String],
        turn_room: &RoomId,
        args: SetNoteArgs,
    ) -> Result<Value, ToolError> {
        let room = self.resolve(&args.room, turn_room)?;
        self.ensure_readable(sessions, &room)?;

        let kind = match args.kind.as_deref() {
            Some(text) => parse_note_kind(text)?,
            None => NoteKind::Term,
        };
        let status = match args.status.as_deref() {
            Some("open") => Some(TopicStatus::Open),
            Some("closed") => Some(TopicStatus::Closed),
            Some(other) => {
                return Err(ToolError::new(
                    ErrorCode::BadRequest,
                    format!("status 只能是 open 或 closed，收到 {other}"),
                ));
            }
            None => None,
        };
        if status.is_some() && kind != NoteKind::Topic {
            return Err(ToolError::new(
                ErrorCode::BadRequest,
                "status 只对 topic 有用",
            ));
        }

        let key = NoteKey::new(room.clone(), kind, args.key);
        let note = Note {
            key: key.clone(),
            value: args.value,
            label: args.label,
            status,
            corrected: false,
            updated_at: now_millis(),
        };
        self.store.upsert_note(note).map_err(internal)?;

        Ok(json!({ "ok": true, "kind": kind.as_str(), "key": key.key }))
    }

    fn delete_note(
        &mut self,
        sessions: &[String],
        turn_room: &RoomId,
        args: DelNoteArgs,
    ) -> Result<Value, ToolError> {
        let room = self.resolve(&args.room, turn_room)?;
        self.ensure_readable(sessions, &room)?;

        let kind = match args.kind.as_deref() {
            Some(text) => parse_note_kind(text)?,
            None => NoteKind::Term,
        };
        let key = NoteKey::new(room, kind, args.key);
        let removed = self.store.delete_note(&key).map_err(internal)?;
        Ok(json!({ "removed": removed }))
    }

    fn events(
        &self,
        sessions: &[String],
        turn_room: &RoomId,
        args: EventsArgs,
    ) -> Result<Value, ToolError> {
        let room = self.resolve(&args.room, turn_room)?;
        self.ensure_readable(sessions, &room)?;

        let limit = args.limit.unwrap_or(DEFAULT_HISTORY).min(MAX_HISTORY);
        let mut records: Vec<Record> = self
            .store
            .since(&room, 0, usize::MAX)
            .map_err(internal)?
            .into_iter()
            .filter(|record| !record.is_message())
            .collect();

        let skip = records.len().saturating_sub(limit);
        records.drain(..skip);
        Ok(json!({ "room": room, "events": records }))
    }

    /// 参数里没给房间就用本轮的房间；给了就解析。
    fn resolve(&self, given: &Option<String>, turn_room: &RoomId) -> Result<RoomId, ToolError> {
        match given {
            Some(text) => resolve_room(text),
            None => Ok(turn_room.clone()),
        }
    }

    // ---- 记忆工具：文件层 ----

    /// 定位当前会话在 `memory` 上的 worktree。校验「这个会话确实引用了该记忆」，
    /// 并惰性初始化 worktree。
    fn memory_worktree(
        &self,
        session: &str,
        memory: &str,
    ) -> Result<std::path::PathBuf, ToolError> {
        let Some(session_cfg) = self.config.session(session) else {
            return Err(ToolError::new(
                ErrorCode::Internal,
                format!("会话 {session} 不存在"),
            ));
        };
        self.memory
            .resolve_for(session, memory, &session_cfg.memories, &|memory_id| {
                self.config.memory(memory_id).map(|m| m.location.clone())
            })
    }

    fn memory_read(&self, session: &str, args: MemoryArgs) -> Result<Value, ToolError> {
        let worktree = self.memory_worktree(session, &args.memory)?;
        let content = read_file(&worktree, &args.path)?;
        Ok(json!({ "path": args.path, "content": content }))
    }

    fn memory_write(&self, session: &str, args: MemoryWriteArgs) -> Result<Value, ToolError> {
        let worktree = self.memory_worktree(session, &args.memory)?;
        write_file(&worktree, &args.path, &args.content)?;
        Ok(json!({ "path": args.path, "written": true }))
    }

    fn memory_list(&self, session: &str, args: MemoryArgs) -> Result<Value, ToolError> {
        let worktree = self.memory_worktree(session, &args.memory)?;
        let entries = list_dir(&worktree, &args.path)?;
        Ok(json!({ "path": args.path, "entries": entries }))
    }

    fn memory_search(&self, session: &str, args: MemorySearchArgs) -> Result<Value, ToolError> {
        let worktree = self.memory_worktree(session, &args.memory)?;
        let hits = search(&worktree, &args.path, &args.needle)?;
        Ok(json!({ "needle": args.needle, "hits": hits }))
    }

    // ---- 记忆工具：git 层 ----

    fn memory_git_status(&self, session: &str, args: MemoryArgs) -> Result<Value, ToolError> {
        let worktree = self.memory_worktree(session, &args.memory)?;
        let status = git_status(&worktree)?;
        Ok(json!({ "status": status }))
    }

    fn memory_git_log(&self, session: &str, args: MemoryLogArgs) -> Result<Value, ToolError> {
        let worktree = self.memory_worktree(session, &args.memory)?;
        let log = git_log(&worktree, args.limit.unwrap_or(10));
        Ok(json!({ "log": log }))
    }

    fn memory_git_diff(&self, session: &str, args: MemoryArgs) -> Result<Value, ToolError> {
        let worktree = self.memory_worktree(session, &args.memory)?;
        let diff = git_diff(&worktree)?;
        Ok(json!({ "diff": diff }))
    }

    fn memory_git_blame(&self, session: &str, args: MemoryBlameArgs) -> Result<Value, ToolError> {
        let worktree = self.memory_worktree(session, &args.memory)?;
        let blame = git_blame(&worktree, &args.path)?;
        Ok(json!({ "path": args.path, "blame": blame }))
    }

    fn memory_git_add(&self, session: &str, args: MemoryAddArgs) -> Result<Value, ToolError> {
        let worktree = self.memory_worktree(session, &args.memory)?;
        git_add(&worktree, &args.path)?;
        Ok(json!({ "staged": if args.path.is_empty() { "全部" } else { &args.path } }))
    }

    fn memory_git_commit(&self, session: &str, args: MemoryCommitArgs) -> Result<Value, ToolError> {
        let worktree = self.memory_worktree(session, &args.memory)?;
        let result = git_commit(&worktree, &args.message)?;
        Ok(json!({ "result": result }))
    }

    fn memory_git_push(&self, session: &str, args: MemoryArgs) -> Result<Value, ToolError> {
        let worktree = self.memory_worktree(session, &args.memory)?;
        let result = git_push(&worktree)?;
        Ok(json!({ "result": result }))
    }

    fn memory_git_branch(&self, session: &str, args: MemoryArgs) -> Result<Value, ToolError> {
        let worktree = self.memory_worktree(session, &args.memory)?;
        let branches = git_branch(&worktree)?;
        Ok(json!({ "branches": branches }))
    }
}

fn resolve_room(text: &str) -> Result<RoomId, ToolError> {
    RoomId::parse(text).map_err(|error| {
        ToolError::new(
            ErrorCode::BadRequest,
            format!("房间要写成 连接:段/段（如 qq:group/123），收到 {text}（{error}）"),
        )
    })
}

fn parse<T: for<'de> Deserialize<'de>>(args: Value) -> Result<T, ToolError> {
    serde_json::from_value(args)
        .map_err(|error| ToolError::new(ErrorCode::BadRequest, format!("参数不对：{error}")))
}

fn parse_note_kind(text: &str) -> Result<NoteKind, ToolError> {
    NoteKind::parse(text).ok_or_else(|| {
        ToolError::new(
            ErrorCode::BadRequest,
            format!("笔记种类只能是 term / member / topic，收到 {text}"),
        )
    })
}

fn note_json(note: &Note) -> Value {
    json!({
        "kind": note.key.kind.as_str(),
        "key": note.key.key,
        "value": note.value,
        "label": note.label,
        "status": note.status.map(|status| match status {
            TopicStatus::Open => "open",
            TopicStatus::Closed => "closed",
        }),
        "corrected": note.corrected,
        "updated_at": note.updated_at,
    })
}

fn internal(error: impl std::fmt::Display) -> ToolError {
    ToolError::new(ErrorCode::Internal, error.to_string())
}

fn outbound_error(error: OutboundError) -> ToolError {
    match error {
        OutboundError::Rejected(detail) => ToolError::new(ErrorCode::Internal, detail),
        OutboundError::Unsupported(detail) => ToolError::new(ErrorCode::BadRequest, detail),
        OutboundError::Unavailable(detail) => ToolError::new(ErrorCode::Internal, detail),
    }
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}
