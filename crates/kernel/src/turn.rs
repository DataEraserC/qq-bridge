//! 回合：票的签发与作废，以及按记忆名的串行队列。
//!
//! 这里落地两条不变量：
//!
//! - **二、串行队列的键是记忆名。** 同一记忆名同时只有一个回合。
//! - **四、回合票一次性。** 票在回合结束时作废，过期、未知、跨回合复用一律拒绝。
//!
//! 积压合并也在这里：队列忙时到达的消息不排队成多个回合，只记一个水位；回合结束后
//! 由调用方重新 `arrive` 一次，把积压合并成**一次**投递。这避免「群里刷屏后连续
//! 回复十条」。
//!
//! 时间由调用方传入，不在这里读时钟——判定可测、可重放。

use std::collections::BTreeMap;
use std::fmt;

use time::{Duration, OffsetDateTime};

/// 回合票。不透明字符串，只在协议里原样传递。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Ticket(String);

impl Ticket {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// 把适配器原样回传的字符串还原成票。
    ///
    /// 不做格式校验：票的内容由签发侧决定，校验它等于把内部格式变成协议的一部分。
    /// 「这是不是一张有效的票」由 [`TurnTable::authorize`] 判断。
    pub fn from_wire(text: &str) -> Self {
        Self(text.to_owned())
    }
}

impl fmt::Display for Ticket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// 一个回合的身份。
///
/// 内核靠它确定一次工具调用「是哪个设备、哪个范围、哪些账号」——适配器只回传票，
/// 不解析这些字段，所以票是唯一的身份来源。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnContext {
    pub memory: String,
    /// 设备标识。同一个记忆名可能被多个设备投递，这里记下本次是哪一个。
    pub device: String,
    /// 本次投递涉及的群。
    pub group: String,
    /// 允许用于发送的账号标识。
    pub send_accounts: Vec<String>,
}

/// 一次到达的判定结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Arrival {
    /// 队列空闲，回合立即开始。
    Start(Ticket),
    /// 队列忙，只更新积压水位。回合结束后调用方应再次 `arrive`。
    Defer,
}

/// 一次回合结束的判定结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Finished {
    /// 没有积压，队列回到空闲。
    Idle,
    /// 有积压，调用方应立即再次 `arrive` 把积压合并成一次投递。
    Backlog,
}

/// 票被拒绝的原因。三个变体对应协议里的两个错误码。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TicketError {
    /// 这个记忆名上没有进行中的回合，或票已经用过。
    Expired,
    /// 票属于另一个记忆名。
    Mismatch,
}

impl TicketError {
    /// 映射到协议错误码的名字。协议层用这个字符串，内核不依赖协议类型。
    pub fn code(self) -> &'static str {
        match self {
            Self::Expired => "ticket_expired",
            Self::Mismatch => "ticket_mismatch",
        }
    }
}

impl fmt::Display for TicketError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Expired => f.write_str("票已作废或过期"),
            Self::Mismatch => f.write_str("票与记忆名不匹配"),
        }
    }
}

impl std::error::Error for TicketError {}

/// 按记忆名的串行队列 + 进行中回合的票表。
#[derive(Debug)]
pub struct TurnTable {
    memories: BTreeMap<String, MemoryState>,
    next_seq: u64,
    timeout: Duration,
    /// 签发过的票数量。用于测试与诊断，不参与判定。
    issued: u64,
}

#[derive(Debug, Default)]
struct MemoryState {
    active: Option<Active>,
    /// 回合忙时是否有新消息到达。只记布尔值，不记条数——积压合并只需要知道「有」。
    backlog: bool,
}

#[derive(Debug)]
struct Active {
    ticket: Ticket,
    context: TurnContext,
    deadline: OffsetDateTime,
}

impl TurnTable {
    /// `timeout` 是单回合上限，超时视为失败。
    pub fn new(timeout: Duration) -> Self {
        Self {
            memories: BTreeMap::new(),
            next_seq: 0,
            timeout,
            issued: 0,
        }
    }

    /// 一批消息到达。
    ///
    /// 队列空闲则签票开始回合；忙则只记积压水位并返回 [`Arrival::Defer`]。
    pub fn arrive(&mut self, context: TurnContext, now: OffsetDateTime) -> Arrival {
        if self.is_busy(&context.memory) {
            self.memories.entry(context.memory).or_default().backlog = true;
            return Arrival::Defer;
        }

        let ticket = self.issue(now);
        let state = self.memories.entry(context.memory.clone()).or_default();
        state.backlog = false;
        state.active = Some(Active {
            ticket: ticket.clone(),
            context,
            deadline: now + self.timeout,
        });
        Arrival::Start(ticket)
    }

