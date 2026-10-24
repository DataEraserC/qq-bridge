//! 投递：把归档增量、唤醒结果与笔记摘要组装成一次 `deliver`。
//!
//! 这一层回答「发什么」：给定设备、群与当前归档，产出一帧内容块。它不发送、不落盘、
//! 不读时钟——socket 与重试都在别处，所以这里可测、可重放。
//!
//! 三条约束在这里落地：
//!
//! - **唤醒是批量判定，不是逐条判定。** 一次投递覆盖一批消息，原因取这批里最强的那个
//!   （[`WakeReason::rank`] 与 [`crate::wake::judge`] 的链条顺序一致）。逐条判出十个原因
//!   再挑一个，等于把同一份链条写两遍。
//! - **忽略名单与休眠期对所有消息生效。** 一批里有一条被忽略，不代表整批失声——只有全部
//!   被排除才是 [`Verdict::Stay`](crate::wake::Verdict)。
//! - **自己发的消息不参与唤醒判定。** 它们已在归档里（不变量一），渲染时进上下文块，但不能
//!   因为它们把整批叫醒。

use qqab_proto::{Block, Deliver, Scope};

use crate::archive::{Archive, Message, render};
use crate::config::{Device, ScopeConfig, WakeConfig};
use crate::notes::NoteStore;
use crate::turn::Ticket;
use crate::wake::{Incoming, Verdict, WakeReason, judge};

/// 组装好的一次投递。`deliver_ok` 之后才推进水位——见 [`Delivery::watermark`]。
#[derive(Debug, Clone, PartialEq)]
pub struct Delivery {
    /// 本回合的票，随帧发给适配器。
    pub ticket: Ticket,
    pub memory: String,
    pub persona: String,
    pub blocks: Vec<Block>,
    pub scope: Scope,
    pub wake_reason: String,
    pub notes: String,
    /// 投递成功后可推进到的水位（这批未读的最大序号）。0 表示没有未读。
    pub watermark: u64,
}

impl Delivery {
    /// 转成协议帧。`id` 由调用方生成——一帧一个请求标识，不在这里造。
    pub fn into_frame(self, id: impl Into<String>) -> Deliver {
        Deliver {
            id: id.into(),
            ticket: self.ticket.as_str().to_owned(),
            memory: self.memory,
            persona: self.persona,
            blocks: self.blocks,
            scope: self.scope,
            wake_reason: self.wake_reason,
            notes: self.notes,
        }
    }
}

/// 单独判定一次拍一拍。
///
/// 拍一拍没有群消息体，但它仍然要过同一条链条：忽略名单、休眠期、`poke` 开关一个不少。
pub fn judge_poke(
    config: &WakeConfig,
    sender_id: Option<&str>,
    now: time::OffsetDateTime,
    roll: f64,
) -> Option<WakeReason> {
    let sender = sender_id?;
    let incoming = Incoming {
        group: "",
        sender_id: sender,
        text: "",
        at_mention: false,
        poke: true,
    };
    match judge(config, &incoming, now, roll) {
        Verdict::Wake(reason) => Some(reason),
        Verdict::Stay => None,
    }
}

/// 组装一次投递。
///
/// 返回 `None` 表示这个群没有未读——没有可发的内容就不是投递，调用方应让队列保持空闲。
#[allow(clippy::too_many_arguments)]
pub fn assemble(
    device: &Device,
    scope: &ScopeConfig,
    group_name: Option<&str>,
    persona: &str,
    archive: &Archive,
    notes: &NoteStore,
    ticket: &Ticket,
    reason: &WakeReason,
    context_messages: usize,
    note_budget: usize,
) -> Option<Delivery> {
    let unread = archive.unread(&device.memory, &scope.group);
    if unread.is_empty() {
        return None;
    }

    let watermark = unread.iter().map(|message| message.seq).max().unwrap_or(0);
    let context = archive.context(&device.memory, &scope.group, context_messages);

    let blocks = render_blocks(&context, &unread, archive.dropped(&device.memory));

    Some(Delivery {
        ticket: ticket.clone(),
        memory: device.memory.clone(),
        persona: persona.to_owned(),
        blocks,
        scope: Scope {
            group: scope.group.clone(),
            group_name: group_name.map(str::to_owned),
            send_accounts: scope.send.clone(),
        },
        wake_reason: reason.label(),
        notes: notes.digest(&scope.group, note_budget),
        watermark,
    })
}

