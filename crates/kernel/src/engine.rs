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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use acb_proto::element::flatten_text;
use acb_proto::{
    Delivery, Element, ErrorCode, MemoryInfo, PersonaInfo, QuoteRef, Record, RecordKind,
    RoomId, Sender, SessionInfo, Ticket, ToolError, TurnReason,
};
use serde::Deserialize;
use serde_json::{Value, json};
use thiserror::Error;

use crate::config::{ClockPolicy, Config, TurnErrorAction, WakeDecision, WakePolicy};
use crate::memory::{
    MemoryRepo, git_add, git_blame, git_branch, git_commit, git_diff, git_log, git_push,
    git_status, list_dir, read_file, search, write_file,
};
use crate::outbound::{
    CallError, CallOutcome, Effect, MethodSpec, Outbound, OutboundError, OutboundHandle, Outbounds,
    OutgoingKind, OutgoingRecord, RecordMode, Risk, Scope, Sent, TargetRole, expand_query,
};
use crate::plugin::{
    AccessAction, AccessDecision, AccessRequest, ClockContext, DefaultDeliveryComposer,
    DefaultTurnErrorProvider, DeliveryRequest, PluginError, PluginManager, ReceiveDecision,
    ReceiveRequest, SendContext, SendHook, SendOrigin, TurnErrorRequest, WakeContext,
};
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
    #[error(transparent)]
    Plugin(#[from] PluginError),
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
    /// 被连接的入站策略丢掉了——不写流水，也不叫醒任何人。
    Dropped { room: RoomId },
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
    /// 本轮投出去的编号。只有这些（以及本轮读过的）能被 `ack_messages` 确认。
    delivered: BTreeSet<u64>,
    /// 本轮通过 `get_history` / `fetch_records` / `get_platform_events` 读到的编号。
    /// 读到但没投递的内容也能确认——水位要能被补读推着走。
    read: BTreeSet<u64>,
    /// 开轮时冻结的配置。轮次中途热重载只影响下一轮；这一轮所有工具查询
    /// 都用这份快照，否则「轮次边界生效」只是投递批次的边界，不是真的边界。
    config: Arc<Config>,
}

/// 刚刚收工的一轮。票不再有效，但留一小会儿：补确认、写反馈都要用到它。
#[derive(Debug, Clone)]
struct EndedTurn {
    session: String,
    room: RoomId,
    delivered: BTreeSet<u64>,
    read: BTreeSet<u64>,
    config: Arc<Config>,
    at: Instant,
}

/// 一次工具调用站在哪一轮上。票据既可能有效（`live`），也可能是刚收工的那一轮。
struct TurnCtx {
    session: String,
    room: RoomId,
    delivered: BTreeSet<u64>,
    read: BTreeSet<u64>,
    /// 这一轮开轮时冻结的配置快照。工具期间不读 `self.config`。
    config: Arc<Config>,
    /// 这一轮还在跑吗。写反馈时记进现场，方便区分「干到一半卡住」和「收工后才想起说」。
    live: bool,
}

/// 已经做完所有校验、只差平台 I/O 的一次 `platform_call`。
///
/// `execute` 故意只拿句柄，不拿 `&mut Bridge`：endpoint 可以先把 core 的全局锁放下，
/// 真正等平台时只锁这条连接自己的出站口。
pub(crate) struct PreparedPlatformCall {
    room: RoomId,
    spec: MethodSpec,
    params: Value,
    args: PlatformCallArgs,
    /// 开轮时冻结的配置快照。结算写流水时要用它查账号、找订阅者。
    config: Arc<Config>,
    outbound: OutboundHandle,
}

impl PreparedPlatformCall {
    pub(crate) fn execute(&self) -> Result<CallOutcome, CallError> {
        self.outbound
            .call(&self.room, self.spec.name.as_ref(), &self.params)
    }
}

/// 已经做完校验、只差出站 I/O 的一次发送。
pub(crate) struct PreparedSend {
    room: RoomId,
    elements: Vec<Element>,
    config: Arc<Config>,
    outbound: OutboundHandle,
    trimmed: bool,
    relayed: Vec<(String, String)>,
    /// 这次发送替哪个会话说话（决定用哪个会话的发送钩子）。
    session: String,
    /// 工具正文还是失败通知。
    origin: SendOrigin,
    /// 发送成功后要跑的观察钩子。
    after_hooks: Vec<Arc<dyn SendHook>>,
}

impl PreparedSend {
    pub(crate) fn execute(&self) -> Result<Sent, OutboundError> {
        self.outbound.send(&self.room, &self.elements)
    }
}

/// `begin_tool_call` 的两种结果：不需要平台 I/O 的已经做完了；需要平台的交给 endpoint
/// 在锁外执行，再回来 `finish`。
pub(crate) enum ToolCallStart {
    Done(Value),
    Platform(PreparedPlatformCall),
    Send(PreparedSend),
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
    outbound: Outbounds,
    /// 扩展宿主（ADR-0026）。Phase 1 只承载 wake provider。
    plugins: PluginManager,
    pending: VecDeque<Pending>,
    /// 键是会话 id：一个会话同时只跑一轮。
    active: HashMap<String, ActiveTurn>,
    /// 票 → 会话。票是开轮时签发的，收工作废。
    by_ticket: HashMap<Ticket, String>,
    /// 刚收工的轮次。留 [`TURN_GRACE`]：补确认、写反馈还要用到票上的会话与房间。
    ended: HashMap<Ticket, EndedTurn>,
    ticket_seq: u64,
    /// 本桥实例的票前缀。和 `ticket_seq` 一起组成票面，跨重启不重复。
    ticket_prefix: String,
    /// 排队等着在锁外发出去的失败通知。endpoint 处理完一帧、放下全局锁后执行。
    pending_notices: Vec<PreparedSend>,
    /// (会话, 房间) → 上次时钟轮时间（Unix 毫秒）。只活在本进程，重启后重新计间隔。
    clock_last: HashMap<(String, RoomId), i64>,
    /// 记忆仓库管理器：负责裸仓库 init 与 worktree checkout。
    memory: MemoryRepo,
    /// 文件中继。配了 `[file_relay]` 才有：发送/平台调用里的本机文件
    /// 换成平台取得到的 URL（ADR-0025）。
    relay: Option<crate::relay::FileRelay>,
    /// 暂存的新配置（管理员在控制台保存后放这，下一次开轮时换上——ADR-0009
    /// 轮次边界重载）。正在跑的轮次不受影响：投递批次在开轮时组装，组装完就不变。
    staged_config: Option<Config>,
    /// 配置版本号：每换一次配置 +1，控制台靠它展示「配置换过没有」。
    config_version: u64,
}

/// 票前缀：进程启动时间 + 进程号 + 进程内桥实例序号。
///
/// 以前票面是「会话-N」，序号从 0 数起，重启后又从 0 数起——上一进程的旧票可能正好
/// 撞上本进程的新票，被误认成「这一轮」的票。前缀让每个进程、每个桥实例的票空间不相交。
fn ticket_prefix() -> String {
    static INSTANCE: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    let instance = INSTANCE.fetch_add(1, Ordering::Relaxed);
    format!("{nanos:x}-{}-{instance:x}", std::process::id())
}

impl<S: Store> Bridge<S> {
    /// 只用内置 provider 的桥。测试和单适配器调用点用这个。
    pub fn new(config: Config, store: S, outbound: Box<dyn Outbound>) -> Self {
        Self::with_plugins(
            config,
            store,
            outbound,
            PluginManager::with_native_builtins(),
        )
    }

