//! 事件管线：把归档、唤醒判定与串行队列串成一次事件的处理顺序。
//!
//! 这里落地 `docs/architecture/03-kernel.md#一次事件的处理顺序` 的第 4 到第 7 步——
//! 第 2 步（身份过滤）由调用方在此之前完成，第 3、5、6 步的钩子尚未实现，第 8 步发出帧
//! 属于传输层。
//!
//! 顺序不可重排，其中两条是硬约束：
//!
//! - **先归档，再判定。** 判定是队列层的前置结论；而积压合并要能重新清点未读，所以判定
//!   必须在归档之后。
//! - **判定针对增量，投递针对全部未读。** 一条消息叫醒设备，投递出去的是自上次投递以来
//!   的所有未读——否则队列忙时积压的消息会一条一条地被投成十个回合。
//!
//! 积压期间的唤醒原因要留。队列只记「有没有积压」这个布尔值，而积压合并后的那一次投递
//! 仍然需要一个原因标签；原因取积压期间**最强**的那个，与批量判定同一把尺子。

use std::collections::BTreeMap;

use time::{Duration, OffsetDateTime};

use crate::archive::{Archive, ArchiveError, Limits, Message, WatermarkError};
use crate::config::{Device, ScopeConfig};
use crate::deliver::{Delivery, assemble, judge_poke};
use crate::notes::NoteStore;
use crate::turn::{Arrival, Finished, Ticket, TicketError, TurnContext, TurnTable};
use crate::wake::WakeReason;

/// 一条事件进入管线后的结论。
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// 队列空闲且判定命中：本回合立即开始，内容已组装好。
    Deliver(Box<Delivery>),
    /// 队列忙：只记积压与原因，等当前回合结束后合并投递。
    Defer,
    /// 判定未命中：归档了，但这一轮不说话。
    Stay,
}

/// 管线状态。归档、笔记与队列各一份，均在内存里。
#[derive(Debug)]
pub struct Pipeline {
    archive: Archive,
    notes: NoteStore,
    turns: TurnTable,
    /// 投递时补多少条已消费的上下文。
    context_messages: usize,
    /// 每回合注入笔记摘要的预算，0 表示不注入。
    note_budget: usize,
    /// 积压期间的唤醒原因，键是记忆名。合并投递时取最强的一个。
    pending: BTreeMap<String, WakeReason>,
}

impl Pipeline {
    pub fn new(archive_limits: Limits, note_capacity: usize, turn_timeout: Duration) -> Self {
        Self {
            archive: Archive::new(archive_limits),
            notes: NoteStore::new(note_capacity),
            turns: TurnTable::new(turn_timeout),
            context_messages: archive_limits.context_messages,
            note_budget: 0,
            pending: BTreeMap::new(),
        }
    }

    /// 设置每回合注入笔记摘要的预算。
    pub fn with_note_budget(mut self, budget: usize) -> Self {
        self.note_budget = budget;
        self
    }

    pub fn archive(&self) -> &Archive {
        &self.archive
    }

    pub fn notes(&self) -> &NoteStore {
        &self.notes
    }

    pub fn notes_mut(&mut self) -> &mut NoteStore {
        &mut self.notes
    }

    /// 一条已通过身份过滤的消息到达。
    ///
    /// `message.own` 必须由调用方按设备的账号集合标好——那是第 2 步的事，管线不重复判断，
    /// 但要按它分岔：自己发的消息照样归档，不判定、不唤醒。
    pub fn ingest(
        &mut self,
        device: &Device,
        scope: &ScopeConfig,
        persona: &str,
        message: Message,
        now: OffsetDateTime,
        roll: f64,
    ) -> Action {
        let own = message.own;
        // 判定需要消息的字段，但 `append` 要拿走所有权——先判定，再追加。顺序不影响结论：
        // 判定只看这一条，不看归档。
        let reason = {
            let incoming = crate::wake::Incoming {
                group: &message.group,
                sender_id: &message.sender_id,
                text: &message.text,
                at_mention: message.at_self,
                poke: false,
            };
            // 只判定新到的那一条：投递时给的是全部未读，但「要不要醒」只由增量决定。
            match crate::wake::judge(&device.wake, &incoming, now, roll) {
                crate::wake::Verdict::Wake(reason) => Some(reason),
                crate::wake::Verdict::Stay => None,
            }
        };

        let appended = self.archive.append(&device.memory, message);

        // 自己发的消息照样归档，但不叫醒自己。
        if own {
            return Action::Stay;
        }

        // 重复到达（重连重发、轮换交接）不是新消息：同一句不该叫醒两次。
        if !appended.fresh {
            return Action::Stay;
        }

        let Some(reason) = reason else {
            return Action::Stay;
        };

        self.start_or_defer(device, scope, persona, reason, now)
    }