/// 渲染成内容块：一段文本，外加这批未读里的媒体。
///
/// 只渲一次而不是逐条一帧，是因为适配器把内容块拼进提示词；逐条分块只会让它多拼几次
/// 换行。媒体单独成块，路径原样给适配器——它不下载，媒体已由内核落地。
fn render_blocks(context: &[&Message], unread: &[&Message], dropped: u64) -> Vec<Block> {
    let mut text = String::new();

    if dropped > 0 {
        text.push_str(&format!(
            "（有 {dropped} 条更早的消息超出保留窗口，未包含在这批里）\n"
        ));
    }

    if !context.is_empty() {
        text.push_str(&render(context));
        text.push_str("\n—— 以下是新消息 ——\n");
    }

    text.push_str(&render(unread));

    if text.is_empty() {
        // 全是媒体且没有正文：仍给一个空文本块，让适配器有内容可投。
        text.push_str("[媒体]");
    }

    let mut blocks = vec![Block::Text { text }];
    for message in unread {
        for media in &message.media {
            blocks.push(block_for(media));
        }
    }

    blocks
}

fn block_for(media: &crate::archive::Media) -> Block {
    match media {
        crate::archive::Media::Image { path, mime } => Block::Image {
            path: path.clone(),
            mime: mime.clone(),
        },
        crate::archive::Media::File { path, mime } => Block::File {
            path: path.clone(),
            mime: mime.clone(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::BTreeSet;

    use time::OffsetDateTime;

    use crate::archive::{Limits, Media};
    use crate::config::WakeConfig;
    use crate::turn::{Arrival, Ticket, TurnContext, TurnTable};

    fn now() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_792_561_540).expect("合法时间戳")
    }

    fn message(seq_group: &str, id: &str, sender: &str, text: &str) -> Message {
        Message {
            account: "main".to_owned(),
            group: seq_group.to_owned(),
            message_id: id.to_owned(),
            sender_id: sender.to_owned(),
            text: text.to_owned(),
            at: 1_792_561_540_000,
            ..Message::default()
        }
    }

    fn archive_with(messages: Vec<Message>) -> Archive {
        let mut archive = Archive::new(Limits::default());
        for message in messages {
            archive.append("alice", message);
        }
        archive
    }

    fn device() -> Device {
        Device {
            id: "alice-phone".to_owned(),
            persona: "whale".to_owned(),
            memory: "alice".to_owned(),
            accounts: vec!["main".to_owned(), "backup".to_owned()],
            scopes: vec![ScopeConfig {
                group: "20001".to_owned(),
                receive: vec!["main".to_owned()],
                send: vec!["main".to_owned(), "backup".to_owned()],
            }],
            wake: WakeConfig::default(),
            hooks: None,
        }
    }

    fn scope() -> ScopeConfig {
        device().scopes[0].clone()
    }

    /// 真签一张票：票的构造器只有队列有，测试也不该绕过它。
    fn ticket() -> Ticket {
        let mut table = TurnTable::new(time::Duration::minutes(10));
        match table.arrive(
            TurnContext {
                memory: "alice".to_owned(),
                device: "alice-phone".to_owned(),
                group: "20001".to_owned(),
                send_accounts: vec!["main".to_owned()],
            },
            now(),
        ) {
            Arrival::Start(ticket) => ticket,
            Arrival::Defer => panic!("队列应是空闲的"),
        }
    }

    #[test]
    fn wake_judges_one_message_at_a_time() {
        // 批量合并后的那一次投递由管线决定；这里只证明「一条消息的判定」与 wake 层一致，
        // 管线不必自己再写一次链条。
        let config = WakeConfig {
            question: true,
            ..WakeConfig::default()
        };
        let mut mention = message("20001", "1", "30002", "在吗");
        mention.at_self = true;

        let incoming = crate::wake::Incoming {
            group: &mention.group,
            sender_id: &mention.sender_id,
            text: &mention.text,
            at_mention: mention.at_self,
            poke: false,
        };
        assert_eq!(
            crate::wake::judge(&config, &incoming, now(), 0.0),
            Verdict::Wake(WakeReason::AtMention)
        );
    }

    #[test]
    fn own_message_is_not_judged() {
        // 自己发的消息即使带 @ 语义也不该叫醒自己——判定在归档层就被 own 挡掉了。
        let mut own = message("20001", "1", "10001", "我自己说的");
        own.own = true;
        own.at_self = true;
        let archive = archive_with(vec![own]);

        assert!(
            archive.unread("alice", "20001").is_empty(),
            "own 本来就不进未读，判定层的输入里没有它"
        );
    }

    #[test]
    fn ignored_speaker_cannot_wake_by_poking() {
        // 忽略名单对拍一拍同样生效——否则「除某人外都唤醒」会被一次拍盖过。
        let config = WakeConfig {
            poke: true,
            ignore_speakers: BTreeSet::from(["30002".to_owned()]),
            ..WakeConfig::default()
        };

        assert_eq!(judge_poke(&config, Some("30002"), now(), 0.0), None);
        assert_eq!(
            judge_poke(&config, Some("30009"), now(), 0.0),
            Some(WakeReason::Poke)
        );
    }

    #[test]
    fn poke_is_off_by_default() {
        assert_eq!(
            judge_poke(&WakeConfig::default(), Some("30002"), now(), 0.0),
            None
        );
        // 没有发送者就不判定——拍一拍的来源必须可归因。
        let config = WakeConfig {
            poke: true,
            ..WakeConfig::default()
        };
        assert_eq!(judge_poke(&config, None, now(), 0.0), None);
    }

    #[test]
    fn assemble_includes_context_and_scope() {
        let archive = archive_with(vec![
            message("20001", "1", "30002", "旧的一"),
            message("20001", "2", "30002", "旧的一二"),
            message("20001", "3", "30002", "新的"),
        ]);
        // 前两条已被消费。
        let mut archive = archive;
        archive.consume("alice", "20001", 2).expect("应能推进");

        let notes = NoteStore::new(100);
        let delivery = assemble(
            &device(),
            &scope(),
            Some("测试群"),
            "你是……",
            &archive,
            &notes,
            &ticket(),
            &WakeReason::AnyMessage,
            10,
            0,
        )
        .expect("有未读就应能组装");

        assert_eq!(delivery.memory, "alice");
        assert_eq!(delivery.watermark, 3, "水位指向这批最大的序号");
        assert_eq!(delivery.scope.group, "20001");
        assert_eq!(delivery.scope.group_name.as_deref(), Some("测试群"));
        assert_eq!(delivery.scope.send_accounts, vec!["main", "backup"]);
        assert_eq!(delivery.wake_reason, "any_message");

        let Block::Text { text } = &delivery.blocks[0] else {
            panic!("第一块应是文本");
        };
        assert!(text.contains("旧的一二"), "上下文要带上");
        assert!(text.contains("以下是新消息"), "上下文与新消息要有分界");
        assert!(text.contains("新的"));
    }

    #[test]
    fn assemble_without_unread_is_none() {
        let archive = archive_with(vec![message("20001", "1", "30002", "一")]);
        let mut archive = archive;
        archive.consume("alice", "20001", 1).expect("应能推进");

        assert!(
            assemble(
                &device(),
                &scope(),
                None,
                "你是……",
                &archive,
                &NoteStore::new(10),
                &ticket(),
                &WakeReason::AnyMessage,
                10,
                0,
            )
            .is_none(),
            "没有未读就不是投递"
        );
    }

    #[test]
    fn media_becomes_its_own_block() {
        let mut with_image = message("20001", "1", "30002", "看图");
        with_image.media = vec![Media::Image {
            path: "/tmp/a.jpg".to_owned(),
            mime: "image/jpeg".to_owned(),
        }];
        let archive = archive_with(vec![with_image]);

        let delivery = assemble(
            &device(),
            &scope(),
            None,
            "你是……",
            &archive,
            &NoteStore::new(10),
            &ticket(),
            &WakeReason::AtMention,
            0,
            0,
        )
        .expect("应能组装");

        assert_eq!(
            delivery.blocks[1],
            Block::Image {
                path: "/tmp/a.jpg".to_owned(),
                mime: "image/jpeg".to_owned(),
            }
        );
    }

    #[test]
    fn dropped_is_disclosed_not_hidden() {
        // 收紧上限制造一次丢弃，投递必须说明有缺，而不是假装完整。
        let mut archive = archive_with(
            (1..=5)
                .map(|index| message("20001", &index.to_string(), "30002", "一"))
                .collect(),
        );
        // 用新的上限重建：只剩未读，没有上下文可丢，最旧的未读被丢弃。
        let json = archive.to_json().expect("应能序列化");
        archive = Archive::from_json(
            &json,
            Limits {
                max_messages: 3,
                max_bytes: Limits::default().max_bytes,
                context_messages: 0,
            },
        )
        .expect("应能解析");

        assert!(archive.dropped("alice") >= 2);

        let delivery = assemble(
            &device(),
            &scope(),
            None,
            "你是……",
            &archive,
            &NoteStore::new(10),
            &ticket(),
            &WakeReason::AnyMessage,
            0,
            0,
        )
        .expect("应能组装");

        let Block::Text { text } = &delivery.blocks[0] else {
            panic!("第一块应是文本");
        };
        assert!(text.contains("超出保留窗口"), "有缺要说明：{text}");
    }

    #[test]
    fn notes_digest_respects_budget() {
        let archive = archive_with(vec![message("20001", "1", "30002", "一")]);
        let mut notes = NoteStore::new(100);
        notes
            .write(
                "20001",
                crate::notes::NoteInput::new(
                    crate::notes::NoteKind::Term,
                    "吸光度",
                    "指溶液对光的吸收程度",
                ),
                1_792_561_540_000,
            )
            .expect("应能写入");

        let with_budget = assemble(
            &device(),
            &scope(),
            None,
            "你是……",
            &archive,
            &notes,
            &ticket(),
            &WakeReason::AnyMessage,
            0,
            800,
        )
        .expect("应能组装");
        assert!(!with_budget.notes.is_empty());

        // 预算 0 等于关掉注入，与笔记层自己的约定一致。
        let without = assemble(
            &device(),
            &scope(),
            None,
            "你是……",
            &archive,
            &notes,
            &ticket(),
            &WakeReason::AnyMessage,
            0,
            0,
        )
        .expect("应能组装");
        assert!(without.notes.is_empty());
    }

    #[test]
    fn delivery_frame_carries_ticket() {
        let archive = archive_with(vec![message("20001", "1", "30002", "在吗")]);
        let delivery_ticket = ticket();
        let delivery = assemble(
            &device(),
            &scope(),
            None,
            "你是……",
            &archive,
            &NoteStore::new(10),
            &delivery_ticket,
            &WakeReason::AtMention,
            0,
            0,
        )
        .expect("应能组装");

        let frame = delivery.into_frame("d-1");
        assert_eq!(frame.id, "d-1");
        assert_eq!(frame.ticket, delivery_ticket.as_str());
        assert_eq!(frame.wake_reason, "at_mention");
    }
}