    /// 带一份已经装好 loader / manager 的扩展宿主建桥。
    ///
    /// 生产 main 用这个：树外 manager 的安装是启动顺序的一部分，装完再校验配置
    /// 引用的 provider（未知 provider 在启动时死，不等第一轮）。
    pub fn with_plugins(
        config: Config,
        store: S,
        outbound: Box<dyn Outbound>,
        plugins: PluginManager,
    ) -> Self {
        // 单实现调用点（测试/单适配器）走兜底；生产 main 传进来的本身就是 Outbounds，
        // 再包一层兜底只是多一次转发，不改变按连接分发。
        let outbound = Outbounds::fallback(outbound);
        let memory = MemoryRepo::new(config.worktrees_root.clone());
        let relay = config
            .file_relay
            .as_ref()
            .map(|cfg| crate::relay::FileRelay::new(&cfg.dir, &cfg.public_base));
        Self {
            config,
            store,
            outbound,
            plugins,
            pending: VecDeque::new(),
            active: HashMap::new(),
            by_ticket: HashMap::new(),
            ended: HashMap::new(),
            ticket_seq: 0,
            ticket_prefix: ticket_prefix(),
            pending_notices: Vec::new(),
            clock_last: HashMap::new(),
            memory,
            relay,
            staged_config: None,
            config_version: 0,
        }
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// 启动时校验配置里的 provider 都存在。`main` 在起线程前调用；
    /// 未知 provider 应该在启动时死，而不是等第一轮才报。
    pub fn validate_plugins(&self) -> Result<(), PluginError> {
        self.plugins.validate_config(&self.config)
    }

    /// 校验一份**还没生效**的配置（控制台保存时用）。暂存前先过这一关，
    /// 避免未知 provider 被写进磁盘、等到下一轮才炸。
    pub fn validate_plugins_for(&self, config: &Config) -> Result<(), PluginError> {
        self.plugins.validate_config(config)
    }

    /// 暂存一份新配置。**不立即生效**：下一次开轮（[`Self::poll_turn`]）时换上
    /// （ADR-0009 轮次边界重载）。
    pub fn stage_config(&mut self, config: Config) {
        self.staged_config = Some(config);
    }

    /// 有没有暂存着的配置还没生效。
    pub fn has_staged_config(&self) -> bool {
        self.staged_config.is_some()
    }

    /// 丢弃暂存的配置（管理员存错了想撤回）。返回之前有没有暂存。
    pub fn discard_staged_config(&mut self) -> bool {
        self.staged_config.take().is_some()
    }

    /// 配置版本号：每换一次 +1。
    pub fn config_version(&self) -> u64 {
        self.config_version
    }

    /// 轮次边界：把暂存的配置换上来。开轮前必调。
    ///
    /// 顺带重建从配置派生的两样东西：记忆仓库（worktrees_root 可能改了）、
    /// 文件中继发布端（public_base/dir 可能改了）。
    fn apply_staged_config(&mut self) {
        let Some(config) = self.staged_config.take() else {
            return;
        };
        self.memory = MemoryRepo::new(config.worktrees_root.clone());
        self.relay = config
            .file_relay
            .as_ref()
            .map(|cfg| crate::relay::FileRelay::new(&cfg.dir, &cfg.public_base));
        self.config = config;
        self.config_version += 1;
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
        // 收消息也是配置消费点（唤醒判定走订阅），暂存的配置在这里也要换上——
        // 否则刚保存的新订阅会漏掉紧接着来的第一条消息。开轮（poll）处同样应用：
        // 保证投递批次组装时用的配置和开轮后工具查询看到的一致。
        self.apply_staged_config();
        let room = draft.room.clone();
        let subscribers = Self::subscribers_of(&self.config, &room);

        if subscribers.is_empty() {
            return Ok(Ingest::NotSubscribed { room });
        }

        // 入站策略：先问这条连接的 provider 收不收。被丢掉的记录不写流水、不叫醒，
        // 就像它从没到过桥——水位也不会因为它出现空洞。
        let receive_policy = self
            .config
            .connection(&room.connection)
            .map(|connection| connection.receive.clone())
            .unwrap_or_default();
        let receive_provider = self
            .plugins
            .resolve_receive(&receive_policy.provider, &receive_policy)?;
        let text = flatten_text(&draft.elements);
        let platform = self
            .config
            .connection(&room.connection)
            .map(|connection| connection.platform.clone())
            .unwrap_or_else(|| "unknown".to_owned());
        let receive_context = ReceiveRequest {
            connection: &room.connection,
            platform: &platform,
            room: &room,
            sender: &draft.sender,
            kind: &draft.kind,
            elements: &draft.elements,
            text: &text,
            at_self: draft.at_self,
        };
        if receive_provider.decide(&receive_policy, &receive_context) == ReceiveDecision::Drop {
            return Ok(Ingest::Dropped { room });
        }

        // 先把这一轮要用的 provider 都解析出来。放 append 之前是有意的：provider id 写错
        // 应该在入库前就报错，不能先写一条流水再发现叫不醒。
        let mut wake_providers = Vec::with_capacity(subscribers.len());
        for subscriber in &subscribers {
            wake_providers.push(
                self.plugins
                    .resolve_wake(&subscriber.wake.provider, &subscriber.wake)?,
            );
        }

        // 「引用的是不是我发的」要查库，所以入库前先判。判据是本房间那条连接的登录账号——
        // 被引用的那条如果是这个号发的，这条就算冲我来的（WakePolicy::reply）。
        let account = self.config.account_of_room(&room);
        let reply_to_self = self.quotes_own_message(&draft, &account)?;
        let draft = draft.with_reply_to_self(reply_to_self);

        let record = self.store.append(draft)?;
        let event = match &record.kind {
            RecordKind::Event { event } => Some(event.as_str()),
            RecordKind::Message => None,
        };

        let mut woke = Vec::new();
        for (subscriber, provider) in subscribers.into_iter().zip(wake_providers) {
            let cursor = Cursor::new(subscriber.session.clone(), room.clone());

            // 平台把我们自己说的话回传回来：记下来，但不是未读，也不叫醒自己。
            if record.sender.id == subscriber.account {
                self.store.ack(&cursor, &[record.id])?;
                continue;
            }

            let pending = self
                .pending_records(&subscriber.session, &room, &subscriber.account)?
                .len() as u32;
            let context = WakeContext {
                session: &subscriber.session,
                room: &room,
                connection: &room.connection,
                platform: &platform,
                sender_id: &record.sender.id,
                text: &text,
                at_self: record.at_self,
                reply_to_self: record.reply_to_self,
                event,
                // 平台适配器还没有把 hints/tags 接进来；Phase 1 先给空切片。
                tags: &[],
                unread: pending,
                now: now_millis(),
            };
            if provider.decide(&subscriber.wake, &context) == WakeDecision::Wake {
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
        self.apply_staged_config();
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
        self.apply_staged_config();
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
        let turn = self.active.get(
            self.by_ticket
                .get(ticket)
                .ok_or_else(|| BridgeError::UnknownTicket(ticket.to_string()))?,
        );
        let turn = turn.cloned();
        let Some(turn) = turn else {
            return Err(BridgeError::UnknownTicket(ticket.to_string()));
        };

        if let Some(error) = error {
            self.notify_turn_error(&turn.config, &turn.session, &turn.room, error);
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
                read: turn.read.clone(),
                config: turn.config.clone(),
                at: Instant::now(),
            },
        );
        self.ended
            .retain(|_, ended| ended.at.elapsed() < TURN_GRACE);
        self.active.remove(&session);
        Ok(())
    }

    /// 取出排队等发的失败通知。endpoint 放下全局锁后执行它们，再回来结算。
    pub(crate) fn take_pending_notices(&mut self) -> Vec<PreparedSend> {
        std::mem::take(&mut self.pending_notices)
    }

    /// 把排队的失败通知一口气发完（测试和非 endpoint 调用点用）。
    ///
    /// endpoint 不走这里：它要先把全局锁放下，等平台的那段时间不挡别人。
    pub fn flush_notices(&mut self) {
        for prepared in self.take_pending_notices() {
            let sent = prepared.execute();
            if let Err(error) = self.finish_send(prepared, sent) {
                eprintln!("失败通知没发成：{error:?}");
            }
        }
    }

    /// 交给会话的失败通知 provider 选动作，再按顺序执行。默认 provider 保持原行为：
    /// 会话动作非空就用它，否则回退全局 `default_on_turn_error`。
    /// 一个动作失败不打断后面的。
    fn notify_turn_error(
        &mut self,
        config: &Arc<Config>,
        session: &str,
        room: &RoomId,
        error: &str,
    ) {
        let policy = config
            .session(session)
            .map(|session| session.turn_error.clone())
            .unwrap_or_default();
        let provider = match self
            .plugins
            .resolve_turn_error(&policy.provider, &policy)
        {
            Ok(provider) => provider,
            Err(resolve_error) => {
                eprintln!(
                    "失败通知策略 {} 解析失败，回退内置：{resolve_error}",
                    policy.provider
                );
                Arc::new(DefaultTurnErrorProvider)
            }
        };
        let actions = provider.decide(&TurnErrorRequest {
            session,
            room,
            error,
            actions: config
                .session(session)
                .map(|session| session.on_turn_error.as_slice())
                .unwrap_or(&[]),
            default_actions: &config.default_on_turn_error,
        });
        for action in actions {
            match action {
                // `log` 由 endpoint 的 Log 帧负责（见 turn_end 处理），这里无需重复。
                TurnErrorAction::Log => {}
                TurnErrorAction::Send { room: target } => {
                    let text = format!("[一轮失败] 会话 {session}（{room}）：{error}");
                    // 这里只组装，不发送：endpoint 会在放下全局锁之后真正等平台。
                    // 失败通知本身发不出去也不该影响主流程，但要把原因记进日志，
                    // 别静默吞掉——否则「为什么没人收到失败提示」会变成查不出的黑洞。
                    match self.prepare_send(
                        config.clone(),
                        session,
                        target,
                        SendOrigin::TurnFailure,
                        vec![Element::text(text)],
                    ) {
                        Ok(prepared) => self.pending_notices.push(prepared),
                        Err(err) => eprintln!("失败通知组装失败：{err:?}"),
                    }
                }
            }
        }
    }

    /// 调工具。
    ///
    /// 工具面一律按**会话**圈，不按连接圈（设备概念已删，2026-11-01 用户裁定）：
    /// 能读、能列、能发的房间 = 当前轮次会话订阅的房间。同一条连接上兄弟会话的
    /// 房间不可见——否则一条连接带两个会话时，agent 会自称管着对方会话的房间。
    /// 跨会话发送被砍掉（ADR-0017），读的边界同理。
    pub fn tool_call(
        &mut self,
        ticket: &Ticket,
        _connection_sessions: &[String],
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
        let turn_config = ctx.config.clone();
        // 下面的分支全用 `sessions` 当可见域——按会话圈，不是连接圈。
        let sessions: &[String] = std::slice::from_ref(&turn_session);
        let room = ctx.room.clone();
        let delivered = ctx.delivered.clone();
        let read = ctx.read.clone();
        let live = ctx.live;

        let value = match tool {
            // 不给目标：接着说当前这一轮的房间，用本轮的身份。
            "send_message" => {
                let args: SendArgs = parse(args)?;
                self.deliver_out(
                    turn_config.clone(),
                    &turn_session,
                    &room,
                    SendOrigin::Message,
                    args.elements.0,
                )
            }
            // 给了目标：跨会话发送已砍；目标房间由本会话的访问策略把关。
            "send_message_to" => {
                let args: SendToArgs = parse(args)?;
                let target = resolve_room(&args.to)?;
                self.ensure_can_send(&turn_config, &turn_session, &target)?;
                self.deliver_out(
                    turn_config.clone(),
                    &turn_session,
                    &target,
                    SendOrigin::Message,
                    args.elements.0,
                )
            }
            "ack_messages" => {
                let args: AckArgs = parse(args)?;
                self.ack(&turn_session, &room, &delivered, &read, args.ids)
            }
            "get_history" => {
                let args: HistoryArgs = parse(args)?;
                self.history(ticket, &turn_config, sessions, &room, args)
            }
            "fetch_records" => {
                let args: FetchRecordsArgs = parse(args)?;
                self.fetch_records(ticket, &turn_config, sessions, &room, args)
            }
            "list_rooms" => self.list_rooms(&turn_config, sessions),
            "get_notes" => {
                let args: NotesArgs = parse(args)?;
                self.read_notes(&turn_config, sessions, &room, args)
            }
            "set_note" => {
                let args: SetNoteArgs = parse(args)?;
                self.write_note(&turn_config, sessions, &room, args)
            }
            "del_note" => {
                let args: DelNoteArgs = parse(args)?;
                self.delete_note(&turn_config, sessions, &room, args)
            }
            "get_platform_events" => {
                let args: EventsArgs = parse(args)?;
                self.events(ticket, &turn_config, sessions, &room, args)
            }
            // 平台能力：core 不认识任何一个方法名，只做校验和转交（ADR-0020）。
            "platform_methods" => {
                let args: PlatformMethodsArgs = parse(args)?;
                self.platform_methods(&turn_config, sessions, &room, args)
            }
            "platform_call" => {
                let args: PlatformCallArgs = parse(args)?;
                self.platform_call(ticket, args)
            }
            // 反馈：诚实报告「这里卡住了」。这是出口，不是抱怨箱（ADR-0021）。
            "report_feedback" => {
                let args: FeedbackArgs = parse(args)?;
                self.report_feedback(ticket, &turn_session, &room, live, args)
            }
            // 记忆：文件层。
            "memory_read" => {
                let args: MemoryArgs = parse(args)?;
                self.memory_read(&turn_config, &turn_session, args)
            }
            "memory_write" => {
                let args: MemoryWriteArgs = parse(args)?;
                self.memory_write(&turn_config, &turn_session, args)
            }
            "memory_list" => {
                let args: MemoryListArgs = parse(args)?;
                self.memory_list(&turn_config, &turn_session, args)
            }
            "memory_search" => {
                let args: MemorySearchArgs = parse(args)?;
                self.memory_search(&turn_config, &turn_session, args)
            }
            // 记忆：git 层。
            "memory_git_status" => {
                let args: MemoryArgs = parse(args)?;
                self.memory_git_status(&turn_config, &turn_session, args)
            }
            "memory_git_log" => {
                let args: MemoryLogArgs = parse(args)?;
                self.memory_git_log(&turn_config, &turn_session, args)
            }
            "memory_git_diff" => {
                let args: MemoryArgs = parse(args)?;
                self.memory_git_diff(&turn_config, &turn_session, args)
            }
            "memory_git_blame" => {
                let args: MemoryBlameArgs = parse(args)?;
                self.memory_git_blame(&turn_config, &turn_session, args)
            }
            "memory_git_add" => {
                let args: MemoryAddArgs = parse(args)?;
                self.memory_git_add(&turn_config, &turn_session, args)
            }
            "memory_git_commit" => {
                let args: MemoryCommitArgs = parse(args)?;
                self.memory_git_commit(&turn_config, &turn_session, args)
            }
            "memory_git_push" => {
                let args: MemoryArgs = parse(args)?;
                self.memory_git_push(&turn_config, &turn_session, args)
            }
            "memory_git_branch" => {
                let args: MemoryArgs = parse(args)?;
                self.memory_git_branch(&turn_config, &turn_session, args)
            }
            other => Err(ToolError::new(
                ErrorCode::ToolNotAllowed,
                format!("没有这个工具：{other}"),
            )),
        }?;

        // 所有工具结果都从这一个口出去：超预算就分页，再不行就落盘（ADR-0022）。
        // 放在这里而不是各个工具里，是为了「新加的工具天然受保护」——
        // 以前每个接口各自截断，于是截断方式各不相同，最糟的一种是把 JSON 砍成半截字符串。
        //
        // `platform_call` 的唯一例外：它可能被 endpoint 拆成「锁内 prepare / 锁外
        // execute / 锁内 finish」，fit 已经在 finish 里做过一次；这里再 fit 会双重分页。
        if tool == "platform_call" {
            Ok(value)
        } else {
            Ok(render::fit(tool, value, &self.spill_dir()))
        }
    }

    /// endpoint 用的两段式入口：能在锁里做完的直接 `Done`，要等平台的返回
    /// `Platform`，由调用方放下 core 全局锁后执行，再回来 `finish_platform_call`。
    pub(crate) fn begin_tool_call(
        &mut self,
        ticket: &Ticket,
        sessions: &[String],
        tool: &str,
        args: Value,
    ) -> Result<ToolCallStart, ToolError> {
        match tool {
            "send_message" => {
                let args: SendArgs = parse(args)?;
                let ctx = self.turn_ctx(ticket, tool)?;
                let prepared = self.prepare_send(
                    ctx.config.clone(),
                    &ctx.session,
                    ctx.room.clone(),
                    SendOrigin::Message,
                    args.elements.0,
                )?;
                Ok(ToolCallStart::Send(prepared))
            }
            "send_message_to" => {
                let args: SendToArgs = parse(args)?;
                let target = resolve_room(&args.to)?;
                let ctx = self.turn_ctx(ticket, tool)?;
                self.ensure_can_send(&ctx.config, &ctx.session, &target)?;
                let prepared = self.prepare_send(
                    ctx.config.clone(),
                    &ctx.session,
                    target,
                    SendOrigin::Message,
                    args.elements.0,
                )?;
                Ok(ToolCallStart::Send(prepared))
            }
            "platform_call" => {
                let args: PlatformCallArgs = parse(args)?;
                Ok(ToolCallStart::Platform(
                    self.prepare_platform_call(ticket, args)?,
                ))
            }
            // 其余工具不碰外部 I/O（读流水/记忆/笔记都在本地），保持原路径。
            _ => {
                let value = self.tool_call(ticket, sessions, tool, args)?;
                Ok(ToolCallStart::Done(value))
            }
        }
    }

    /// 校验并组装一次 `platform_call`，但**不执行**平台 I/O。
    fn prepare_platform_call(
        &mut self,
        ticket: &Ticket,
        args: PlatformCallArgs,
    ) -> Result<PreparedPlatformCall, ToolError> {
        let ctx = self.turn_ctx(ticket, "platform_call")?;
        let sessions = std::slice::from_ref(&ctx.session);
        let room = self.resolve(&args.room, &ctx.room)?;
        let config = ctx.config.clone();

        let methods = self.outbound.catalog(&room.connection);
        if methods.is_empty() {
            return Err(no_catalog(&room.connection));
        }
        let Some(spec) = methods
            .iter()
            .find(|method| method.name.as_ref() == args.method.as_str())
        else {
            let hint = match methods
                .iter()
                .find(|method| method.group() == group_of_name(&args.method))
            {
                Some(near) => format!("你是不是想调 {}？", near.name),
                None => format!("这个连接支持：{}", method_names(&methods)),
            };
            return Err(ToolError::new(
                ErrorCode::BadRequest,
                format!("[unknown_method] 没有这个方法：{}；{hint}", args.method),
            ));
        };

        match spec.scope {
            Scope::Room => {
                // 只读方法和写方法分开判：策略可以只放开读（或只放开写）。
                let action = if spec.effect == Effect::Read {
                    AccessAction::Read
                } else {
                    AccessAction::Send
                };
                self.ensure_room_access(&ctx.config, sessions, action, &room)?;
            }
            Scope::Connection => {
                self.ensure_connection_readable(&ctx.config, sessions, &room.connection)?
            }
        }

        // 要写流水的方法必须先知道“我是谁”。放在平台调用之前查：平台已经发出去了
        // 再发现记不了账，那条消息就会在自己的水位上留洞。
        if spec.record != RecordMode::None && config.account_of_room(&room).is_empty() {
            return Err(ToolError::new(
                ErrorCode::Internal,
                format!("{room} 所属的连接没有配登录账号，不知道自己是谁"),
            ));
        }

        if spec.risk == Risk::Destructive
            && !Self::allows_destructive(&ctx.config, &room.connection, spec.name.as_ref())
        {
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
        check_target_params(spec, &room, &args.params)?;
        // params 里的本机文件路径换成中继 URL（ADR-0025）。这一步只碰本地文件，
        // 在锁里做没问题；真正的平台调用在锁外。
        let params = self.relay_params(args.params.clone())?;

        let outbound = self.outbound.handle_for(&room.connection).ok_or_else(|| {
            call_error(CallError::Unavailable(format!(
                "没有名为 {} 的连接，调不到 {}",
                room.connection, room
            )))
        })?;

        Ok(PreparedPlatformCall {
            room,
            spec: spec.clone(),
            params,
            args,
            config,
            outbound,
        })
    }

    /// 平台结果回来之后：发送类调用先记账，再做查询/分页，最后 fit。
    ///
    /// 记账必须做：模型用 `platform_call` 发的消息也是「自己说的话」。不写流水、
    /// 不确认，它就会在自己的水位上留一个洞，洞后面的新消息永远推不过去
    /// （同 `finish_send` 里 2026-10-31 那条教训）。记录内容由适配器翻译
    /// （`OutgoingRecord`），core 不认识平台消息段。
    pub(crate) fn finish_platform_call(
        &mut self,
        prepared: PreparedPlatformCall,
        result: Result<CallOutcome, CallError>,
    ) -> Result<Value, ToolError> {
        let PreparedPlatformCall {
            room,
            spec,
            args,
            config,
            ..
        } = prepared;
        let CallOutcome { value, record } = result.map_err(call_error)?;

        if spec.record != RecordMode::None {
            if let Some(record) = record {
                self.record_outgoing(&config, &room, record)
                    .map_err(internal)?;
            }
        }

        let grouped = args.query.group_by.is_some();
        let result = crate::query::apply_to_value(value, &args.query)
            .map_err(|error| ToolError::new(ErrorCode::BadRequest, error.to_string()))?;
        let result = if grouped {
            result
        } else {
            let limit = args
                .limit
                .unwrap_or(DEFAULT_PAGE_LIMIT)
                .clamp(1, MAX_PAGE_LIMIT);
            let offset = args.offset.unwrap_or(0);
            render::slice(result, offset, limit)
        };
        Ok(render::fit("platform_call", result, &self.spill_dir()))
    }

    /// 直连 `tool_call` 的 `platform_call` 路径：校验 + 执行 + 结算，仍在调用方锁里。
    /// endpoint 走 `begin_tool_call` / `finish_platform_call` 的锁外版本。
    fn platform_call(
        &mut self,
        ticket: &Ticket,
        args: PlatformCallArgs,
    ) -> Result<Value, ToolError> {
        let prepared = self.prepare_platform_call(ticket, args)?;
        let result = prepared.execute();
        self.finish_platform_call(prepared, result)
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
                read: turn.read.clone(),
                config: turn.config.clone(),
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
            read: ended.read,
            config: ended.config,
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

    /// 记下「本轮读过哪些编号」。只有当前轮房间的读取才算：`ack_messages`
    /// 只能确认当前轮房间，记到别的房间没有意义。
    fn mark_read(&mut self, ticket: &Ticket, room: &RoomId, ids: impl IntoIterator<Item = u64>) {
        let session = self.by_ticket.get(ticket).cloned();
        if let Some(session) = session {
            if let Some(turn) = self.active.get_mut(&session) {
                if &turn.room == room {
                    turn.read.extend(ids);
                }
            }
            return;
        }
        if let Some(turn) = self.ended.get_mut(ticket) {
            if &turn.room == room {
                turn.read.extend(ids);
            }
        }
    }

    /// 这个会话在这个房间里真正要处理的未读：水位之后、不在乱序确认里、且不是自己说的。
    ///
    /// 取全量再筛选，不再只取「最旧 200 条」——混投策略要同时拿到最旧一小段和最新一批，
    /// 只取最旧一段会让新消息永远排不上。
    fn pending_records(
        &self,
        session: &str,
        room: &RoomId,
        account: &str,
    ) -> Result<Vec<Record>, StorageError> {
        let cursor = Cursor::new(session, room.clone());
        let mut records = unread(&self.store, &cursor, usize::MAX)?;
        records.retain(|record| record.sender.id != account);
        Ok(records)
    }

    /// 把引用里的平台编号翻成本房间的水位编号（ADR-0024）。
    ///
    /// 平台给的是它自己的编号（`-1179324851`），模型手上是 `#621`。以前这一步得模型
    /// 自己做，做不了就只能猜；现在两个号码并排给出来。
    ///
    /// 查不到也留一个 `id: None` 的 `quote_ref`——「这个房间里没有那条」本身就是信息
    /// （被清理了，或者引用的是别的平台/别的房间的消息），比什么都不给强。
    fn fill_quote_refs(&self, room: &RoomId, records: &mut [Record]) {
        for record in records.iter_mut() {
            let Some(platform_id) = record.quote_of().map(str::to_owned) else {
                continue;
            };
            let id = self
                .store
                .located(room, &platform_id)
                .ok()
                .flatten()
                .map(|found| found.id);
            record.quote_ref = Some(QuoteRef { platform_id, id });
        }
    }

    fn subscribers_of(config: &Config, room: &RoomId) -> Vec<Subscriber> {
        let account = config.account_of_room(room);
        config
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

    /// 用会话自己的访问策略判定一次访问。
    ///
    /// provider 只回答允许/拒绝；拒绝的形状（错误码、文案）由 core 统一组织。
    /// `sessions` 取并集：任何一个会话被允许就算通过（工具面目前只传当前会话一个）。
    fn check_access(
        &self,
        config: &Config,
        sessions: &[String],
        action: AccessAction,
        room: Option<&RoomId>,
        connection: &str,
    ) -> Result<(), ToolError> {
        let mut denied = Vec::new();
        for id in sessions {
            let Some(session) = config.session(id) else {
                continue;
            };
            let provider = self
                .plugins
                .resolve_access(&session.access.provider, &session.access)
                .map_err(|error| {
                    ToolError::new(
                        ErrorCode::Internal,
                        format!(
                            "[internal] 会话 {id} 的访问策略 {} 不可用：{error}",
                            session.access.provider
                        ),
                    )
                })?;
            let subscriptions: Vec<RoomId> = session
                .subscriptions
                .iter()
                .map(|subscription| subscription.room.clone())
                .collect();
            let request = AccessRequest {
                session: id,
                connection,
                room,
                action,
                subscriptions: &subscriptions,
            };
            if provider.check(&session.access, &request) == AccessDecision::Allow {
                return Ok(());
            }
            denied.push(format!("{}（{}）", id, session.access.provider));
        }

        let detail = match action {
            AccessAction::Connection => format!(
                "会话 {} 在连接 {connection} 上没有可访问的房间",
                sessions.join(", ")
            ),
            AccessAction::Read | AccessAction::Send => format!(
                "会话 {} 没有访问 {} 的权限",
                sessions.join(", "),
                room.map(ToString::to_string).unwrap_or_default()
            ),
        };
        let policy = if denied.is_empty() {
            String::new()
        } else {
            format!("（访问策略 {}）", denied.join("；"))
        };
        Err(ToolError::new(
            ErrorCode::RoomNotSubscribed,
            format!("{detail}{policy}"),
        ))
    }

    /// 能不能对 `room` 做 `action`。
    fn ensure_room_access(
        &self,
        config: &Config,
        sessions: &[String],
        action: AccessAction,
        room: &RoomId,
    ) -> Result<(), ToolError> {
        self.check_access(
            config,
            sessions,
            action,
            Some(room),
            &room.connection,
        )
    }

    /// 读的工具默认要求「能读这个房间」。具体边界由会话的访问策略决定。
    fn ensure_readable(
        &self,
        config: &Config,
        sessions: &[String],
        room: &RoomId,
    ) -> Result<(), ToolError> {
        self.ensure_room_access(config, sessions, AccessAction::Read, room)
    }

    /// 当前会话能不能往这个房间发。`send_message_to` 的边界。
    fn ensure_can_send(
        &self,
        config: &Config,
        session: &str,
        room: &RoomId,
    ) -> Result<(), ToolError> {
        let sessions = [session.to_owned()];
        self.ensure_room_access(config, &sessions, AccessAction::Send, room)
    }

    /// 连接级权限：访问策略决定这条连接上有没有可用的房间。
    ///
    /// 好友列表、账号资料这类方法跟房间无关，所以不要求模型报出一个具体房间；
    /// 但也不能凭空调一个自己完全够不着的号。
    fn ensure_connection_readable(
        &self,
        config: &Config,
        sessions: &[String],
        connection: &str,
    ) -> Result<(), ToolError> {
        self.check_access(config, sessions, AccessAction::Connection, None, connection)
    }

    /// 解析一次平台调用落在哪个房间上，并按方法声明的 [`Scope`] 做权限校验。
    ///
    /// 房间级的必须落在一个具体房间上；连接级的只借 `room` 选连接，不给就用当前轮次的。
    fn resolve_capability_room(
        &self,
        config: &Config,
        sessions: &[String],
        given: &Option<String>,
        turn_room: &RoomId,
        scope: Scope,
    ) -> Result<RoomId, ToolError> {
        let room = self.resolve(given, turn_room)?;
        match scope {
            Scope::Room => self.ensure_readable(config, sessions, &room)?,
            Scope::Connection => {
                self.ensure_connection_readable(config, sessions, &room.connection)?
            }
        }
        Ok(room)
    }

    /// 时钟评估：检查每个订阅的时钟策略，到点就把（会话, 房间）排进队列。
    ///
    /// 只做廉价检查：解析 provider 时用当前配置快照，到点才开轮。返回本次排了几轮。
    pub fn tick_clock(&mut self, now: i64) -> usize {
        // 先把订阅整理出来，避免遍历 config 的同时改 self 的其他字段。
        let clocks: Vec<(String, Vec<(RoomId, ClockPolicy)>)> = self
            .config
            .sessions
            .iter()
            .map(|session| {
                (
                    session.id.clone(),
                    session
                        .subscriptions
                        .iter()
                        .map(|subscription| (subscription.room.clone(), subscription.clock.clone()))
                        .collect(),
                )
            })
            .collect();

        let mut queued = 0;
        for (session, subscriptions) in clocks {
            for (room, policy) in subscriptions {
                let key = (session.clone(), room.clone());
                let Some(last_clock_at) = self.clock_last.get(&key).copied() else {
                    // 第一次见到这个订阅：从这一 tick 开始计间隔，不立刻开轮。
                    self.clock_last.insert(key, now);
                    continue;
                };
                // 会话正在跑，或这个房间已经排着队：等它空下来再说，别叠轮次。
                if self.active.contains_key(&session)
                    || self
                        .pending
                        .iter()
                        .any(|pending| pending.session == session && pending.room == room)
                {
                    continue;
                }
                let provider = match self.plugins.resolve_clock(&policy.provider, &policy) {
                    Ok(provider) => provider,
                    Err(error) => {
                        eprintln!(
                            "时钟策略 {} 解析失败，跳过 {session}（{room}）：{error}",
                            policy.provider
                        );
                        continue;
                    }
                };
                let last_record_at = match self.latest_record_at(&room) {
                    Ok(at) => at,
                    Err(error) => {
                        eprintln!("读 {room} 最后一条记录失败：{error}");
                        None
                    }
                };
                let context = ClockContext {
                    session: &session,
                    room: &room,
                    now,
                    last_clock_at,
                    last_record_at,
                };
                if provider.is_due(&policy, &context) {
                    self.enqueue(session.clone(), room.clone(), TurnReason::Clock);
                    self.clock_last.insert(key, now);
                    queued += 1;
                }
            }
        }
        queued
    }

    fn latest_record_at(&self, room: &RoomId) -> Result<Option<i64>, BridgeError> {
        let Some(id) = self.store.latest(room)? else {
            return Ok(None);
        };
        let Some(record) = self
            .store
            .since(room, id.saturating_sub(1), 1)?
            .into_iter()
            .next()
        else {
            return Ok(None);
        };
        Ok(Some(record.at))
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
        // 开轮时冻结配置：这一轮后续的工具查询都用它，热重载只影响下一轮。
        let turn_config = Arc::new(self.config.clone());
        let account = turn_config.account_of_room(&pending.room);
        self.heal_own_holes(&pending.session, &pending.room, &account)?;
        let mut records = self.pending_records(&pending.session, &pending.room, &account)?;
        // 引用里的平台编号翻成本房间编号，跟平台编号并排放（ADR-0024）。
        self.fill_quote_refs(&pending.room, &mut records);

        // 先选本轮真正要投的批次，再计数、再进 delivered：
        // 没投出去的记录不能算「投过」，否则 ack_messages 会放行没见过的编号，
        // 投递次数也会被白白加上一次。
        //
        // 消息过多时给最旧一小段（让水位能继续前进）+ 最新一批（让模型看得到
        // 眼前的话），中间那段跳过并明确告诉模型编号区间。
        let limit = if turn_config.delivery_limit == 0 {
            MAX_DELIVERY
        } else {
            turn_config.delivery_limit.min(MAX_DELIVERY)
        };
        // composer 只决定「投哪些编号」；它看不到 store、票和水位，选出来的编号
        // 还要在这里过一遍真实记录集合——未知编号、重复编号都不会凭空进 delivered。
        let delivery_policy = turn_config
            .session(&pending.session)
            .map(|session| session.delivery.clone())
            .unwrap_or_default();
        let composer = match self
            .plugins
            .resolve_delivery(&delivery_policy.provider, &delivery_policy)
        {
            Ok(composer) => composer,
            Err(error) => {
                eprintln!(
                    "投递策略 {} 解析失败，回退内置 composer：{error}",
                    delivery_policy.provider
                );
                Arc::new(DefaultDeliveryComposer)
            }
        };
        let plan = composer.compose(
            &delivery_policy,
            &DeliveryRequest {
                session: &pending.session,
                room: &pending.room,
                records: &records,
                limit,
            },
        );
        let omitted = plan.omitted;
        let more = omitted.map(|omitted| omitted.count);
        let mut by_id: HashMap<u64, Record> = records.into_iter().map(|r| (r.id, r)).collect();
        let mut selected = Vec::with_capacity(plan.keep.len());
        for id in &plan.keep {
            if let Some(record) = by_id.remove(id) {
                selected.push(record);
            }
        }
        records = selected;

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
        let ticket = Ticket::new(format!(
            "{}-{}-{}",
            pending.session, self.ticket_prefix, self.ticket_seq
        ))
        .expect("票面非空");

        self.by_ticket
            .insert(ticket.clone(), pending.session.clone());
        self.active.insert(
            pending.session.clone(),
            ActiveTurn {
                session: pending.session.clone(),
                room: pending.room.clone(),
                delivered,
                read: BTreeSet::new(),
                config: turn_config.clone(),
            },
        );

        // 把这个会话引用的记忆连同 worktree 路径一起带给 harness。惰性初始化：worktree
        // 第一次用到才 checkout，之后复用。
        let memories = self.session_memories(&turn_config, &pending.session);

        // 会话的身份信息：harness 据此知道「以什么身份说话、用哪个模型」。
        let session_info = turn_config
            .session(&pending.session)
            .map(|session| SessionInfo {
                id: session.id.clone(),
                name: session.name.clone(),
                harness: session.harness.clone(),
                model: session.model.clone(),
            });
        // 人格卡：桥原样下发，怎么塞进模型上下文由 harness 适配器决定。
        let persona = turn_config
            .session(&pending.session)
            .and_then(|session| turn_config.persona(&session.persona))
            .map(|persona| PersonaInfo {
                id: persona.id.clone(),
                name: persona.name.clone(),
                text: persona.text.clone(),
            });

        // 批次已经由 composer 选好（默认是最旧一小段 + 最新一批），这里直接用它。
        Ok(Delivery {
            ticket,
            session: pending.session,
            room: pending.room,
            records,
            reason: pending.reason,
            memories,
            session_info,
            persona,
            more,
            omitted,
            // 0 = 配置里关了超时，harness 收到就别自己掐。
            turn_timeout_ms: Some(turn_config.turn_timeout_ms()),
        })
    }

    /// 开轮前自愈：把水位之上「本连接账号自己发的」记录确认掉。
    ///
    /// 自己发的消息不算未读（投递时会被过滤），但水位只推连续前沿——这些记录
    /// 如果停在水位之上，就是前沿迈不过去的洞，洞后面的真消息会被反复重投。
    /// 历史上 deliver_out 只确认发送者自己的会话（2026-10-31 修），留下的旧洞
    /// 也靠这里在下次开轮时自动补上，不用手工修库。
    fn heal_own_holes(
        &mut self,
        session: &str,
        room: &RoomId,
        account: &str,
    ) -> Result<(), BridgeError> {
        if account.is_empty() {
            return Ok(());
        }
        let cursor = Cursor::new(session, room.clone());
        let watermark = self.store.watermark(&cursor)?;
        let own: Vec<u64> = self
            .store
            .since(room, watermark, usize::MAX)?
            .into_iter()
            .filter(|record| record.sender.id == account)
            .map(|record| record.id as u64)
            .collect();
        if own.is_empty() {
            return Ok(());
        }
        self.store.ack(&cursor, &own)?;
        Ok(())
    }

    /// 会话引用的记忆列表（id + worktree 路径）。worktree 惰性 checkout，失败这一轮就
    /// 开不成——记忆配错了要早点暴露，别让 harness 拿到一个碰不了的路径。
    fn session_memories(&self, config: &Config, session: &str) -> Vec<MemoryInfo> {
        let Some(session_cfg) = config.session(session) else {
            return Vec::new();
        };
        session_cfg
            .memories
            .iter()
            .filter_map(|memory_id| {
                let memory = config.memory(memory_id)?;
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
        config: Arc<Config>,
        session: &str,
        room: &RoomId,
        origin: SendOrigin,
        elements: Vec<Element>,
    ) -> Result<Value, ToolError> {
        let prepared = self.prepare_send(config, session, room.clone(), origin, elements)?;
        let sent = prepared.execute();
        self.finish_send(prepared, sent)
    }

    /// 校验、切调用标记、跑发送前钩子、中继本机文件、查账号、取出发通道——但**不发送**。
    /// endpoint 可以拿着这个结果放下全局锁，真正等平台。
    fn prepare_send(
        &mut self,
        config: Arc<Config>,
        session: &str,
        room: RoomId,
        origin: SendOrigin,
        mut elements: Vec<Element>,
    ) -> Result<PreparedSend, ToolError> {
        // 先把混进正文的「调用标记」切掉。这不是洁癖：群里真的出现过带
        // `</mcp__acb__send_message>` 和特殊 token 的回复（见 sanitize 模块）。
        // 切完落空的文本段直接丢，免得往平台上发一个空段。
        let trimmed = crate::sanitize::trim_elements(&mut elements);
        if trimmed {
            elements.retain(
                |element| !matches!(element, Element::Text { text } if text.trim().is_empty()),
            );
        }
        if elements.is_empty() {
            return Err(ToolError::new(
                ErrorCode::BadRequest,
                if trimmed {
                    "[bad_params] 你的 text 整个都是调用标记（`</text>` 之类），切完没有正文可发。\
                     把要说的话单独写一遍——参数要放在各自的字段里，别写进正文。"
                } else {
                    "没有内容可发"
                },
            ));
        }

        // 发送前钩子：按会话配置的顺序跑，可改写正文（例如加签名、脱敏）。
        // 钩子失败就让这次发送失败，绝不静默跳过——否则「以为挂了钩子」只是错觉。
        let outbound_policy = config
            .session(session)
            .map(|session| session.outbound.clone())
            .unwrap_or_default();
        let before_hooks = self
            .plugins
            .resolve_send_hooks(&outbound_policy.before)
            .map_err(|error| {
                ToolError::new(
                    ErrorCode::Internal,
                    format!("发送前钩子解析失败：{error}"),
                )
            })?;
        let after_hooks = self
            .plugins
            .resolve_send_hooks(&outbound_policy.after)
            .map_err(|error| {
                ToolError::new(
                    ErrorCode::Internal,
                    format!("发送后钩子解析失败：{error}"),
                )
            })?;
        let hook_context = SendContext {
            session,
            room: &room,
            origin,
        };
        for hook in &before_hooks {
            hook.before_send(&hook_context, &mut elements)
                .map_err(|error| {
                    ToolError::new(
                        ErrorCode::Internal,
                        format!("发送前钩子失败：{error}"),
                    )
                })?;
        }
        if elements.is_empty() {
            return Err(ToolError::new(
                ErrorCode::BadRequest,
                "发送前钩子把正文清空了；检查会话的 [sessions.outbound].before 配置",
            ));
        }

        // 本机文件换成平台取得到的 URL（ADR-0025）。配了 [file_relay] 才有动作；
        // 没配、或路径不是本机存在的文件，就原样透传——平台侧路径（共享卷等）不受影响。
        let relayed = self.relay_elements(&mut elements)?;

        let account = config.account_of_room(&room);
        if account.is_empty() {
            return Err(ToolError::new(
                ErrorCode::Internal,
                format!("{room} 所属的连接没有配登录账号，不知道自己是谁"),
            ));
        }

        let outbound = self.outbound.handle_for(&room.connection).ok_or_else(|| {
            ToolError::new(
                ErrorCode::Internal,
                format!("没有名为 {} 的连接，发不到 {room}", room.connection),
            )
        })?;

        Ok(PreparedSend {
            room,
            elements,
            config,
            outbound,
            trimmed,
            relayed,
            session: session.to_owned(),
            origin,
            after_hooks,
        })
    }

    /// 出站结果回来之后：写流水、对所有订阅者 ack、拼回执。
    pub(crate) fn finish_send(
        &mut self,
        prepared: PreparedSend,
        result: Result<Sent, OutboundError>,
    ) -> Result<Value, ToolError> {
        let PreparedSend {
            room,
            elements,
            config,
            trimmed,
            relayed,
            session,
            origin,
            after_hooks,
            ..
        } = prepared;

        let sent = match result {
            Ok(sent) => sent,
            Err(error) => {
                // 平台侧拉不到媒体 URL 时的报错只有一句干巴巴的 fetch failed
                // （平台内部自己的下载失败，桥拿不到它的 errno）。桥补得上的是
                // 上下文：哪些 URL、很可能只有发消息这侧可达、本机文件该怎么写。
                let mut tool_error = outbound_error(error);
                let urls: Vec<&str> = elements.iter().flat_map(|e| e.media_urls()).collect();
                if !urls.is_empty() {
                    tool_error.message.push_str(&format!(
                        "\n[hint] 这条消息带了媒体：{}。平台是自己去下载这个地址的，\
                         只有你这一侧可达的地址（本机端口、你刚生成的服务的地址）它会拉不到。\
                         本机文件直接把路径写进 url，桥会自动中继成平台取得到的地址。",
                        urls.join(", ")
                    ));
                }
                return Err(tool_error);
            }
        };

        // 发送成功后跑观察钩子。它们只拿到回执，不能改已经发出去的内容。
        let hook_context = SendContext {
            session: &session,
            room: &room,
            origin,
        };
        for hook in &after_hooks {
            hook.after_send(&hook_context, &sent);
        }

        let account = config.account_of_room(&room);
        let (record, watermark) = self
            .record_and_ack(
                &config,
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

        // 回执要「像回执」：ok 摆在第一位，发出去了才给 platform_id / at。
        // 失败时这个函数直接返回 Err（调用方拿到的是 error），绝不会出现
        // 「一脸成功但平台没收到」的形状——模型据此判断能不能去群里说「已经发了」。
        let mut receipt = json!({
            "ok": true,
            "id": record.id,
            "platform_id": sent.platform_id,
            "at": sent.at,
            "room": room.to_string(),
            "watermark": watermark,
        });
        if trimmed {
            // 必须说：模型以为发出去的是全文，实际少了尾巴。不说它就永远不知道。
            receipt["trimmed"] = json!(true);
            receipt["note"] = json!(
                "你的 text 里混着调用标记（`</text>`、`<｜｜DSML｜｜` 之类），\
                 已经在第一个标记处截断，后面那截没发出去。\
                 那串东西是解析工具调用时漏进来的，不是正文；要传的参数放各自字段里，别写进 text。"
            );
        }
        if !relayed.is_empty() {
            // 也必须说：模型给的是本机路径，实际发出去的是中继 URL。
            receipt["relayed"] = json!(
                relayed
                    .iter()
                    .map(|(from, to)| json!({"from": from, "to": to}))
                    .collect::<Vec<_>>()
            );
        }
        Ok(receipt)
    }

    /// 适配器翻译好的出站记录 → 流水。
    ///
    /// 只由 `finish_platform_call` 调用：发送类平台方法（`MethodSpec::record` 非空）
    /// 成功后由适配器带回实际发出去的内容。平台编号、时间缺省时用本地时间。
    fn record_outgoing(
        &mut self,
        config: &Config,
        room: &RoomId,
        record: OutgoingRecord,
    ) -> Result<(Record, u64), BridgeError> {
        let account = config.account_of_room(room);
        let sender = Sender {
            id: account.clone(),
            name: account,
            role: None,
        };
        let at = record.at.unwrap_or_else(now_millis);
        let mut draft = match record.kind {
            OutgoingKind::Message => {
                RecordDraft::message(room.clone(), sender, at, record.elements, false)
            }
            OutgoingKind::Event(event) => {
                RecordDraft::event(room.clone(), sender, at, event, record.elements, false)
            }
        };
        if let Some(platform_id) = record.platform_id {
            draft = draft.with_platform_id(platform_id);
        }
        self.record_and_ack(config, draft)
    }

    /// 写一条「自己发出去的」记录，并对**所有订阅者**确认。
    ///
    /// 必须给所有订阅者确认，不能只确认发消息的这个会话。血的教训（2026-10-31 真机）：
    /// 一个群被两个会话订阅时（dsh 和 dsh-group2），A 会话发的消息只在 A 的水位上确认，
    /// B 的水位上永远留一个洞——而水位只推连续前沿，洞后面的消息就永远算未读，
    /// 表现为「同一批积压反复重投」。
    fn record_and_ack(
        &mut self,
        config: &Config,
        draft: RecordDraft,
    ) -> Result<(Record, u64), BridgeError> {
        let room = draft.room.clone();
        let record = self.store.append(draft)?;
        let mut watermark = 0;
        for subscriber in Self::subscribers_of(config, &room) {
            let cursor = Cursor::new(subscriber.session, room.clone());
            watermark = self.store.ack(&cursor, &[record.id])?;
        }
        Ok((record, watermark))
    }

    /// 两段式发送结算后，endpoint 用同一个口做分页/落盘。
    pub(crate) fn fit_tool_result(&self, tool: &str, value: Value) -> Value {
        render::fit(tool, value, &self.spill_dir())
    }

    /// 把元素里的本机文件收进中继，换成平台取得到的 URL。返回 (原路径, URL) 列表。
    ///
    /// 只动「本机真实存在」的路径：本机没有的路径可能是平台那边才存在的
    /// （共享卷、平台自己的缓存），动它反而弄坏原本能用的写法。
    fn relay_elements(
        &mut self,
        elements: &mut [Element],
    ) -> Result<Vec<(String, String)>, ToolError> {
        let Some(relay) = &self.relay else {
            return Ok(Vec::new());
        };
        let mut relayed = Vec::new();
        for element in elements.iter_mut() {
            let url = match element {
                Element::Image { url, .. } | Element::Audio { url } | Element::Video { url } => url,
                Element::File { url, .. } => url,
                _ => continue,
            };
            let Some(local) = crate::relay::local_path(url) else {
                continue;
            };
            if !local.is_file() {
                continue;
            }
            match relay.publish(&local) {
                Ok(published) => {
                    relayed.push((url.clone(), published.clone()));
                    *url = published;
                }
                Err(error) => {
                    return Err(ToolError::new(
                        ErrorCode::BadRequest,
                        format!(
                            "[bad_params] 文件 {} 本机存在，但桥收不下来：{error}。\
                             平台拿不到这个文件。先看 [file_relay].dir 能不能写；\
                             小文件（base64 后 1 MiB 内）也可以把 url 写成 base64://… 内联。",
                            local.display()
                        ),
                    ));
                }
            }
        }
        Ok(relayed)
    }

    /// 把 params 里的本机文件路径换成中继 URL（ADR-0025）。
    ///
    /// 平台调用（upload_file 这类）的文件路径在 params 里，不在元素里，是同一个断层。
    /// 递归遍历，只动「本机真实存在」的绝对路径，其余字符串原样保留。
    fn relay_params(&self, params: serde_json::Value) -> Result<serde_json::Value, ToolError> {
        fn walk(
            relay: &crate::relay::FileRelay,
            value: serde_json::Value,
        ) -> Result<serde_json::Value, ToolError> {
            match value {
                serde_json::Value::String(text) => {
                    let Some(local) = crate::relay::local_path(&text) else {
                        return Ok(serde_json::Value::String(text));
                    };
                    if !local.is_file() {
                        return Ok(serde_json::Value::String(text));
                    }
                    relay
                        .publish(&local)
                        .map(serde_json::Value::String)
                        .map_err(|error| {
                            ToolError::new(
                                ErrorCode::BadRequest,
                                format!(
                                    "[bad_params] params 里的文件 {} 本机存在，\
                                 但桥收不下来：{error}。小文件可以改用 base64:// 内联。",
                                    local.display()
                                ),
                            )
                        })
                }
                serde_json::Value::Array(items) => items
                    .into_iter()
                    .map(|item| walk(relay, item))
                    .collect::<Result<Vec<_>, _>>()
                    .map(serde_json::Value::Array),
                serde_json::Value::Object(map) => map
                    .into_iter()
                    .map(|(key, value)| walk(relay, value).map(|value| (key, value)))
                    .collect::<Result<serde_json::Map<_, _>, _>>()
                    .map(serde_json::Value::Object),
                other => Ok(other),
            }
        }

        match &self.relay {
            Some(relay) => walk(relay, params),
            None => Ok(params),
        }
    }

    /// 确认一批记录。
    ///
    /// 三条规矩，都是为了让「确认」这件事**幂等**：
    /// 1. 已经确认过的编号（<= 当前水位）当成功跳过。重复确认不是错误——以前它报
    ///    bad_request，模型于是以为「确认失败了」，接着往下不再确认，那几条就被反复重投。
    /// 2. 只能确认「本轮投给过这个会话」或「本轮通过读工具读到过」的编号；别的编号
    ///    报 `not_delivered`。读到但没投递的也能确认，补读才能把水位推着走。
    /// 3. 确认是显式动作，桥不会替 agent 自动推水位。
    fn ack(
        &mut self,
        session: &str,
        room: &RoomId,
        delivered: &BTreeSet<u64>,
        read: &BTreeSet<u64>,
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
            if !delivered.contains(&id) && !read.contains(&id) {
                return Err(ToolError::new(
                    ErrorCode::BadRequest,
                    format!(
                        "[not_delivered] #{id} 这一轮既没投给你，也没读过，确认不了。\
                         这一轮投给你的：{delivered}；读过的：{read}。\
                         已经确认过的编号不用再确认（跳过去就行）。",
                        delivered = delivery_range(delivered),
                        read = if read.is_empty() {
                            "（没有）".to_owned()
                        } else {
                            delivery_range(read)
                        }
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
        &mut self,
        ticket: &Ticket,
        config: &Config,
        sessions: &[String],
        turn_room: &RoomId,
        args: HistoryArgs,
    ) -> Result<Value, ToolError> {
        let room = self.resolve(&args.room, turn_room)?;
        self.ensure_readable(config, sessions, &room)?;

        let columns = column_set(args.columns.as_deref())?;

        let limit = args.limit.unwrap_or(DEFAULT_HISTORY).min(MAX_HISTORY);
        let mut records: Vec<Record> = self
            .store
            .since(&room, 0, usize::MAX)
            .map_err(internal)?
            .into_iter()
            .filter(|record| args.after.is_none_or(|after| record.id > after))
            .filter(|record| args.before.is_none_or(|before| record.id < before))
            .collect();

        // 总数一定要给：不给的话「还有多少条」只能靠猜，猜错就以为翻到底了。
        let total = records.len();
        // 两个方向：
        // - 给了 `after`：从旧往新翻（补看更新的消息），一次给最早的 limit 条；
        // - 没给 `after`：原来的行为，从新往旧翻（看更早的历史）。
        let (has_more, next_before, next_after) = if args.after.is_some() {
            records.truncate(limit);
            let has_more = total > records.len();
            let next_after = if has_more {
                records.last().map(|record| record.id)
            } else {
                None
            };
            (has_more, None, next_after)
        } else {
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
            (has_more, next_before, None)
        };
        // 读到的编号记进本轮 read set：补读过的未投递内容可以直接确认。
        self.mark_read(ticket, &room, records.iter().map(|record| record.id));
        self.fill_quote_refs(&room, &mut records);

        let rendered: Vec<Value> = records
            .iter()
            .map(|r| record_columns(r, &columns))
            .collect();
        Ok(json!({
            "room": room,
            "columns": columns,
            "total": total,
            "returned": rendered.len(),
            "has_more": has_more,
            "next_before": next_before,
            "next_after": next_after,
            "records": rendered,
        }))
    }

    /// 按水位编号精确查一批记录，只返回指定列。
    fn fetch_records(
        &mut self,
        ticket: &Ticket,
        config: &Config,
        sessions: &[String],
        turn_room: &RoomId,
        args: FetchRecordsArgs,
    ) -> Result<Value, ToolError> {
        let room = self.resolve(&args.room, turn_room)?;
        self.ensure_readable(config, sessions, &room)?;

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

        // 查出几条第几条，都算本轮读过：之后 `ack_messages` 可以确认它们。
        self.mark_read(ticket, &room, records.iter().map(|record| record.id));

        // 找出没查到的编号，一起回给调用方，别让 agent 以为「那些 id 不存在」。
        let mut records = records;
        self.fill_quote_refs(&room, &mut records);
        let found: std::collections::BTreeSet<u64> = records.iter().map(|r| r.id).collect();
        let missing: Vec<u64> = wanted.difference(&found).copied().collect();

        let rendered: Vec<Value> = records
            .iter()
            .map(|r| record_columns(r, &columns))
            .collect();
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
    fn list_rooms(&self, config: &Config, sessions: &[String]) -> Result<Value, ToolError> {
        let mut rooms = Vec::new();
        for id in sessions {
            let Some(session) = config.session(id) else {
                continue;
            };
            for subscription in &session.subscriptions {
                let room = &subscription.room;
                let account = config.account_of_room(room);
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
        config: &Config,
        sessions: &[String],
        turn_room: &RoomId,
        args: NotesArgs,
    ) -> Result<Value, ToolError> {
        let room = self.resolve(&args.room, turn_room)?;
        self.ensure_readable(config, sessions, &room)?;

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
        config: &Config,
        sessions: &[String],
        turn_room: &RoomId,
        args: SetNoteArgs,
    ) -> Result<Value, ToolError> {
        let room = self.resolve(&args.room, turn_room)?;
        self.ensure_readable(config, sessions, &room)?;

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
        config: &Config,
        sessions: &[String],
        turn_room: &RoomId,
        args: DelNoteArgs,
    ) -> Result<Value, ToolError> {
        let room = self.resolve(&args.room, turn_room)?;
        self.ensure_readable(config, sessions, &room)?;

        let kind = match args.kind.as_deref() {
            Some(text) => parse_note_kind(text)?,
            None => NoteKind::Term,
        };
        let key = NoteKey::new(room, kind, args.key);
        let removed = self.store.delete_note(&key).map_err(internal)?;
        Ok(json!({ "removed": removed }))
    }

    fn events(
        &mut self,
        ticket: &Ticket,
        config: &Config,
        sessions: &[String],
        turn_room: &RoomId,
        args: EventsArgs,
    ) -> Result<Value, ToolError> {
        let room = self.resolve(&args.room, turn_room)?;
        self.ensure_readable(config, sessions, &room)?;

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
        self.mark_read(ticket, &room, page.iter().map(|record| record.id));
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
        config: &Config,
        sessions: &[String],
        turn_room: &RoomId,
        args: PlatformMethodsArgs,
    ) -> Result<Value, ToolError> {
        // 目录本身就是连接级的东西：同一个号的哪个房间，能做的事都一样。
        // 所以这里不要求模型报出一个具体房间，只要它订阅了这条连接就行。
        let room = self.resolve_capability_room(
            config,
            sessions,
            &args.room,
            turn_room,
            Scope::Connection,
        )?;

        let methods = self.outbound.catalog(&room.connection);
        if methods.is_empty() {
            return Err(no_catalog(&room.connection));
        }
        let room_text = room.to_string();
        let connection = room.connection.clone();
        let platform = Self::platform_of(config, &room.connection).to_owned();

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
                "tags": tags_of(&methods),
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
            .filter(|method| terms.is_empty() || terms.iter().any(|term| method.matches(term)))
            .collect();

        if hits.is_empty() {
            return Err(no_match(&methods, group, tag, query.as_deref()));
        }

        Ok(json!({
            "room": room_text,
            "connection": connection,
            "platform": platform,
            "total": methods.len(),
            "matched": hits.len(),
            "methods": hits.iter().map(|method| describe_method(method)).collect::<Vec<_>>(),
            "hint": "每条都带一个 example：照它把 params 里的号/内容换掉，就能直接喂给 platform_call。",
        }))
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
        // 同上：dsh 的解析器实测会把整段 `<invoke>`（含后面的参数标签）塞进 text。
        // 不切的话，反馈列表上看到的就是「正文 + 一串标签」，人根本没法读。
        let (body, markup_trimmed) = crate::sanitize::trim_call_markup(args.text.trim());
        if body.is_empty() {
            return Err(ToolError::new(
                ErrorCode::BadRequest,
                if markup_trimmed {
                    "[bad_params] 你的 text 整个都是调用标记（`</text>` 之类），切完没有正文。\
                     把「被什么卡住了」写成正文再说一遍，参数放各自字段。"
                } else {
                    "[bad_params] text 不能为空：把「你被什么卡住了」写进来，一两句话就行。"
                },
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
        let (title_raw, _) = split_first_line(&body);
        let (title, title_clipped) = clip(title_raw, MAX_FEEDBACK_TITLE);
        let (body, body_clipped) = clip(&body, MAX_FEEDBACK_BODY);
        let suggestion = args
            .suggestion
            .as_deref()
            .map(str::trim)
            .filter(|text| !text.is_empty());
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
            tool: args
                .tool
                .as_deref()
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(str::to_owned),
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
        if markup_trimmed {
            // 单独标一个键：这不是「写太长被截」，是「参数写错了地方」。
            truncated.insert("text_markup".into(), json!(true));
        }

        let mut note = if recorded.duplicate {
            "这条已经提过（同样的标题），次数已累加，不用再提。接着干活就行。".to_owned()
        } else {
            "记下了。这是出口，不是许愿池：写清「在哪一步卡住」比写「希望支持 X」有用得多。"
                .to_owned()
        };
        if markup_trimmed {
            note.push_str(
                " 另外：你的 text 里混着一串调用标记（`</text>` 之类），已经在第一个标记处截断——\
                 那是解析工具调用时漏进来的，不是正文。参数请放在各自的字段里，别写进 text。",
            );
        }

        Ok(json!({
            "ok": true,
            "id": recorded.id,
            "duplicate": recorded.duplicate,
            "repeats": recorded.repeats,
            "occurrences": recorded.occurrences,
            "truncated": truncated,
            "refs_dropped": refs_dropped,
            "note": note,
        }))
    }

    /// 连接名 → 平台名。只给展示用，查不到就说 unknown，不要因此拒绝服务。
    fn platform_of<'a>(config: &'a Config, connection: &str) -> &'a str {
        config
            .connections
            .iter()
            .find(|item| item.id == connection)
            .map(|item| item.platform.as_str())
            .unwrap_or("unknown")
    }

    /// 这条连接的配置放不放行这个破坏性方法。查不到连接 = 不放行。
    fn allows_destructive(config: &Config, connection: &str, method: &str) -> bool {
        config
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
        config: &Config,
        session: &str,
        memory: &str,
    ) -> Result<std::path::PathBuf, ToolError> {
        let Some(session_cfg) = config.session(session) else {
            return Err(ToolError::new(
                ErrorCode::Internal,
                format!("会话 {session} 不存在"),
            ));
        };
        self.memory
            .resolve_for(session, memory, &session_cfg.memories, &|memory_id| {
                config.memory(memory_id).map(|m| m.location.clone())
            })
    }

    fn memory_read(
        &self,
        config: &Config,
        session: &str,
        args: MemoryArgs,
    ) -> Result<Value, ToolError> {
        let worktree = self.memory_worktree(config, session, &args.memory)?;
        let content = read_file(&worktree, &args.path)?;
        Ok(json!({ "path": args.path, "content": content }))
    }

    fn memory_write(
        &self,
        config: &Config,
        session: &str,
        args: MemoryWriteArgs,
    ) -> Result<Value, ToolError> {
        let worktree = self.memory_worktree(config, session, &args.memory)?;
        write_file(&worktree, &args.path, &args.content)?;
        Ok(json!({ "path": args.path, "written": true }))
    }

    fn memory_list(
        &self,
        config: &Config,
        session: &str,
        args: MemoryListArgs,
    ) -> Result<Value, ToolError> {
        let worktree = self.memory_worktree(config, session, &args.memory)?;
        let entries = list_dir(&worktree, &args.path)?;
        let total = entries.len();
        let offset = args.offset.unwrap_or(0);
        let limit = args
            .limit
            .unwrap_or(DEFAULT_PAGE_LIMIT)
            .clamp(1, MAX_PAGE_LIMIT);
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

    fn memory_search(
        &self,
        config: &Config,
        session: &str,
        args: MemorySearchArgs,
    ) -> Result<Value, ToolError> {
        let worktree = self.memory_worktree(config, session, &args.memory)?;
        let hits = search(&worktree, &args.path, &args.needle)?;
        let total = hits.len();
        let offset = args.offset.unwrap_or(0);
        let limit = args
            .limit
            .unwrap_or(DEFAULT_PAGE_LIMIT)
            .clamp(1, MAX_PAGE_LIMIT);
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

    fn memory_git_status(
        &self,
        config: &Config,
        session: &str,
        args: MemoryArgs,
    ) -> Result<Value, ToolError> {
        let worktree = self.memory_worktree(config, session, &args.memory)?;
        let status = git_status(&worktree)?;
        Ok(json!({ "status": status }))
    }

    fn memory_git_log(
        &self,
        config: &Config,
        session: &str,
        args: MemoryLogArgs,
    ) -> Result<Value, ToolError> {
        let worktree = self.memory_worktree(config, session, &args.memory)?;
        let log = git_log(&worktree, args.limit.unwrap_or(10));
        Ok(json!({ "log": log }))
    }

    fn memory_git_diff(
        &self,
        config: &Config,
        session: &str,
        args: MemoryArgs,
    ) -> Result<Value, ToolError> {
        let worktree = self.memory_worktree(config, session, &args.memory)?;
        let diff = git_diff(&worktree)?;
        Ok(json!({ "diff": diff }))
    }

    fn memory_git_blame(
        &self,
        config: &Config,
        session: &str,
        args: MemoryBlameArgs,
    ) -> Result<Value, ToolError> {
        let worktree = self.memory_worktree(config, session, &args.memory)?;
        let blame = git_blame(&worktree, &args.path)?;
        Ok(json!({ "path": args.path, "blame": blame }))
    }

    fn memory_git_add(
        &self,
        config: &Config,
        session: &str,
        args: MemoryAddArgs,
    ) -> Result<Value, ToolError> {
        let worktree = self.memory_worktree(config, session, &args.memory)?;
        git_add(&worktree, &args.path)?;
        Ok(json!({ "staged": if args.path.is_empty() { "全部" } else { &args.path } }))
    }

    fn memory_git_commit(
        &self,
        config: &Config,
        session: &str,
        args: MemoryCommitArgs,
    ) -> Result<Value, ToolError> {
        let worktree = self.memory_worktree(config, session, &args.memory)?;
        let result = git_commit(&worktree, &args.message)?;
        Ok(json!({ "result": result }))
    }

    fn memory_git_push(
        &self,
        config: &Config,
        session: &str,
        args: MemoryArgs,
    ) -> Result<Value, ToolError> {
        let worktree = self.memory_worktree(config, session, &args.memory)?;
        let result = git_push(&worktree)?;
        Ok(json!({ "result": result }))
    }

    fn memory_git_branch(
        &self,
        config: &Config,
        session: &str,
        args: MemoryArgs,
    ) -> Result<Value, ToolError> {
        let worktree = self.memory_worktree(config, session, &args.memory)?;
        let branches = git_branch(&worktree)?;
        Ok(json!({ "branches": branches }))
    }
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
            "quote_ref" => json!(record.quote_ref),
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
        OutboundError::Rejected(detail) => {
            ToolError::new(ErrorCode::Internal, with_file_hint(detail))
        }
        OutboundError::Unsupported(detail) => {
            ToolError::new(ErrorCode::BadRequest, with_file_hint(detail))
        }
        OutboundError::Unavailable(detail) => {
            ToolError::new(ErrorCode::Internal, with_file_hint(detail))
        }
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

/// 详情档的一条：多一张参数表（JSON Schema，模型最熟的形式）+ 一个能照抄的样例。
///
/// 参数表说「该填什么」，样例说「填好长这样」。只给前者，模型还得把 Schema 翻译成
/// 一次调用；翻译错就要多试一轮，而试错正是反馈里最多的摩擦。
fn describe_method(spec: &MethodSpec) -> Value {
    json!({
        "name": spec.name,
        "summary": spec.summary,
        "tags": spec.tags,
        "risk": spec.risk.as_str(),
        "effect": spec.effect.as_str(),
        "scope": spec.scope.as_str(),
        "params": spec.params_schema(),
        "example": spec.example_call(),
    })
}

/// 这台连接用到的标签，按首次出现的顺序。
fn tag_names<'a>(methods: &'a [MethodSpec]) -> Vec<&'a str> {
    let mut names: Vec<&'a str> = Vec::new();
    for method in methods {
        for tag in method.tags.iter() {
            let tag = tag.as_ref();
            if !names.contains(&tag) {
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
        format!(
            "[no_match] 没有同时满足 {} 的方法；{hint}",
            given.join(" + ")
        ),
    )
}

fn method_names(methods: &[MethodSpec]) -> String {
    methods
        .iter()
        .map(|method| method.name.as_ref())
        .collect::<Vec<_>>()
        .join(" / ")
}

/// 有哪些分组，按声明顺序（不是字母序）：适配器写得顺，模型看得也顺。
fn groups_of<'a>(methods: &'a [MethodSpec]) -> Vec<&'a str> {
    let mut groups: Vec<&'a str> = Vec::new();
    for method in methods {
        let group = method.group();
        if !groups.contains(&group) {
            groups.push(group);
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

    for param in spec.params.iter() {
        match given.get(param.name.as_ref()) {
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
                    let allowed = value.as_str().is_some_and(|text| {
                        param
                            .values
                            .iter()
                            .any(|candidate| candidate.as_ref() == text)
                    });
                    if !allowed {
                        let choices = param
                            .values
                            .iter()
                            .map(|candidate| candidate.as_ref())
                            .collect::<Vec<_>>()
                            .join(" / ");
                        return Err(ToolError::new(
                            ErrorCode::BadRequest,
                            format!(
                                "[bad_params] {} 的参数 {} 只能是 {choices}，给的是 {value}",
                                spec.name, param.name
                            ),
                        ));
                    }
                }
            }
        }
    }

    for key in given.keys() {
        if !spec
            .params
            .iter()
            .any(|param| param.name.as_ref() == key.as_str())
        {
            let known = if spec.params.is_empty() {
                "这个方法不要参数".to_owned()
            } else {
                format!(
                    "能给的参数：{}",
                    spec.params
                        .iter()
                        .map(|param| param.name.as_ref())
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

/// 房间目标参数校验：`group_id` / `user_id` 这类参数只能等于当前房间。
///
/// `MethodSpec.targets` 由适配器声明（ADR-0027）。core 不认识平台词，只做两件事：
/// - 当前房间类型匹配的 `Room` 目标：参数给了就必须等于房间 id；
/// - 其它房间类型的 `Room` 目标：给了就拒绝——除非同一参数在当前房间类型里
///   被声明成 `Member`（如 `poke.send` 在群里的 `user_id` 是“拍谁”）。
fn check_target_params(spec: &MethodSpec, room: &RoomId, params: &Value) -> Result<(), ToolError> {
    let Some(given) = params.as_object() else {
        // check_params 已经处理过非对象；这里只兜底。
        return Ok(());
    };
    if given.is_empty() || spec.targets.is_empty() {
        return Ok(());
    }

    let room_kind = room.path.first().map(String::as_str).unwrap_or("");
    let room_id = room.path.get(1).map(String::as_str).unwrap_or("");

    for target in spec.targets.iter() {
        if target.role != TargetRole::Room {
            continue;
        }
        let Some(value) = given.get(target.param.as_ref()) else {
            continue;
        };
        let given_id = value.as_str().unwrap_or_default();

        if target.room_kind.as_ref() == room_kind {
            if given_id != room_id {
                return Err(bad_target(spec, room, target, given_id));
            }
            continue;
        }

        // 这个参数指向另一种房间。只有“同一参数在当前房间类型里是成员目标”
        // 的情况可以放过（poke.send 群里的 user_id）。
        let is_member_here = spec.targets.iter().any(|other| {
            other.role == TargetRole::Member
                && other.param.as_ref() == target.param.as_ref()
                && other.room_kind.as_ref() == room_kind
        });
        if !is_member_here {
            return Err(bad_target(spec, room, target, given_id));
        }
    }

    Ok(())
}

fn bad_target(
    spec: &MethodSpec,
    room: &RoomId,
    target: &crate::outbound::TargetSpec,
    given_id: &str,
) -> ToolError {
    let suggestion = format!(
        "{}:{}/{}",
        room.connection,
        target.room_kind.as_ref(),
        given_id
    );
    ToolError::new(
        ErrorCode::BadRequest,
        format!(
            "[bad_params] {} 的 {} 是 {given_id}，但当前房间是 {room}。\
             **不要在 params 里换目标**；要操作它，把 platform_call 的 room 改成 \"{suggestion}\"\
             （并确认本会话订阅了它）。",
            spec.name, target.param
        ),
    )
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
    ToolError::new(code, with_file_hint(format!("[{kind}] {error}")))
}

/// 平台说「文件没有」时，多半不是路径写错：agent 和平台常常不在同一个文件系统
/// 命名空间里（平台在容器里），同一个文件两边看到的路径不一样。裸 ENOENT 会把
/// 排查方向带偏到拼写/权限/软链上——把三条出路附在错误后面。
fn with_file_hint(detail: String) -> String {
    let lower = detail.to_ascii_lowercase();
    let missing = lower.contains("enoent")
        || lower.contains("no such file")
        || lower.contains("realpath")
        || lower.contains("image not found")
        || lower.contains("file not found")
        || lower.contains("not found in cache");
    if !missing {
        return detail;
    }
    format!(
        "{detail}\n[文件问题?] 平台说的「没有」很可能不是路径写错：agent 和平台常常不在\
         同一个文件系统命名空间（平台在容器里）。三条出路：\
         ① 配好 [file_relay] 后重试，本机存在的文件桥会自动收下换成 URL；\
         ② 小文件（原始内容 750 KB 内，单帧上限 1 MiB）把元素的 url 写成 base64://…；\
         ③ 放到两边都挂载了的共享目录，传平台那边的路径。别先去查路径拼写和权限。"
    )
}