    /// 一次拍一拍到达。它不是消息，所以不走归档，照样参与判定。
    pub fn poke(
        &mut self,
        device: &Device,
        scope: &ScopeConfig,
        persona: &str,
        from: Option<&str>,
        now: OffsetDateTime,
        roll: f64,
    ) -> Action {
        let Some(reason) = judge_poke(&device.wake, from, now, roll) else {
            return Action::Stay;
        };
        self.start_or_defer(device, scope, persona, reason, now)
    }

    /// 回合结束。
    ///
    /// 返回 `Some(delivery)` 表示有积压，已合并成**一次**投递并签好了新票；`None` 表示队列
    /// 回到空闲。
    pub fn finish(
        &mut self,
        device: &Device,
        scope: &ScopeConfig,
        persona: &str,
        ticket: &Ticket,
        now: OffsetDateTime,
    ) -> Result<Option<Delivery>, PipelineError> {
        match self.turns.finish(&device.memory, ticket, now)? {
            Finished::Idle => Ok(None),
            Finished::Backlog => {
                // 积压期间一定有原因——只有判定命中才会走 Defer。万一没有，退回「任意消息」
                // 也比把这批未读永远压在归档里好。
                let reason = self
                    .pending
                    .remove(&device.memory)
                    .unwrap_or(WakeReason::AnyMessage);
                match self.turns.arrive(self.context_of(device, scope), now) {
                    Arrival::Start(ticket) => {
                        match assemble(
                            device,
                            scope,
                            None,
                            persona,
                            &self.archive,
                            &self.notes,
                            &ticket,
                            &reason,
                            self.context_messages,
                            self.note_budget,
                        ) {
                            Some(delivery) => Ok(Some(delivery)),
                            // 有积压却没有未读：票已签出，必须收回，否则队列永远忙。
                            // 原因也一并清掉——它对应的未读已经不在归档里了。
                            None => {
                                let _ = self.turns.finish(&device.memory, &ticket, now);
                                Err(PipelineError::NoUnread)
                            }
                        }
                    }
                    // 刚收尾又变成忙，说明这期间起了另一个回合；积压留着，原因也留着。
                    Arrival::Defer => {
                        self.pending.insert(device.memory.clone(), reason);
                        Ok(None)
                    }
                }
            }
        }
    }

    /// 投递成功，推进水位。失败的重试不推进——归档里的未读还在。
    pub fn confirm(
        &mut self,
        memory: &str,
        group: &str,
        through: u64,
    ) -> Result<(), PipelineError> {
        if through == 0 {
            return Ok(());
        }
        self.archive.consume(memory, group, through)?;
        Ok(())
    }

    /// 清扫超时的回合。与 [`TurnTable::sweep`] 同义，积压与原因保留。
    pub fn sweep(&mut self, now: OffsetDateTime) -> Vec<(String, Ticket)> {
        self.turns.sweep(now)
    }

    fn start_or_defer(
        &mut self,
        device: &Device,
        scope: &ScopeConfig,
        persona: &str,
        reason: WakeReason,
        now: OffsetDateTime,
    ) -> Action {
        match self.turns.arrive(self.context_of(device, scope), now) {
            Arrival::Start(ticket) => {
                match assemble(
                    device,
                    scope,
                    None,
                    persona,
                    &self.archive,
                    &self.notes,
                    &ticket,
                    &reason,
                    self.context_messages,
                    self.note_budget,
                ) {
                    Some(delivery) => Action::Deliver(Box::new(delivery)),
                    // 判定命中却没有未读：票已签出，必须收回，否则队列会永远忙。
                    None => {
                        let _ = self.turns.finish(&device.memory, &ticket, now);
                        Action::Stay
                    }
                }
            }
            Arrival::Defer => {
                self.remember_reason(&device.memory, reason);
                Action::Defer
            }
        }
    }

    /// 积压期间保留最强的那个原因。
    fn remember_reason(&mut self, memory: &str, reason: WakeReason) {
        match self.pending.get(memory) {
            Some(current) if current.rank() <= reason.rank() => {}
            _ => {
                self.pending.insert(memory.to_owned(), reason);
            }
        }
    }

