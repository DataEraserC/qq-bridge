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
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use acb_proto::element::flatten_text;
use acb_proto::{
    Delivery, Element, ErrorCode, MemoryInfo, Record, RecordKind, RoomId, Sender, SessionInfo,
    Ticket, ToolError, TurnReason,
};
use serde::Deserialize;
use serde_json::{Value, json};
use thiserror::Error;

use crate::config::{Config, Session, TurnErrorAction, WakeDecision, WakeInput, WakePolicy};
use crate::memory::{
    MemoryRepo, git_add, git_blame, git_branch, git_commit, git_diff, git_log, git_push,
    git_status, list_dir, read_file, search, write_file,
};
use crate::outbound::{CallError, MethodSpec, Outbound, OutboundError, Risk, Scope, expand_query};
use crate::render;
use crate::storage::{
    Cursor, MIN_KEPT, Note, NoteKey, NoteKind, PrunePlan, RecordDraft, StorageError, Store,
    TopicStatus, unread,
};
use crate::tools::{
    AckArgs, DEFAULT_COLUMNS, DEFAULT_PAGE_LIMIT, DelNoteArgs, EventsArgs, FeedbackArgs,
    FetchRecordsArgs, HistoryArgs, MAX_PAGE_LIMIT, MemoryAddArgs, MemoryArgs, MemoryBlameArgs,
    MemoryCommitArgs, MemoryListArgs, MemoryLogArgs, MemorySearchArgs, MemoryWriteArgs, NotesArgs,
    PlatformCallArgs, PlatformMethodsArgs, RECORD_COLUMNS, SendArgs, SendToArgs, SetNoteArgs,
    is_known,
};

/// 一轮最多投多少条。剩下的下一轮再投，免得一次塞爆上下文。
pub const MAX_DELIVERY: usize = 200;

/// 一轮投递的字节上限：整帧不能超过 1 MiB（协议上限），这里留一半余量给信封和记忆信息。
///
/// 为什么按字节再设一道：200 条长消息也能撑到一兆。以前只按条数截，真碰上大段文字
/// 会把 `turn_start` 撑爆，harness 那边帧解不开、直接断连——看起来像「桥挂了」。
const MAX_DELIVERY_BYTES: usize = 512 * 1024;

/// 查历史时的默认条数与上限。
const DEFAULT_HISTORY: usize = 20;
const MAX_HISTORY: usize = 200;

/// 轮次结束后票还留多久。留它是为了两件事：补确认（可能慢一步）、写反馈（随时想写）。
const TURN_GRACE: Duration = Duration::from_secs(600);

/// 轮次结束后多久之内还接受 `ack_messages`。
///
/// 只给 60 秒：再晚就说不清「这是补刚才那一轮」还是「你其实在处理新的一轮」，
/// 而新的一轮会正常投递、正常确认，不需要替旧票开太久的口子。
const ACK_GRACE: Duration = Duration::from_secs(60);

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

/// 刚刚收工的一轮。票不再有效，但留一小会儿：补确认、写反馈都要用到它。
#[derive(Debug, Clone)]
struct EndedTurn {
    session: String,
    room: RoomId,
    delivered: BTreeSet<u64>,
    at: Instant,
}

/// 一次工具调用站在哪一轮上。票据既可能有效（`live`），也可能是刚收工的那一轮。
struct TurnCtx {
    session: String,
    room: RoomId,
    delivered: BTreeSet<u64>,
    /// 这一轮还在跑吗。写反馈时记进现场，方便区分「干到一半卡住」和「收工后才想起说」。
    live: bool,
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
    /// 刚收工的轮次。留 [`TURN_GRACE`]：补确认、写反馈还要用到票上的会话与房间。
    ended: HashMap<Ticket, EndedTurn>,
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
            ended: HashMap::new(),
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

    /// 可变地看存储。只给控制台用（改反馈状态），调度自己不要借这个口绕过流水。
    pub fn store_mut(&mut self) -> &mut S {
        &mut self.store
    }

    /// 记录会话映射：acb 会话 id → harness 内部会话 id（harness 回报时调）。
    pub fn upsert_session_map(
        &mut self,
        session: String,
        harness_session: String,
        meta: Value,
    ) -> Result<(), BridgeError> {
        self.store
            .upsert_session_map(crate::storage::SessionMap {
                session,
                harness_session,
                meta,
                updated_at: now_millis(),
            })
            .map_err(BridgeError::Storage)
    }

    /// 删会话映射（重置时用）。
    pub fn delete_session_map(&mut self, session: &str) -> Result<bool, BridgeError> {
        self.store
            .delete_session_map(session)
            .map_err(BridgeError::Storage)
    }