    /// 回合结束，作废票。
    ///
    /// 票不匹配时状态不变并返回错误——一个错误的 `turn_end` 不能把别人的回合关掉。
    pub fn finish(
        &mut self,
        memory: &str,
        ticket: &Ticket,
        now: OffsetDateTime,
    ) -> Result<Finished, TicketError> {
        let state = self.memories.get_mut(memory).ok_or(TicketError::Expired)?;

        let active = state.active.as_ref().ok_or(TicketError::Expired)?;
        if &active.ticket != ticket {
            return Err(TicketError::Mismatch);
        }

        // 超时的回合即使带着正确的票也不接受收尾：它已经被 sweep 视为失败，
        // 再接受一次等于让一个死回合重新关闭队列。
        if now >= active.deadline {
            return Err(TicketError::Expired);
        }

        state.active = None;
        Ok(if state.backlog {
            Finished::Backlog
        } else {
            Finished::Idle
        })
    }

    /// 校验一次工具调用的票，并返回该回合的身份。
    pub fn authorize(
        &self,
        memory: &str,
        ticket: &Ticket,
        now: OffsetDateTime,
    ) -> Result<&TurnContext, TicketError> {
        let state = self.memories.get(memory).ok_or(TicketError::Expired)?;
        let active = state.active.as_ref().ok_or(TicketError::Expired)?;

        if &active.ticket != ticket {
            return Err(TicketError::Mismatch);
        }
        if now >= active.deadline {
            return Err(TicketError::Expired);
        }

        Ok(&active.context)
    }

    /// 找出超时的回合并把它们作废，返回被作废的 `(记忆名, 票)`。
    ///
    /// 调用方据此上报失败并触发重投。积压水位保留——超时不代表消息丢了。
    pub fn sweep(&mut self, now: OffsetDateTime) -> Vec<(String, Ticket)> {
        let mut expired = Vec::new();
        for (memory, state) in &mut self.memories {
            let Some(active) = &state.active else {
                continue;
            };
            if now < active.deadline {
                continue;
            }
            let ticket = active.ticket.clone();
            state.active = None;
            expired.push((memory.clone(), ticket));
        }
        expired
    }

    /// 某记忆名是否有进行中的回合。
    pub fn is_busy(&self, memory: &str) -> bool {
        self.memories
            .get(memory)
            .is_some_and(|state| state.active.is_some())
    }

    /// 某记忆名是否有未投递的积压。
    pub fn has_backlog(&self, memory: &str) -> bool {
        self.memories.get(memory).is_some_and(|state| state.backlog)
    }

    /// 签发过的票总数。诊断用。
    pub fn issued(&self) -> u64 {
        self.issued
    }