    fn context_of(&self, device: &Device, scope: &ScopeConfig) -> TurnContext {
        TurnContext {
            memory: device.memory.clone(),
            device: device.id.clone(),
            group: scope.group.clone(),
            send_accounts: scope.send.clone(),
        }
    }
}

/// 管线的失败原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PipelineError {
    Ticket(TicketError),
    Watermark(WatermarkError),
    Archive(ArchiveError),
    /// 该有未读却没有——投递无内容可发。
    NoUnread,
}

impl From<TicketError> for PipelineError {
    fn from(error: TicketError) -> Self {
        Self::Ticket(error)
    }
}

impl From<WatermarkError> for PipelineError {
    fn from(error: WatermarkError) -> Self {
        Self::Watermark(error)
    }
}

impl From<ArchiveError> for PipelineError {
    fn from(error: ArchiveError) -> Self {
        Self::Archive(error)
    }
}

impl std::fmt::Display for PipelineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ticket(error) => write!(f, "回合票被拒绝：{error}"),
            Self::Watermark(error) => write!(f, "水位被拒绝：{error}"),
            Self::Archive(error) => write!(f, "{error}"),
            Self::NoUnread => f.write_str("判定命中但没有未读，不需要投递"),
        }
    }
}

impl std::error::Error for PipelineError {}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::config::WakeConfig;
    use crate::wake::Verdict;

    fn now() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_792_561_540).expect("合法时间戳")
    }

    fn device() -> Device {
        Device {
            id: "alice-phone".to_owned(),
            persona: "whale".to_owned(),
            memory: "alice".to_owned(),
            accounts: vec!["main".to_owned()],
            scopes: vec![ScopeConfig {
                group: "20001".to_owned(),
                receive: vec!["main".to_owned()],
                send: vec!["main".to_owned()],
            }],
            wake: WakeConfig::default(),
            hooks: None,
        }
    }

    fn scope() -> ScopeConfig {
        device().scopes[0].clone()
    }

    fn pipeline() -> Pipeline {
        Pipeline::new(Limits::default(), 100, Duration::minutes(10))
    }

    fn message(id: &str, sender: &str, text: &str) -> Message {
        Message {
            account: "main".to_owned(),
            group: "20001".to_owned(),
            message_id: id.to_owned(),
            sender_id: sender.to_owned(),
            text: text.to_owned(),
            at: 1_792_561_540_000,
            ..Message::default()
        }
    }

    fn mentioned(id: &str, sender: &str, text: &str) -> Message {
        Message {
            at_self: true,
            ..message(id, sender, text)
        }
    }

    #[test]
    fn mention_starts_turn_with_content() {
        let mut pipeline = pipeline();
        let action = pipeline.ingest(
            &device(),
            &scope(),
            "你是……",
            mentioned("1", "30002", "在吗"),
            now(),
            0.0,
        );

        let Action::Deliver(delivery) = action else {
            panic!("@ 应能唤醒，得到 {action:?}");
        };
        assert_eq!(delivery.wake_reason, "at_mention");
        assert_eq!(delivery.watermark, 1);
        assert_eq!(pipeline.archive().retained("alice"), 1, "先归档后判定");
    }

    #[test]
    fn duplicate_arrival_does_not_wake_twice() {
        // 轮换交接：a1 与 a2 各看到同一条 @。判定只对第一条生效，第二条按合并处理。
        let mut pipeline = pipeline();
        let Action::Deliver(first) = pipeline.ingest(
            &device(),
            &scope(),
            "你是……",
            mentioned("1", "30002", "在吗"),
            now(),
            0.0,
        ) else {
            panic!("第一份应能唤醒");
        };

        let mut rotated = mentioned("1", "30002", "在吗");
        rotated.account = "backup".to_owned();
        assert_eq!(
            pipeline.ingest(&device(), &scope(), "你是……", rotated, now(), 0.0),
            Action::Stay,
            "同一条从第二条连接再来，不该再一次唤醒或再记一次积压"
        );
        assert_eq!(pipeline.archive().retained("alice"), 1, "只归档一份");
        assert_eq!(pipeline.turns.issued(), 1, "只签过一张票");
        let _ = first;
    }

    #[test]
    fn ordinary_message_is_archived_but_stays() {
        let mut pipeline = pipeline();
        let action = pipeline.ingest(
            &device(),
            &scope(),
            "你是……",
            message("1", "30002", "随便"),
            now(),
            0.0,
        );

        assert_eq!(action, Action::Stay);
        assert_eq!(pipeline.archive().retained("alice"), 1, "没唤醒也要归档");
    }

    #[test]
    fn own_message_never_wakes_even_with_at() {
        let mut pipeline = pipeline();
        let mut own = mentioned("1", "10001", "我自己说的");
        own.own = true;

        assert_eq!(
            pipeline.ingest(&device(), &scope(), "你是……", own, now(), 0.0),
            Action::Stay
        );
        assert_eq!(pipeline.archive().retained("alice"), 1);
        assert!(pipeline.archive().unread("alice", "20001").is_empty());
    }

    #[test]
    fn second_message_while_busy_defers_and_merges() {
        let mut pipeline = pipeline();
        let first = pipeline.ingest(
            &device(),
            &scope(),
            "你是……",
            mentioned("1", "30002", "在吗"),
            now(),
            0.0,
        );
        let Action::Deliver(delivery) = first else {
            panic!("第一轮应能开始");
        };

        // 队列忙：第二条只记积压，不排成第二个回合。
        assert_eq!(
            pipeline.ingest(
                &device(),
                &scope(),
                "你是……",
                mentioned("2", "30002", "又一条"),
                now(),
                0.0
            ),
            Action::Defer
        );

        // 收尾：积压合并成一次投递，内容是两条未读。
        let merged = pipeline
            .finish(&device(), &scope(), "你是……", &delivery.ticket, now())
            .expect("应能收尾")
            .expect("有积压就该再投一次");

        assert_eq!(merged.watermark, 2, "一次投递覆盖全部积压");
        let qqab_proto::Block::Text { text } = &merged.blocks[0] else {
            panic!("第一块应是文本");
        };
        assert!(text.contains("在吗") && text.contains("又一条"));

        // 收尾后队列空闲。
        assert!(
            pipeline
                .finish(&device(), &scope(), "你是……", &merged.ticket, now())
                .expect("应能收尾")
                .is_none()
        );
    }

    #[test]
    fn backlog_keeps_strongest_reason() {
        let mut pipeline = pipeline();
        let Action::Deliver(first) = pipeline.ingest(
            &device(),
            &scope(),
            "你是……",
            mentioned("1", "30002", "在吗"),
            now(),
            0.0,
        ) else {
            panic!("第一轮应能开始");
        };

        // 先来一条普通消息（默认配置不命中），再来一条 @。
        assert_eq!(
            pipeline.ingest(
                &device(),
                &scope(),
                "你是……",
                message("2", "30002", "随便"),
                now(),
                0.0
            ),
            Action::Stay
        );
        let mut later = mentioned("3", "30002", "再看这个");
        later.message_id = "3".to_owned();
        assert_eq!(
            pipeline.ingest(&device(), &scope(), "你是……", later, now(), 0.0),
            Action::Defer
        );

        let merged = pipeline
            .finish(&device(), &scope(), "你是……", &first.ticket, now())
            .expect("应能收尾")
            .expect("有积压就该再投");
        assert_eq!(
            merged.wake_reason, "at_mention",
            "积压合并取最强原因，不是最后一条的"
        );
    }

    #[test]
    fn confirm_advances_watermark_and_clears_unread() {
        let mut pipeline = pipeline();
        let Action::Deliver(delivery) = pipeline.ingest(
            &device(),
            &scope(),
            "你是……",
            mentioned("1", "30002", "在吗"),
            now(),
            0.0,
        ) else {
            panic!("应能开始");
        };

        pipeline
            .confirm("alice", "20001", delivery.watermark)
            .expect("应能推进");
        assert!(pipeline.archive().unread("alice", "20001").is_empty());

        // 回合结束，队列空闲，下一轮才能开始。
        assert!(
            pipeline
                .finish(&device(), &scope(), "你是……", &delivery.ticket, now())
                .expect("应能收尾")
                .is_none(),
            "确认已清空未读，收尾后没有积压"
        );

        let Action::Deliver(second) = pipeline.ingest(
            &device(),
            &scope(),
            "你是……",
            mentioned("2", "30002", "再问一次"),
            now(),
            0.0,
        ) else {
            panic!("应能开始");
        };
        assert_eq!(second.watermark, 2);
        assert_eq!(pipeline.archive().unread("alice", "20001").len(), 1);
    }

    #[test]
    fn poke_wakes_without_archiving() {
        let mut pipeline = pipeline();
        let mut poke_device = device();
        poke_device.wake = WakeConfig {
            poke: true,
            ..WakeConfig::default()
        };

        let action = pipeline.poke(&poke_device, &scope(), "你是……", Some("30002"), now(), 0.0);
        // 没有消息可投，票收回，队列不忙。
        assert_eq!(action, Action::Stay);
        assert!(!pipeline.turns.is_busy("alice"));

        // 有未读时拍一拍就能投。
        pipeline.ingest(
            &poke_device,
            &scope(),
            "你是……",
            message("1", "30002", "随便"),
            now(),
            0.0,
        );
        let action = pipeline.poke(&poke_device, &scope(), "你是……", Some("30002"), now(), 0.0);
        let Action::Deliver(delivery) = action else {
            panic!("有未读时拍一拍应能投递");
        };
        assert_eq!(delivery.wake_reason, "poke");
    }

    #[test]
    fn ignored_speaker_stays_even_while_busy() {
        let mut pipeline = pipeline();
        let filtered = Device {
            wake: WakeConfig {
                any_message: true,
                ignore_speakers: std::collections::BTreeSet::from(["30002".to_owned()]),
                ..WakeConfig::default()
            },
            ..device()
        };

        assert_eq!(
            pipeline.ingest(
                &filtered,
                &scope(),
                "你是……",
                message("1", "30002", "一"),
                now(),
                0.0
            ),
            Action::Stay
        );
    }

    #[test]
    fn finish_without_unread_releases_the_ticket() {
        // 积压的未读被别处确认掉了（例如重复到达后水位推进），收尾时组装不出内容：
        // 票必须收回，否则队列永久卡在忙。
        let mut pipeline = pipeline();
        let Action::Deliver(first) = pipeline.ingest(
            &device(),
            &scope(),
            "你是……",
            mentioned("1", "30002", "在吗"),
            now(),
            0.0,
        ) else {
            panic!("应能开始");
        };
        assert_eq!(
            pipeline.ingest(
                &device(),
                &scope(),
                "你是……",
                mentioned("2", "30002", "还在吗"),
                now(),
                0.0
            ),
            Action::Defer
        );

        // 把积压那批也确认掉，模拟「别处已经推进了水位」。
        pipeline.confirm("alice", "20001", 2).expect("应能推进水位");

        assert_eq!(
            pipeline.finish(&device(), &scope(), "你是……", &first.ticket, now()),
            Err(PipelineError::NoUnread)
        );
        assert!(!pipeline.turns.is_busy("alice"), "票收回了，队列不该还忙");
    }

    #[test]
    fn expired_turn_keeps_backlog() {
        let mut pipeline = Pipeline::new(Limits::default(), 10, Duration::minutes(1));
        let Action::Deliver(first) = pipeline.ingest(
            &device(),
            &scope(),
            "你是……",
            mentioned("1", "30002", "在吗"),
            now(),
            0.0,
        ) else {
            panic!("应能开始");
        };
        assert_eq!(
            pipeline.ingest(
                &device(),
                &scope(),
                "你是……",
                mentioned("2", "30002", "还在吗"),
                now(),
                0.0
            ),
            Action::Defer
        );

        let later = now() + Duration::minutes(2);
        let swept = pipeline.sweep(later);
        assert_eq!(swept.len(), 1, "超时回合被作废");
        assert_eq!(swept[0].1, first.ticket);

        // 超时不丢消息：重新 ingest 能把积压投出去。
        let action = pipeline.ingest(
            &device(),
            &scope(),
            "你是……",
            mentioned("3", "30002", "再叫一次"),
            now() + Duration::minutes(3),
            0.0,
        );
        let Action::Deliver(delivery) = action else {
            panic!("超时后应能重新开始");
        };
        assert_eq!(delivery.watermark, 3);
    }

    #[test]
    fn judge_wake_stay_matches_pipeline_decision() {
        // 管线与逐条判定不能分叉：同一输入同一结论。
        let mut pipeline = pipeline();
        let config = WakeConfig::default();
        let incoming = crate::wake::Incoming {
            group: "20001",
            sender_id: "30002",
            text: "随便",
            at_mention: false,
            poke: false,
        };
        assert_eq!(
            crate::wake::judge(&config, &incoming, now(), 0.0),
            Verdict::Stay
        );
        assert_eq!(
            pipeline.ingest(
                &device(),
                &scope(),
                "你是……",
                message("1", "30002", "随便"),
                now(),
                0.0
            ),
            Action::Stay
        );
    }
}