    /// 全部会话映射，给控制台展示用。
    pub fn session_maps(&self) -> Result<Vec<crate::storage::SessionMap>, BridgeError> {
        self.store.session_maps().map_err(BridgeError::Storage)
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
    ///
    /// `error` 非空表示 harness 报告这轮失败了。这时按配置的 `on_turn_error` 动作
    /// 列表顺序执行（记日志 / 发到指定房间），让失败信息能被看见，而不是只吞进日志。
    pub fn end_turn(&mut self, ticket: &Ticket, error: Option<&str>) -> Result<(), BridgeError> {
        // 先把这一轮的信息取出来（room + session），remove 之前要用它做失败通知。
        let turn = self.active.get(self.by_ticket.get(ticket).ok_or_else(|| {
            BridgeError::UnknownTicket(ticket.to_string())
        })?);
        let turn = turn.cloned();
        let Some(turn) = turn else {
            return Err(BridgeError::UnknownTicket(ticket.to_string()));
        };

        if let Some(error) = error {
            self.notify_turn_error(&turn.session, &turn.room, error);
        }

        let Some(session) = self.by_ticket.remove(ticket) else {
            return Err(BridgeError::UnknownTicket(ticket.to_string()));
        };
        // 票作废，但留一小会儿：补确认、写反馈都还要知道「这是哪一轮、哪个房间」。
        // 以前是直接扔掉，于是「轮次结束后 ack 会报 unauthorized」——重复投递的一大来源。
        self.ended.insert(
            ticket.clone(),
            EndedTurn {
                session: turn.session.clone(),
                room: turn.room.clone(),
                delivered: turn.delivered.clone(),
                at: Instant::now(),
            },
        );
        self.ended
            .retain(|_, ended| ended.at.elapsed() < TURN_GRACE);
        self.active.remove(&session);
        Ok(())
    }

    /// 按「会话自己的」`on_turn_error` 顺序执行失败通知；会话没配就回退到全局默认
    /// `default_on_turn_error`。一个动作失败不打断后面的。
    fn notify_turn_error(&mut self, session: &str, room: &RoomId, error: &str) {
        // 会话自己配了动作就用它的；没配（空）或找不到会话，回退到全局默认。
        // 注意：会话存在但 on_turn_error 是空列表，也算「没配」——否则全局默认
        // 永远不生效（会话总有这条记录）。这是之前失败通知发不出去的根因。
        let actions = self
            .config
            .session(session)
            .filter(|s| !s.on_turn_error.is_empty())
            .map(|s| s.on_turn_error.clone())
            .unwrap_or_else(|| self.config.default_on_turn_error.clone());
        for action in actions {
            match action {
                // `log` 由 endpoint 的 Log 帧负责（见 turn_end 处理），这里无需重复。
                TurnErrorAction::Log => {}
                TurnErrorAction::Send { room: target } => {
                    let text = format!("[一轮失败] 会话 {session}（{room}）：{error}");
                    // 失败通知本身发不出去也不该影响主流程，但要把原因记进日志，
                    // 别静默吞掉——否则「为什么没人收到失败提示」会变成查不出的黑洞。
                    if let Err(err) = self.deliver_out(session, &target, vec![Element::text(text)]) {
                        eprintln!("失败通知发到 {target} 没成：{err:?}");
                    }
                }
            }
        }
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
        if !is_known(tool) {
            return Err(ToolError::new(
                ErrorCode::ToolNotAllowed,
                format!("没有这个工具：{tool}"),
            ));
        }

        let ctx = self.turn_ctx(ticket, tool)?;
        let turn_session = ctx.session.clone();
        let room = ctx.room.clone();
        let delivered = ctx.delivered.clone();
        let live = ctx.live;

        let value = match tool {
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
            "fetch_records" => {
                let args: FetchRecordsArgs = parse(args)?;
                self.fetch_records(sessions, &room, args)
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
            // 平台能力：core 不认识任何一个方法名，只做校验和转交（ADR-0020）。
            "platform_methods" => {
                let args: PlatformMethodsArgs = parse(args)?;
                self.platform_methods(sessions, &room, args)
            }
            "platform_call" => {
                let args: PlatformCallArgs = parse(args)?;
                self.platform_call(sessions, &room, args)
            }
            // 反馈：诚实报告「这里卡住了」。这是出口，不是抱怨箱（ADR-0021）。
            "report_feedback" => {
                let args: FeedbackArgs = parse(args)?;
                self.report_feedback(ticket, &turn_session, &room, live, args)
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
                let args: MemoryListArgs = parse(args)?;
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
        }?;

        // 所有工具结果都从这一个口出去：超预算就分页，再不行就落盘（ADR-0022）。
        // 放在这里而不是各个工具里，是为了「新加的工具天然受保护」——
        // 以前每个接口各自截断，于是截断方式各不相同，最糟的一种是把 JSON 砍成半截字符串。
        Ok(render::fit(tool, value, &self.spill_dir()))
    }

    /// 这张票能干什么。
    ///
    /// 分两种情况：轮次在跑（正常）；轮次刚结束但票还没过期（补确认、写反馈）。
    /// 后者是这次修的：以前票一作废，所有调用一律 unauthorized，agent 分不清
    /// 「轮次结束了」和「桥坏了」，于是反复重试、反复被重投。
    fn turn_ctx(&mut self, ticket: &Ticket, tool: &str) -> Result<TurnCtx, ToolError> {
        if let Some(session) = self.by_ticket.get(ticket) {
            let turn = self.active.get(session).expect("有票就一定有活跃轮次");
            return Ok(TurnCtx {
                session: turn.session.clone(),
                room: turn.room.clone(),
                delivered: turn.delivered.clone(),
                live: true,
            });
        }

        let Some(ended) = self.ended.get(ticket).cloned() else {
            return Err(no_active_round());
        };

        // 轮次结束了。只放行两件事——其余工具这轮真的用不了，说清楚让 agent 收手。
        match tool {
            "ack_messages" => {
                if ended.at.elapsed() > ACK_GRACE {
                    return Err(ToolError::new(
                        ErrorCode::Unauthorized,
                        format!(
                            "[ack_expired] 这一轮结束超过 {} 秒了，补确认的口子关了。\
                             那几条会在下一轮作为未读重投给你（编号一样），到时候再确认就行；\
                             现在别重试——重试也不会成功。",
                            ACK_GRACE.as_secs()
                        ),
                    ));
                }
            }
            "report_feedback" => {}
            _ => return Err(no_active_round()),
        }

        Ok(TurnCtx {
            session: ended.session,
            room: ended.room,
            delivered: ended.delivered,
            live: false,
        })
    }

    /// 落盘兜底放在哪：状态库旁边（`state/spill/`），跟库一起备份、一起清。
    fn spill_dir(&self) -> std::path::PathBuf {
        std::path::Path::new(&self.config.storage.path)
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .map(|parent| parent.join("spill"))
            .unwrap_or_else(|| std::path::PathBuf::from("spill"))
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

    /// 连接级权限：这组会话里有任何一个在订阅这条连接上的房间，就算有权限。
    ///
    /// 好友列表、账号资料这类方法跟房间无关，所以不要求模型报出一个具体房间；
    /// 但也不能凭空调一个自己完全没订阅的号。
    fn ensure_connection_readable(
        &self,
        sessions: &[String],
        connection: &str,
    ) -> Result<(), ToolError> {
        let ok = self
            .config
            .sessions
            .iter()
            .filter(|session| sessions.iter().any(|id| id == &session.id))
            .any(|session| {
                session
                    .subscriptions
                    .iter()
                    .any(|subscription| subscription.room.connection == connection)
            });
        if !ok {
            return Err(ToolError::new(
                ErrorCode::RoomNotSubscribed,
                format!("你这组会话在连接 {connection} 上没有任何订阅"),
            ));
        }
        Ok(())
    }

    /// 解析一次平台调用落在哪个房间上，并按方法声明的 [`Scope`] 做权限校验。
    ///
    /// 房间级的必须落在一个具体房间上；连接级的只借 `room` 选连接，不给就用当前轮次的。
    fn resolve_capability_room(
        &self,
        sessions: &[String],
        given: &Option<String>,
        turn_room: &RoomId,
        scope: Scope,
    ) -> Result<RoomId, ToolError> {
        let room = self.resolve(given, turn_room)?;
        match scope {
            Scope::Room => self.ensure_readable(sessions, &room)?,
            Scope::Connection => self.ensure_connection_readable(sessions, &room.connection)?,
        }
        Ok(room)
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

        // 会话的身份信息：harness 据此知道「以什么身份说话、用哪个模型」。
        let session_info = self
            .config
            .session(&pending.session)
            .map(|session| SessionInfo {
                id: session.id.clone(),
                name: session.name.clone(),
                harness: session.harness.clone(),
                model: session.model.clone(),
            });

        // 消息过多时不全投：只投前 delivery_limit 条，剩下的让模型用 fetch_records 抓。
        let limit = self.config.delivery_limit;
        let total_pending = records.len();
        if limit > 0 && records.len() > limit {
            records.truncate(limit);
        }
        // 再按字节卡一道：条数不多但每条很长时，200 条照样能把 `turn_start` 撑过 1 MiB，
        // harness 那边帧就解不开了——表现是「连上了又立刻断」，比截断难查十倍。
        let keep = fit_records(&records, MAX_DELIVERY_BYTES);
        records.truncate(keep);
        // 有截断就把「还有几条」写上，让 harness 提示模型去 fetch_records。
        let more = (total_pending > records.len()).then(|| total_pending - records.len());

        Ok(Delivery {
            ticket,
            session: pending.session,
            room: pending.room,
            records,
            reason: pending.reason,
            memories,
            session_info,
            more,
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
                        role: None,
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

        // 回执要「像回执」：ok 摆在第一位，发出去了才给 platform_id / at。
        // 失败时这个函数直接返回 Err（调用方拿到的是 error），绝不会出现
        // 「一脸成功但平台没收到」的形状——模型据此判断能不能去群里说「已经发了」。
        Ok(json!({
            "ok": true,
            "id": record.id,
            "platform_id": sent.platform_id,
            "at": sent.at,
            "room": room.to_string(),
            "watermark": watermark,
        }))
    }

    /// 确认一批记录。
    ///
    /// 两条规矩，都是为了让「确认」这件事**幂等**：
    /// 1. 已经确认过的编号（<= 当前水位）当成功跳过。重复确认不是错误——以前它报
    ///    bad_request，模型于是以为「确认失败了」，接着往下不再确认，那几条就被反复重投。
    /// 2. 只有「本轮真的投给过这个会话」的编号才能确认；别的编号报 `not_delivered`，
    ///    并在错误里说清本轮投的是哪一段，让模型一轮就知道该改什么。
    fn ack(
        &mut self,
        session: &str,
        room: &RoomId,
        delivered: &BTreeSet<u64>,
        ids: Vec<u64>,
    ) -> Result<Value, ToolError> {
        if ids.is_empty() {
            return Err(ToolError::new(
                ErrorCode::BadRequest,
                "[bad_params] ids 不能为空：把投递里记录前面的 #编号写进来",
            ));
        }

        let cursor = Cursor::new(session, room.clone());
        let watermark = self.store.watermark(&cursor).map_err(internal)?;

        let mut fresh: Vec<u64> = Vec::new();
        let mut already: Vec<u64> = Vec::new();
        for id in ids {
            if id <= watermark {
                already.push(id);
                continue;
            }
            if !delivered.contains(&id) {
                return Err(ToolError::new(
                    ErrorCode::BadRequest,
                    format!(
                        "[not_delivered] #{id} 这一轮没投给你，确认不了。这一轮投给你的：{range}。\
                         已经确认过的编号不用再确认（跳过去就行）。",
                        range = delivery_range(delivered)
                    ),
                ));
            }
            fresh.push(id);
        }

        if fresh.is_empty() {
            // 全都是确认过的：这是好消息，不是错误。
            return Ok(json!({
                "ok": true,
                "watermark": watermark,
                "acked": [],
                "already": already,
                "note": "这些编号之前已经确认过了，水位没动。不用再确认，接着干活就行。",
            }));
        }

        let watermark = self.store.ack(&cursor, &fresh).map_err(internal)?;
        for id in fresh.iter().chain(already.iter()) {
            self.store.forget_delivery(&cursor, *id).map_err(internal)?;
        }

        Ok(json!({
            "ok": true,
            "watermark": watermark,
            "acked": fresh,
            "already": already,
        }))
    }

    fn history(
        &self,
        sessions: &[String],
        turn_room: &RoomId,
        args: HistoryArgs,
    ) -> Result<Value, ToolError> {
        let room = self.resolve(&args.room, turn_room)?;
        self.ensure_readable(sessions, &room)?;

        let columns = column_set(args.columns.as_deref())?;

        let limit = args.limit.unwrap_or(DEFAULT_HISTORY).min(MAX_HISTORY);
        let mut records: Vec<Record> = self
            .store
            .since(&room, 0, usize::MAX)
            .map_err(internal)?
            .into_iter()
            .filter(|record| args.before.is_none_or(|before| record.id < before))
            .collect();

        // 总数一定要给：不给的话「还有多少条」只能靠猜，猜错就以为翻到底了。
        let total = records.len();
        let skip = total.saturating_sub(limit);
        records.drain(..skip);
        let has_more = skip > 0;
        // 往更早翻：把 oldest-1 当 before 传回来即可（不用记 offset，也不会因为
        // 中途有新消息进来而错位——新消息都在另一头）。
        let next_before = if has_more {
            records.first().map(|record| record.id)
        } else {
            None
        };

        let rendered: Vec<Value> = records.iter().map(|r| record_columns(r, &columns)).collect();
        Ok(json!({
            "room": room,
            "columns": columns,
            "total": total,
            "returned": rendered.len(),
            "has_more": has_more,
            "next_before": next_before,
            "records": rendered,
        }))
    }

    /// 按水位编号精确查一批记录，只返回指定列。
    fn fetch_records(
        &self,
        sessions: &[String],
        turn_room: &RoomId,
        args: FetchRecordsArgs,
    ) -> Result<Value, ToolError> {
        let room = self.resolve(&args.room, turn_room)?;
        self.ensure_readable(sessions, &room)?;

        if args.ids.is_empty() {
            return Err(ToolError::new(
                ErrorCode::BadRequest,
                "[bad_params] ids 不能为空：把投递里记录前面的 #编号写进来",
            ));
        }
        let columns = column_set(args.columns.as_deref())?;

        let wanted: std::collections::BTreeSet<u64> = args.ids.iter().copied().collect();
        let records: Vec<Record> = self
            .store
            .since(&room, 0, usize::MAX)
            .map_err(internal)?
            .into_iter()
            .filter(|record| wanted.contains(&record.id))
            .collect();

        // 找出没查到的编号，一起回给调用方，别让 agent 以为「那些 id 不存在」。
        let found: std::collections::BTreeSet<u64> = records.iter().map(|r| r.id).collect();
        let missing: Vec<u64> = wanted.difference(&found).copied().collect();

        let rendered: Vec<Value> = records.iter().map(|r| record_columns(r, &columns)).collect();
        Ok(json!({
            "room": room,
            "columns": columns,
            "total": wanted.len(),
            "returned": rendered.len(),
            "records": rendered,
            "missing": missing,
        }))
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
        let total = rooms.len();
        Ok(json!({ "rooms": rooms, "total": total }))
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

        Ok(json!({
            "room": room,
            "total": notes.len(),
            "notes": notes.iter().map(note_json).collect::<Vec<_>>(),
        }))
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

        let limit = args.limit.unwrap_or(DEFAULT_HISTORY).min(MAX_PAGE_LIMIT);
        let offset = args.offset.unwrap_or(0);
        let events: Vec<Record> = self
            .store
            .since(&room, 0, usize::MAX)
            .map_err(internal)?
            .into_iter()
            .filter(|record| !record.is_message())
            .collect();

        let total = events.len();
        let page: Vec<Record> = events.into_iter().skip(offset).take(limit).collect();
        let returned = page.len();
        let has_more = offset + returned < total;
        Ok(json!({
            "room": room,
            "total": total,
            "offset": offset,
            "returned": returned,
            "has_more": has_more,
            "next_offset": if has_more { json!(offset + returned) } else { Value::Null },
            "events": page,
        }))
    }

    /// 平台能力目录（ADR-0020）。
    ///
    /// 两个维度、两档披露：
    /// - 名字里点号前那段是**归属**，跟平台 API 的命名空间对齐；`tags` 是**用途**，跨归属。
    /// - 不给任何条件 = 逐条清单（名字 + 一句话 + 标签），够模型决定调哪个。
    /// - 给了 `group` / `tag` / `query` = 交集里的方法连**参数表**一起给，这时候模型是在准备调。
    ///
    /// 为什么清单不砍成「只有组名」：多一次往返就是多一整轮推理和几秒延迟，
    /// 而一张清单在四五十个方法时也就一千来 token，只花在模型主动查的那一轮。
    fn platform_methods(
        &self,
        sessions: &[String],
        turn_room: &RoomId,
        args: PlatformMethodsArgs,
    ) -> Result<Value, ToolError> {
        // 目录本身就是连接级的东西：同一个号的哪个房间，能做的事都一样。
        // 所以这里不要求模型报出一个具体房间，只要它订阅了这条连接就行。
        let room = self.resolve_capability_room(sessions, &args.room, turn_room, Scope::Connection)?;

        let methods = self.outbound.catalog(&room.connection);
        if methods.is_empty() {
            return Err(no_catalog(&room.connection));
        }
        let room_text = room.to_string();
        let connection = room.connection.clone();
        let platform = self.platform_of(&room.connection).to_owned();

        let group = clean(args.group.as_deref());
        let tag = clean(args.tag.as_deref());
        let query = clean(args.query.as_deref());

        // 没设条件：清单。不带参数表——那样就等于把整张表倒出来。
        if group.is_none() && tag.is_none() && query.is_none() {
            let list: Vec<Value> = methods.iter().map(describe_brief).collect();
            return Ok(json!({
                "room": room_text,
                "connection": connection,
                "platform": platform,
                "total": methods.len(),
                "returned": list.len(),
                "methods": list,
                "tags": tags_of(methods),
                "hint": "名字里点号前那段是归属（跟平台 API 命名空间对齐）；tags 是用途，跨归属。\
                         要某个方法的参数表，再调一次并给 group / tag / query，三者可叠加。\
                         scope=connection 的方法跟房间无关（好友列表、账号资料这类），调它不用给 room。",
            }));
        }

        // 缩了范围：命中的方法连参数表一起给。
        let terms = query.as_deref().map(expand_query).unwrap_or_default();
        let hits: Vec<&MethodSpec> = methods
            .iter()
            .filter(|method| group.map_or(true, |group| method.group() == group))
            .filter(|method| tag.map_or(true, |tag| method.has_tag(tag)))
            .filter(|method| {
                terms.is_empty() || terms.iter().any(|term| method.matches(term))
            })
            .collect();

        if hits.is_empty() {
            return Err(no_match(methods, group, tag, query.as_deref()));
        }

        Ok(json!({
            "room": room_text,
            "connection": connection,
            "platform": platform,
            "total": methods.len(),
            "matched": hits.len(),
            "methods": hits.iter().map(|method| describe_method(method)).collect::<Vec<_>>(),
        }))
    }

    /// 调一个平台方法。core 只做校验和转交，不认识任何方法名。
    fn platform_call(
        &mut self,
        sessions: &[String],
        turn_room: &RoomId,
        args: PlatformCallArgs,
    ) -> Result<Value, ToolError> {
        // 先定位连接，权限校验要等方法查出来才知道该按房间算还是按连接算
        // （好友列表这类跟房间无关，用房间级的尺子量它会逼模型瞎编一个群号）。
        let room = self.resolve(&args.room, turn_room)?;

        let methods = self.outbound.catalog(&room.connection);
        if methods.is_empty() {
            return Err(no_catalog(&room.connection));
        }

        // 白名单：只能调适配器声明过的方法，没有「调任意平台 API」这条路。
        let Some(spec) = methods.iter().find(|method| method.name == args.method) else {
            let hint = match methods
                .iter()
                .find(|method| method.group() == group_of_name(&args.method))
            {
                Some(near) => format!("你是不是想调 {}？", near.name),
                None => format!("这个连接支持：{}", method_names(methods)),
            };
            return Err(ToolError::new(
                ErrorCode::BadRequest,
                format!("[unknown_method] 没有这个方法：{}；{hint}", args.method),
            ));
        };

        // 权限按方法自己声明的边界来：房间里的事要订阅了这个房间，
        // 号上的事（好友列表、账号资料）只要订阅了这个连接。
        match spec.scope {
            Scope::Room => self.ensure_readable(sessions, &room)?,
            Scope::Connection => self.ensure_connection_readable(sessions, &room.connection)?,
        }

        // 破坏性操作默认关。白名单挡不住「模型被群里的话骗着去调一个它本来就有权调的方法」，
        // 只有默认关才挡得住；要放行得在 config.toml 那条连接上显式写 allow_destructive。
        if spec.risk == Risk::Destructive && !self.allows_destructive(&room.connection, spec.name) {
            return Err(ToolError::new(
                ErrorCode::ToolNotAllowed,
                format!(
                    "[forbidden] {} 是破坏性操作，连接 {} 默认不开放。\
                     要开就在 config.toml 的 [[connections]] 那段加 allow_destructive = true\
                     （或只列要开的方法，如 allow_destructive = [\"{}\"]）",
                    spec.name, room.connection, spec.name
                ),
            ));
        }

        check_params(spec, &args.params)?;

        let result = self
            .outbound
            .call(&room, spec.name, &args.params)
            .map_err(call_error)?;

        // 查询在**分页之前**做：先过滤/分组，再切页——否则 `_page.total` 给的是过滤前的数，
        // 模型永远算不出「到底几个」。
        let grouped = args.query.group_by.is_some();
        let result = crate::query::apply_to_value(result, &args.query)
            .map_err(|error| ToolError::new(ErrorCode::BadRequest, error.to_string()))?;
        if grouped {
            // 分组结果本来就只有几行（十几个角色、几个人），不切页。
            return Ok(result);
        }

        // 分页是**桥这一层**的事：平台照旧一次给全，桥切片后再交出去。
        // 平台方法签名不用为分页改五十遍，大群的大列表也不会因为「一次给不完」被写死。
        let limit = args.limit.unwrap_or(DEFAULT_PAGE_LIMIT).clamp(1, MAX_PAGE_LIMIT);
        let offset = args.offset.unwrap_or(0);
        Ok(render::slice(result, offset, limit))
    }

    /// 记一条反馈（ADR-0021）。
    ///
    /// 取向是**写建议，不是填表**：正文一个字段，想说多少说多少（上限
    /// [`MAX_FEEDBACK_BODY`] 字，超了截断并告知）；出处、次数、类型都可以不填。
    /// 桥自己会补现场（会话、harness、房间、模型、这一轮还在不在跑），
    /// agent 不用把「我是谁、我在哪」再抄一遍。
    ///
    /// 为什么要这个工具：agent 卡住时的唯一出路曾经是「自己想办法」——有的直接绕过桥
    /// 去戳平台端口。那种绕道没有订阅校验、没有审计，也没人知道它撞了什么墙。
    /// 给一个正式的出口，绕道才会自己消失。
    fn report_feedback(
        &mut self,
        ticket: &Ticket,
        session: &str,
        room: &RoomId,
        live: bool,
        args: FeedbackArgs,
    ) -> Result<Value, ToolError> {
        let body = args.text.trim();
        if body.is_empty() {
            return Err(ToolError::new(
                ErrorCode::BadRequest,
                "[bad_params] text 不能为空：把「你被什么卡住了」写进来，一两句话就行。",
            ));
        }

        let kind = match args.kind.as_deref().map(str::trim) {
            None | Some("") => "friction",
            Some(kind) => {
                let kind = kind.to_ascii_lowercase();
                if !FEEDBACK_KINDS.contains(&kind.as_str()) {
                    return Err(ToolError::new(
                        ErrorCode::BadRequest,
                        format!(
                            "[bad_params] kind 只能是 {}，收到 {kind}；不填默认 friction。",
                            FEEDBACK_KINDS.join(" / ")
                        ),
                    ));
                }
                // 字符串要活得比函数长：转成静态表里的那个值。
                FEEDBACK_KINDS
                    .iter()
                    .find(|known| **known == kind)
                    .copied()
                    .unwrap_or("friction")
            }
        };

        let severity = match args.severity.as_deref().map(str::trim) {
            None | Some("") => default_severity(kind),
            Some(severity) => {
                let severity = severity.to_ascii_lowercase();
                if !FEEDBACK_SEVERITIES.contains(&severity.as_str()) {
                    return Err(ToolError::new(
                        ErrorCode::BadRequest,
                        format!(
                            "[bad_params] severity 只能是 {}，收到 {severity}。",
                            FEEDBACK_SEVERITIES.join(" / ")
                        ),
                    ));
                }
                FEEDBACK_SEVERITIES
                    .iter()
                    .find(|known| **known == severity)
                    .copied()
                    .unwrap_or("major")
            }
        };

        // 第一条非空行当标题，剩下的当正文。想写日记也行，列表里只显示第一行。
        let (title_raw, _) = split_first_line(body);
        let (title, title_clipped) = clip(title_raw, MAX_FEEDBACK_TITLE);
        let (body, body_clipped) = clip(body, MAX_FEEDBACK_BODY);
        let suggestion = args.suggestion.as_deref().map(str::trim).filter(|text| !text.is_empty());
        let (suggestion, suggestion_clipped) = match suggestion {
            Some(text) => {
                let (clipped, cut) = clip(text, MAX_FEEDBACK_SUGGESTION);
                (Some(clipped), cut)
            }
            None => (None, false),
        };

        // 出处：清掉空串、限个数、限长度。这里不校验「这个编号存不存在」——
        // 有时候最该记下来的正是「那个编号查不到」。
        let mut refs: Vec<String> = Vec::new();
        for item in args.refs.iter().take(MAX_FEEDBACK_REFS) {
            let item = item.trim();
            if item.is_empty() {
                continue;
            }
            let (item, _) = clip(item, MAX_FEEDBACK_REF_CHARS);
            if !refs.contains(&item) {
                refs.push(item);
            }
        }
        let refs_dropped = args.refs.len().saturating_sub(MAX_FEEDBACK_REFS);

        let count = args.count.unwrap_or(1).clamp(1, MAX_FEEDBACK_OCCURRENCES);

        let harness = self
            .config
            .session(session)
            .map(|session| session.harness.clone());
        let model = self
            .config
            .session(session)
            .and_then(|session| session.model.clone());
        let at = now_millis();
        let log_title = title.clone();

        let draft = crate::storage::FeedbackDraft {
            fingerprint: feedback_fingerprint(&title),
            title,
            body,
            suggestion,
            kind: kind.to_owned(),
            severity: severity.to_owned(),
            refs,
            occurrences: count,
            tool: args.tool.as_deref().map(str::trim).filter(|t| !t.is_empty()).map(str::to_owned),
            session: session.to_owned(),
            harness,
            room: Some(room.to_string()),
            context: json!({
                "ticket": ticket.as_str(),
                "model": model,
                "turn_running": live,
                "at": at,
            }),
            at,
        };

        let recorded = self.store.record_feedback(draft).map_err(internal)?;

        // 也打进桥日志：一份被人看见的反馈，不该只躺在库里等人去查表。
        eprintln!(
            "反馈 #{} [{} {}] {}：{}（自称 {} 次，第 {} 次报告）",
            recorded.id, kind, severity, session, log_title, recorded.occurrences, recorded.repeats
        );

        let mut truncated = serde_json::Map::new();
        if title_clipped {
            truncated.insert("title".into(), json!(MAX_FEEDBACK_TITLE));
        }
        if body_clipped {
            truncated.insert("text".into(), json!(MAX_FEEDBACK_BODY));
        }
        if suggestion_clipped {
            truncated.insert("suggestion".into(), json!(MAX_FEEDBACK_SUGGESTION));
        }

        Ok(json!({
            "ok": true,
            "id": recorded.id,
            "duplicate": recorded.duplicate,
            "repeats": recorded.repeats,
            "occurrences": recorded.occurrences,
            "truncated": truncated,
            "refs_dropped": refs_dropped,
            "note": if recorded.duplicate {
                "这条已经提过（同样的标题），次数已累加，不用再提。接着干活就行。"
            } else {
                "记下了。这是出口，不是许愿池：写清「在哪一步卡住」比写「希望支持 X」有用得多。"
            },
        }))
    }

    /// 连接名 → 平台名。只给展示用，查不到就说 unknown，不要因此拒绝服务。
    fn platform_of(&self, connection: &str) -> &str {
        self.config
            .connections
            .iter()
            .find(|item| item.id == connection)
            .map(|item| item.platform.as_str())
            .unwrap_or("unknown")
    }

    /// 这条连接的配置放不放行这个破坏性方法。查不到连接 = 不放行。
    fn allows_destructive(&self, connection: &str, method: &str) -> bool {
        self.config
            .connections
            .iter()
            .find(|item| item.id == connection)
            .is_some_and(|item| item.allow_destructive.allows(method))
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

    fn memory_list(&self, session: &str, args: MemoryListArgs) -> Result<Value, ToolError> {
        let worktree = self.memory_worktree(session, &args.memory)?;
        let entries = list_dir(&worktree, &args.path)?;
        let total = entries.len();
        let offset = args.offset.unwrap_or(0);
        let limit = args.limit.unwrap_or(DEFAULT_PAGE_LIMIT).clamp(1, MAX_PAGE_LIMIT);
        let page: Vec<String> = entries.into_iter().skip(offset).take(limit).collect();
        let has_more = offset + page.len() < total;
        Ok(json!({
            "path": args.path,
            "total": total,
            "offset": offset,
            "returned": page.len(),
            "has_more": has_more,
            "next_offset": if has_more { json!(offset + page.len()) } else { Value::Null },
            "entries": page,
        }))
    }

    fn memory_search(&self, session: &str, args: MemorySearchArgs) -> Result<Value, ToolError> {
        let worktree = self.memory_worktree(session, &args.memory)?;
        let hits = search(&worktree, &args.path, &args.needle)?;
        let total = hits.len();
        let offset = args.offset.unwrap_or(0);
        let limit = args.limit.unwrap_or(DEFAULT_PAGE_LIMIT).clamp(1, MAX_PAGE_LIMIT);
        let page: Vec<String> = hits.into_iter().skip(offset).take(limit).collect();
        let has_more = offset + page.len() < total;
        Ok(json!({
            "needle": args.needle,
            "total": total,
            "offset": offset,
            "returned": page.len(),
            "has_more": has_more,
            "next_offset": if has_more { json!(offset + page.len()) } else { Value::Null },
            "hits": page,
        }))
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

/// 按字节预算算「前多少条能一起投」。至少留一条：一条都投不出去，这一轮就别开了。
///
/// 从前往后累加，一旦超预算就停在断点上——**顺序不能乱**，投递是按时间读的。
fn fit_records(records: &[Record], budget: usize) -> usize {
    let mut used = 0usize;
    for (index, record) in records.iter().enumerate() {
        let size = serde_json::to_vec(record).map(|bytes| bytes.len()).unwrap_or(0);
        if index > 0 && used + size > budget {
            return index;
        }
        used += size;
    }
    records.len()
}

/// 「本轮投给你的是哪一段」——错误信息里要能直接照抄的答案。
fn delivery_range(delivered: &BTreeSet<u64>) -> String {
    match (delivered.iter().next(), delivered.iter().next_back()) {
        (Some(first), Some(last)) if first == last => format!("#{first}"),
        (Some(first), Some(last)) => format!("#{first}–#{last}（共 {} 条）", delivered.len()),
        _ => "（这一轮没有投任何记录）".to_owned(),
    }
}

/// 票用不了了。**说清「这一轮不会再有工具可用」**——这是让 agent 收手的关键一句。
///
/// 以前这里只有「这张票无效或本轮已结束」：模型分不清是自己调错了、还是桥坏了，
/// 于是继续试别的工具，一路试到轮次真的结束。错误文案本身就是接口的一部分。
fn no_active_round() -> ToolError {
    ToolError::new(
        ErrorCode::Unauthorized,
        "[no_active_round] 这张票对应的轮次已经结束了，**这一轮不会再有工具可用——请结束本轮，别重试**。\
         没确认的消息还在，会在下一轮作为未读重新投给你（编号不变），到那时再确认即可。\
         还能做的只有两件：60 秒内补确认（ack_messages）、随时写反馈（report_feedback）。",
    )
}

fn resolve_room(text: &str) -> Result<RoomId, ToolError> {
    RoomId::parse(text).map_err(|error| {
        ToolError::new(
            ErrorCode::BadRequest,
            format!("房间要写成 连接:段/段（如 qq:group/123），收到 {text}（{error}）"),
        )
    })
}

/// 校验/归一列名，返回按文档顺序排好的列名列表。
///
/// 没指定（`None`）就给默认精简子集；指定了但含非法列名就报错，并把全部合法列名
/// 列出来，让 agent 能照着改。
fn column_set(given: Option<&[String]>) -> Result<Vec<&'static str>, ToolError> {
    let Some(given) = given else {
        return Ok(DEFAULT_COLUMNS.to_vec());
    };
    let mut columns: Vec<&'static str> = Vec::with_capacity(given.len());
    for name in given {
        let name = name.trim();
        match RECORD_COLUMNS.iter().find(|known| **known == name) {
            Some(known) => {
                if !columns.contains(known) {
                    columns.push(known);
                }
            }
            None => {
                return Err(ToolError::new(
                    ErrorCode::BadRequest,
                    format!(
                        "没有这一列：{name}。能查的列有：{}",
                        RECORD_COLUMNS.join("、")
                    ),
                ));
            }
        }
    }
    if columns.is_empty() {
        return Ok(DEFAULT_COLUMNS.to_vec());
    }
    Ok(columns)
}

/// 把一条记录按指定列序列化，并附带「每列取没取」的状态。
///
/// 返回形如 `{ "id": 237, "columns": { "id": true, "platform_id": false } }`。
/// 这样 agent 能一眼看出哪列是它要的、哪列没取，不会把「没取」误当「没有」。
///
/// 状态值是布尔，不是描述文字：「查了 / 没查」这种中文值只能给人看，机器要判断得再猜一遍。
fn record_columns(record: &Record, columns: &[&str]) -> Value {
    let mut status = serde_json::Map::new();
    let mut out = serde_json::Map::new();
    for column in RECORD_COLUMNS {
        let taken = columns.contains(column);
        status.insert((*column).to_owned(), json!(taken));
        if !taken {
            continue;
        }
        let value = match *column {
            "id" => json!(record.id),
            "room" => json!(record.room),
            "kind" => match &record.kind {
                RecordKind::Message => json!("message"),
                RecordKind::Event { event } => json!({ "event": event }),
            },
            "sender" => json!(record.sender),
            "at" => json!(record.at),
            "elements" => json!(record.elements),
            "platform_id" => json!(record.platform_id),
            "at_self" => json!(record.at_self),
            "reply_to_self" => json!(record.reply_to_self),
            _ => continue,
        };
        out.insert((*column).to_owned(), value);
    }
    out.insert("columns".to_owned(), Value::Object(status));
    Value::Object(out)
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

// ── 反馈（ADR-0021）────────────────────────────────────────────────────

/// 反馈的类型。四种够表达「哪里不对」，再多就是让人纠结了。
const FEEDBACK_KINDS: &[&str] = &["bug", "gap", "friction", "idea"];

/// 影响面。三者足够排序：干不下去 / 很烦 / 小毛病。
const FEEDBACK_SEVERITIES: &[&str] = &["blocker", "major", "minor"];

/// 正文上限（字符）。4000 字大约能装一次完整的失败对话 + 复现步骤。
///
/// 为什么有上限而不是不限：一条反馈是给人看的，超过四千字人就不会看了；
/// 真想长篇大论，写进文件再贴路径（refs 里放路径）。
const MAX_FEEDBACK_BODY: usize = 4000;

/// 标题上限（字符）：列表里一行扫过去的东西。
const MAX_FEEDBACK_TITLE: usize = 120;

/// 建议上限（字符）。
const MAX_FEEDBACK_SUGGESTION: usize = 1000;

/// 出处最多记多少条。
const MAX_FEEDBACK_REFS: usize = 20;

/// 单条出处的长度上限（字符）。
const MAX_FEEDBACK_REF_CHARS: usize = 120;

/// 自称发生次数的上限。比这更大就不像「撞了几次」，像在灌数。
const MAX_FEEDBACK_OCCURRENCES: u32 = 100_000;

/// 没写 severity 时按类型推：坏了或缺能力默认 major，别扭和愿望默认 minor。
fn default_severity(kind: &str) -> &'static str {
    match kind {
        "bug" | "gap" => "major",
        _ => "minor",
    }
}

/// 第一行（标题），其余（正文）。空行跳过。
fn split_first_line(text: &str) -> (&str, &str) {
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            let rest = &text[offset + line.len()..];
            return (trimmed, rest.trim());
        }
        offset += line.len();
    }
    (text.trim(), "")
}

/// 按字符截断（不是字节，别把中文劈成半个）。返回（截断后的文本, 是否截过）。
fn clip(text: &str, max_chars: usize) -> (String, bool) {
    if text.chars().count() <= max_chars {
        return (text.to_owned(), false);
    }
    let mut out: String = text.chars().take(max_chars).collect();
    out.push('…');
    (out, true)
}

/// 去重指纹：归一化标题的 FNV-1a。
///
/// 只认标题，不认类型：同一个坑这次报 gap、下次报 bug，还是同一个坑。
/// 合并时保留第一次的类型（人已经看过那一行，不该被后来的报告悄悄改分类）。
///
/// 不引哈希库：这个东西只用来「同一条别再建一行」，不需要抗碰撞强度——
/// 撞了最多是两条不同的反馈合并成一条（次数加起来），不会丢内容也不会出错。
fn feedback_fingerprint(title: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in title
        .to_lowercase()
        .bytes()
        .filter(|byte| !byte.is_ascii_whitespace())
    {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

// ── 平台能力（ADR-0020）────────────────────────────────────────────────

fn no_catalog(connection: &str) -> ToolError {
    ToolError::new(
        ErrorCode::ToolNotAllowed,
        format!("[unsupported_platform] 连接 {connection} 没声明任何平台特有方法"),
    )
}

/// 空串当没给：模型有时会把「我不填」写成空字符串，别让它变成一次必然落空的过滤。
fn clean(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|text| !text.is_empty())
}

/// 清单档的一条：够模型判断「这是我要的」，但不带参数表。
fn describe_brief(spec: &MethodSpec) -> Value {
    let mut item = json!({
        "name": spec.name,
        "summary": spec.summary,
        "tags": spec.tags,
        "risk": spec.risk.as_str(),
    });
    // 房间级是默认，不写；只有「跟房间无关」的那些才标一下，
    // 顺便告诉模型这些方法不用为了它编一个房间出来。五十来行清单里只多十处。
    if spec.scope == Scope::Connection {
        item["scope"] = json!(spec.scope.as_str());
    }
    item
}

/// 详情档的一条：多一张参数表（JSON Schema，模型最熟的形式）。
fn describe_method(spec: &MethodSpec) -> Value {
    json!({
        "name": spec.name,
        "summary": spec.summary,
        "tags": spec.tags,
        "risk": spec.risk.as_str(),
        "scope": spec.scope.as_str(),
        "params": spec.params_schema(),
    })
}

/// 这台连接用到的标签，按首次出现的顺序。
fn tag_names(methods: &[MethodSpec]) -> Vec<&'static str> {
    let mut names: Vec<&'static str> = Vec::new();
    for method in methods {
        for tag in method.tags {
            if !names.contains(tag) {
                names.push(tag);
            }
        }
    }
    names
}

/// 标签概览，带各几个方法。
fn tags_of(methods: &[MethodSpec]) -> Vec<Value> {
    tag_names(methods)
        .into_iter()
        .map(|tag| {
            json!({
                "tag": tag,
                "count": methods.iter().filter(|method| method.has_tag(tag)).count(),
            })
        })
        .collect()
}

/// 过滤后一个都没命中：说清楚条件是什么、手上有哪些可选项。
///
/// 这类错只有一种正确写法——带上可选项，让模型一轮就改对，而不是换个词再试三次。
fn no_match(
    methods: &[MethodSpec],
    group: Option<&str>,
    tag: Option<&str>,
    query: Option<&str>,
) -> ToolError {
    let mut given = Vec::new();
    if let Some(group) = group {
        given.push(format!("group={group}"));
    }
    if let Some(tag) = tag {
        given.push(format!("tag={tag}"));
    }
    if let Some(query) = query {
        given.push(format!("query={query}"));
    }

    let mut hint = format!("这个连接有这些归属：{}", groups_of(methods).join(" / "));
    let known_tags = tag_names(methods);
    if !known_tags.is_empty() {
        hint.push_str(&format!("；有这些标签：{}", known_tags.join(" / ")));
    }

    ToolError::new(
        ErrorCode::BadRequest,
        format!("[no_match] 没有同时满足 {} 的方法；{hint}", given.join(" + ")),
    )
}

fn method_names(methods: &[MethodSpec]) -> String {
    methods
        .iter()
        .map(|method| method.name)
        .collect::<Vec<_>>()
        .join(" / ")
}

/// 有哪些分组，按声明顺序（不是字母序）：适配器写得顺，模型看得也顺。
fn groups_of(methods: &[MethodSpec]) -> Vec<&'static str> {
    let mut groups: Vec<&'static str> = Vec::new();
    for method in methods {
        if !groups.contains(&method.group()) {
            groups.push(method.group());
        }
    }
    groups
}

fn group_of_name(method: &str) -> &str {
    method.split('.').next().unwrap_or(method)
}

/// 调之前把参数形状核一遍。
///
/// 未声明的参数也拒：既是白名单精神，也能帮模型发现字段名拼错了——平台那边回的错通常看不懂。
fn check_params(spec: &MethodSpec, params: &Value) -> Result<(), ToolError> {
    let empty = serde_json::Map::new();
    let given = match params {
        Value::Null => &empty,
        Value::Object(map) => map,
        _ => {
            return Err(ToolError::new(
                ErrorCode::BadRequest,
                format!("[bad_params] {} 的 params 要给一个对象（map）", spec.name),
            ));
        }
    };

    for param in spec.params {
        match given.get(param.name) {
            None | Some(Value::Null) => {
                if param.required {
                    return Err(ToolError::new(
                        ErrorCode::BadRequest,
                        format!(
                            "[bad_params] {} 少了必填参数 {}（{}）",
                            spec.name, param.name, param.desc
                        ),
                    ));
                }
            }
            Some(value) => {
                if !param.ty.matches(value) {
                    return Err(ToolError::new(
                        ErrorCode::BadRequest,
                        format!(
                            "[bad_params] {} 的参数 {} 要 {}，给的是 {value}",
                            spec.name,
                            param.name,
                            param.ty.json_type()
                        ),
                    ));
                }
                if !param.values.is_empty() {
                    let allowed = value.as_str().is_some_and(|text| param.values.contains(&text));
                    if !allowed {
                        return Err(ToolError::new(
                            ErrorCode::BadRequest,
                            format!(
                                "[bad_params] {} 的参数 {} 只能是 {}，给的是 {value}",
                                spec.name,
                                param.name,
                                param.values.join(" / ")
                            ),
                        ));
                    }
                }
            }
        }
    }

    for key in given.keys() {
        if !spec.params.iter().any(|param| param.name == key) {
            let known = if spec.params.is_empty() {
                "这个方法不要参数".to_owned()
            } else {
                format!(
                    "能给的参数：{}",
                    spec
                        .params
                        .iter()
                        .map(|param| param.name)
                        .collect::<Vec<_>>()
                        .join(" / ")
                )
            };
            return Err(ToolError::new(
                ErrorCode::BadRequest,
                format!("[bad_params] {} 不认识参数 {key}；{known}", spec.name),
            ));
        }
    }

    Ok(())
}

/// 适配器报的错 → 工具错。分类写进 message 里，模型靠它判断该改参数还是该放弃。
fn call_error(error: CallError) -> ToolError {
    let (code, kind) = match &error {
        CallError::UnknownMethod { .. } => (ErrorCode::BadRequest, "unknown_method"),
        CallError::BadParams(_) => (ErrorCode::BadRequest, "bad_params"),
        CallError::UnsupportedPlatform(_) => (ErrorCode::ToolNotAllowed, "unsupported_platform"),
        CallError::NotImplemented(_) => (ErrorCode::ToolNotAllowed, "not_implemented"),
        CallError::Forbidden(_) => (ErrorCode::ToolNotAllowed, "forbidden"),
        CallError::Unavailable(_) => (ErrorCode::Internal, "unavailable"),
    };
    ToolError::new(code, format!("[{kind}] {error}"))
}