    fn issue(&mut self, now: OffsetDateTime) -> Ticket {
        self.next_seq += 1;
        self.issued += 1;
        // 序号 + 签发时刻：进程重启后序号从零开始，加上时刻避免与上一轮进程的
        // 遗留票撞上。票本身不需要不可猜测——它是本机 socket 上的身份引用，不是凭据。
        Ticket(format!(
            "t{}-{}",
            self.next_seq,
            now.unix_timestamp_nanos() / 1_000_000
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_792_561_540).expect("合法时间戳")
    }

    fn table() -> TurnTable {
        TurnTable::new(Duration::minutes(10))
    }

    fn ctx(memory: &str) -> TurnContext {
        TurnContext {
            memory: memory.to_owned(),
            device: format!("{memory}-phone"),
            group: "20001".to_owned(),
            send_accounts: vec!["main".to_owned()],
        }
    }

    fn start(table: &mut TurnTable, memory: &str) -> Ticket {
        match table.arrive(ctx(memory), now()) {
            Arrival::Start(ticket) => ticket,
            Arrival::Defer => panic!("{memory} 应能立即开始"),
        }
    }

    #[test]
    fn first_arrival_starts_turn() {
        let mut table = table();
        let ticket = start(&mut table, "alice");
        assert!(table.is_busy("alice"));
        assert_eq!(table.issued(), 1);
        assert!(ticket.as_str().starts_with('t'));
    }

    #[test]
    fn second_arrival_defers_and_sets_backlog() {
        let mut table = table();
        start(&mut table, "alice");

        assert_eq!(table.arrive(ctx("alice"), now()), Arrival::Defer);
        assert_eq!(table.arrive(ctx("alice"), now()), Arrival::Defer);
        assert!(table.has_backlog("alice"));
        // 积压不签新票：它只记水位，不排成第二个回合。
        assert_eq!(table.issued(), 1);
    }

    #[test]
    fn finish_without_backlog_returns_idle() {
        let mut table = table();
        let ticket = start(&mut table, "alice");

        assert_eq!(table.finish("alice", &ticket, now()), Ok(Finished::Idle));
        assert!(!table.is_busy("alice"));

        // 空闲后可以开始新的回合。
        let next = start(&mut table, "alice");
        assert_ne!(next, ticket);
    }

    #[test]
    fn finish_with_backlog_asks_for_redelivery() {
        let mut table = table();
        let ticket = start(&mut table, "alice");
        assert_eq!(table.arrive(ctx("alice"), now()), Arrival::Defer);

        assert_eq!(table.finish("alice", &ticket, now()), Ok(Finished::Backlog));
        assert!(!table.is_busy("alice"));

        // 积压合并成一次投递，不是每来一条一个回合。
        let merged = start(&mut table, "alice");
        assert!(!table.has_backlog("alice"));
        assert_eq!(table.issued(), 2, "两次投递共两张票");
        assert_ne!(merged, ticket);
    }

    #[test]
    fn used_ticket_is_dead() {
        let mut table = table();
        let ticket = start(&mut table, "alice");
        table.finish("alice", &ticket, now()).expect("应能收尾");

        // 跨回合复用。
        assert_eq!(
            table.finish("alice", &ticket, now()),
            Err(TicketError::Expired)
        );
        assert_eq!(
            table.authorize("alice", &ticket, now()),
            Err(TicketError::Expired)
        );
    }

    #[test]
    fn ticket_from_another_memory_is_rejected() {
        let mut table = table();
        let alice = start(&mut table, "alice");
        let _bob = start(&mut table, "bob");

        assert_eq!(
            table.authorize("bob", &alice, now()),
            Err(TicketError::Mismatch)
        );
        assert_eq!(
            table.finish("bob", &alice, now()),
            Err(TicketError::Mismatch)
        );
        // 被拒绝的调用不能改变 bob 的回合状态。
        assert!(table.is_busy("bob"));
    }

    #[test]
    fn authorize_returns_turn_context() {
        let mut table = table();
        let ticket = start(&mut table, "alice");

        let context = table.authorize("alice", &ticket, now()).expect("票应有效");
        assert_eq!(context.device, "alice-phone");
        assert_eq!(context.group, "20001");
        assert_eq!(context.send_accounts, vec!["main".to_owned()]);
    }

    #[test]
    fn memories_do_not_block_each_other() {
        let mut table = table();
        let alice = start(&mut table, "alice");
        let bob = start(&mut table, "bob");

        // alice 忙不影响 bob 开始。
        assert!(table.is_busy("alice"));
        assert!(table.is_busy("bob"));
        assert_eq!(table.arrive(ctx("bob"), now()), Arrival::Defer);

        // bob 的积压只影响 bob；bob 先结束，alice 不受影响。
        assert_eq!(table.finish("bob", &bob, now()), Ok(Finished::Backlog));
        assert!(!table.has_backlog("alice"));
        assert!(table.is_busy("alice"));
        assert_eq!(
            table.authorize("alice", &alice, now()).map(|c| &c.memory),
            Ok(&"alice".to_owned())
        );
    }

    #[test]
    fn expired_turn_rejects_ticket_and_is_swept() {
        let mut table = TurnTable::new(Duration::minutes(10));
        let ticket = start(&mut table, "alice");

        let later = now() + Duration::minutes(11);
        assert_eq!(
            table.authorize("alice", &ticket, later),
            Err(TicketError::Expired)
        );
        // 带着正确票的收尾也不接受：超时的回合已经死了。
        assert_eq!(
            table.finish("alice", &ticket, later),
            Err(TicketError::Expired)
        );

        let swept = table.sweep(later);
        assert_eq!(swept, vec![("alice".to_owned(), ticket)]);
        assert!(!table.is_busy("alice"));
    }

    #[test]
    fn sweep_keeps_backlog() {
        let mut table = TurnTable::new(Duration::minutes(1));
        let _ticket = start(&mut table, "alice");
        assert_eq!(table.arrive(ctx("alice"), now()), Arrival::Defer);

        let later = now() + Duration::minutes(2);
        assert_eq!(table.sweep(later).len(), 1);
        // 超时不代表消息丢了：积压还在，重新 arrive 会立即投递。
        assert!(table.has_backlog("alice"));
        match table.arrive(ctx("alice"), later) {
            Arrival::Start(_) => {}
            Arrival::Defer => panic!("超时后应能重新开始"),
        }
    }

    #[test]
    fn sweep_ignores_live_turns() {
        let mut table = table();
        let _ticket = start(&mut table, "alice");
        assert!(table.sweep(now()).is_empty());
        assert!(table.is_busy("alice"));
    }

    #[test]
    fn finish_on_unknown_memory_is_expired() {
        let mut table = table();
        let ticket = start(&mut table, "alice");
        assert_eq!(
            table.finish("nobody", &ticket, now()),
            Err(TicketError::Expired)
        );
    }

    #[test]
    fn ticket_error_codes_match_protocol() {
        assert_eq!(TicketError::Expired.code(), "ticket_expired");
        assert_eq!(TicketError::Mismatch.code(), "ticket_mismatch");
    }
}
