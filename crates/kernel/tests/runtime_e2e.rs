//! 运行时的端到端测试：不碰网络、不碰真 socket，把一轮完整的来回跑完。
//!
//! 验证的是**接线**而不是各层：一条群消息进来，出站里应该有一帧 `deliver`；适配器回
//! `deliver_ok` 之后水位推进；`tool_call` 变成 QQ 动作；`turn_end` 之后积压合并成下一次
//! 投递。每一层自己的测试在各自的模块里，这里只问「它们接对了吗」。

use std::sync::mpsc::channel;

use serde_json::json;
use time::{Duration, OffsetDateTime};

use qqab_kernel::adapter::AdapterEvent;
use qqab_kernel::archive::Limits;
use qqab_kernel::chat::{ChatEvent, ChatMessage, ConversationKey, ReplyAddress};
use qqab_kernel::config::Config;
use qqab_kernel::conn::Pool;
use qqab_kernel::persona::Personas;
use qqab_kernel::pipeline::Pipeline;
use qqab_kernel::runtime::{Inbound, Kernel, Outbound, REDELIVER_AFTER};
use qqab_kernel::transport::Incoming;
use qqab_proto::{Frame, decode};

const CONFIG: &str = r#"
[[accounts]]
id = "main"
self_id = "10001"
ws_url = "ws://127.0.0.1:6700"

[[personas]]
id = "whale"
text = "你是……"

[[devices]]
id = "alice-phone"
persona = "whale"
memory = "alice"
accounts = ["main"]

  [[devices.scopes]]
  conversation = "20001"
  receive = ["main"]
  send = ["main"]

  [devices.wake]
  at_mention = true
  name_mention = true
"#;

/// 测试里的会话键：这些用例只有 QQ 一个平台。
fn qq(conversation: &str) -> ConversationKey {
    ConversationKey::new("qq", conversation)
}

fn now() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_792_561_540).expect("合法时间戳")
}

fn config_text() -> String {
    CONFIG.to_owned()
}

fn config() -> Config {
    Config::parse(&config_text()).expect("配置应能解析")
}

/// 建一个已连上适配器与 QQ 账号的运行时。
fn kernel() -> (Kernel, std::sync::mpsc::Receiver<String>) {
    let config = config();
    let personas = Personas::load(&config, std::path::Path::new(".")).expect("应能加载人格");
    let pool = Pool::new(&config.accounts);
    let pipeline = Pipeline::new(Limits::default(), 100, Duration::minutes(10));
    let mut kernel = Kernel::new(config, personas, pipeline, pool);

    // QQ 侧：接上一个动作入队口。
    let (action_tx, _action_rx) = channel();
    kernel.handle(
        Inbound::LinkUp {
            account: "main".to_owned(),
            actions: action_tx,
        },
        now(),
    );

    // 适配器侧：接上并打招呼。
    let (out_tx, out_rx) = channel();
    kernel.handle(
        Inbound::Adapter(AdapterEvent::Connected {
            conn: 1,
            out: out_tx,
            peer: "test".to_owned(),
        }),
        now(),
    );

    (kernel, out_rx)
}

fn group_message(id: &str, sender: &str, text: &str, at_self: bool) -> ChatEvent {
    ChatEvent::Message(ChatMessage {
        self_id: "10001".to_owned(),
        conversation: ConversationKey::new("qq", "20001"),
        reply_to: None,
        message_id: id.to_owned(),
        sender_id: sender.to_owned(),
        sender_name: Some("小明".to_owned()),
        text: text.to_owned(),
        at_self,
        time: 1_792_561_540_000,
    })
}

/// 取出出站里的帧并解码。
fn frames(out: &[Outbound]) -> Vec<Frame> {
    out.iter()
        .filter_map(|outbound| match outbound {
            Outbound::Frame { line, .. } => Some(decode(line).expect("出站的行应能解码")),
            _ => None,
        })
        .collect()
}

fn only_deliver(out: &[Outbound]) -> qqab_proto::Deliver {
    for frame in frames(out) {
        if let Frame::Deliver(deliver) = frame {
            return deliver;
        }
    }
    panic!("期望一帧 deliver，实际出站是 {:?}", out.len());
}

#[test]
fn mention_produces_a_deliver_frame_with_content_and_ticket() {
    let (mut kernel, _rx) = kernel();
    let out = kernel.handle(
        Inbound::Qq {
            account: "main".to_owned(),
            incoming: Incoming::Event(Box::new(group_message("1", "30002", "在吗", true))),
        },
        now(),
    );

    let deliver = only_deliver(&out);
    assert_eq!(deliver.memory, "alice");
    assert_eq!(deliver.wake_reason, "at_mention");
    assert_eq!(deliver.scope.conversation, "20001");
    assert!(!deliver.ticket.is_empty(), "投递必须带票，工具调用要靠它");
    assert!(!deliver.persona.is_empty(), "人格文本要随投递发过去");

    // 内容块里应看到渲染过的消息。
    let text = deliver
        .blocks
        .iter()
        .find_map(|block| match block {
            qqab_proto::Block::Text { text } => Some(text.clone()),
            _ => None,
        })
        .expect("应有文本块");
    assert!(text.contains("30002"), "号码才是身份：{text}");
    assert!(text.contains("在吗"), "正文要在块里：{text}");
}

#[test]
fn unaddressed_message_is_archived_but_not_delivered() {
    let (mut kernel, _rx) = kernel();
    let out = kernel.handle(
        Inbound::Qq {
            account: "main".to_owned(),
            incoming: Incoming::Event(Box::new(group_message("1", "30002", "随便说说", false))),
        },
        now(),
    );

    assert!(frames(&out).is_empty(), "没被叫到就不投递");
    assert_eq!(
        kernel.pipeline().archive().retained("alice"),
        1,
        "但仍然归档了——它可能成为上下文"
    );
}

#[test]
fn self_sent_message_is_archived_but_never_delivered() {
    let (mut kernel, _rx) = kernel();
    let out = kernel.handle(
        Inbound::Qq {
            account: "main".to_owned(),
            // 自己账号发的，而且 @ 了自己——不变量一：不叫醒自己。
            incoming: Incoming::Event(Box::new(group_message("1", "10001", "我说的", true))),
        },
        now(),
    );

    assert!(frames(&out).is_empty());
    assert_eq!(kernel.pipeline().archive().retained("alice"), 1);
    assert!(
        kernel
            .pipeline()
            .archive()
            .unread("alice", &qq("20001"))
            .is_empty(),
        "自己不产生未读"
    );
}

#[test]
fn deliver_ok_advances_the_watermark_only_after_the_adapter_confirms() {
    let (mut kernel, _rx) = kernel();
    let out = kernel.handle(
        Inbound::Qq {
            account: "main".to_owned(),
            incoming: Incoming::Event(Box::new(group_message("1", "30002", "在吗", true))),
        },
        now(),
    );
    let deliver = only_deliver(&out);
    assert_eq!(
        kernel.pipeline().archive().watermark("alice", &qq("20001")),
        0,
        "还没确认，水位不动"
    );

    kernel.handle(
        Inbound::Adapter(AdapterEvent::Frame {
            conn: 1,
            frame: Box::new(Frame::DeliverOk(qqab_proto::DeliverOk { id: deliver.id })),
        }),
        now(),
    );

    assert_eq!(
        kernel.pipeline().archive().watermark("alice", &qq("20001")),
        1,
        "确认之后才推进"
    );
    assert!(
        kernel
            .pipeline()
            .archive()
            .unread("alice", &qq("20001"))
            .is_empty()
    );
}

#[test]
fn tool_call_with_a_valid_ticket_becomes_a_qq_action() {
    let (mut kernel, _rx) = kernel();
    let out = kernel.handle(
        Inbound::Qq {
            account: "main".to_owned(),
            incoming: Incoming::Event(Box::new(group_message("1", "30002", "在吗", true))),
        },
        now(),
    );
    let deliver = only_deliver(&out);

    let out = kernel.handle(
        Inbound::Adapter(AdapterEvent::Frame {
            conn: 1,
            frame: Box::new(Frame::ToolCall(qqab_proto::ToolCall {
                id: "t-1".to_owned(),
                ticket: deliver.ticket.clone(),
                memory: "alice".to_owned(),
                tool: "qq_send_message".to_owned(),
                args: json!({"text": "在"}),
                as_account: None,
            })),
        }),
        now(),
    );

    let act = out
        .iter()
        .find_map(|outbound| match outbound {
            Outbound::Act { account, request } => Some((account.clone(), request)),
            _ => None,
        })
        .expect("应产生一次动作");
    assert_eq!(act.0, "main", "只能用这一轮 send 列表里的账号");
    assert_eq!(
        act.1.action,
        qqab_kernel::chat::ChatAction::Send {
            to: ReplyAddress {
                platform: "qq".to_owned(),
                address: "20001".to_owned(),
            },
            text: "在".to_owned(),
        },
        "群号来自回合上下文，不是模型给的参数"
    );
}

#[test]
fn tool_call_with_a_forged_ticket_is_refused() {
    let (mut kernel, _rx) = kernel();
    let out = kernel.handle(
        Inbound::Adapter(AdapterEvent::Frame {
            conn: 1,
            frame: Box::new(Frame::ToolCall(qqab_proto::ToolCall {
                id: "t-1".to_owned(),
                ticket: "t-9999-forged".to_owned(),
                memory: "alice".to_owned(),
                tool: "qq_send_message".to_owned(),
                args: json!({"text": "在"}),
                as_account: None,
            })),
        }),
        now(),
    );

    let frames = frames(&out);
    let error = frames
        .iter()
        .find_map(|frame| match frame {
            Frame::ToolResult(result) => result.error.clone(),
            _ => None,
        })
        .expect("应回一帧带错误的 tool_result");
    assert_eq!(error.code, qqab_proto::ErrorCode::TicketExpired);
    assert!(
        !out.iter().any(|item| matches!(item, Outbound::Act { .. })),
        "票不认就不该真发出去"
    );
}

#[test]
fn turn_end_merges_backlog_into_one_delivery() {
    let (mut kernel, _rx) = kernel();

    // 第一条 @：立即投递。
    let out = kernel.handle(
        Inbound::Qq {
            account: "main".to_owned(),
            incoming: Incoming::Event(Box::new(group_message("1", "30002", "在吗", true))),
        },
        now(),
    );
    let first = only_deliver(&out);

    // 回合期间又来两条，其中一条也是 @：应只攒积压，不签字二张以外的票。
    kernel.handle(
        Inbound::Qq {
            account: "main".to_owned(),
            incoming: Incoming::Event(Box::new(group_message("2", "30003", "围观", false))),
        },
        now(),
    );
    let out = kernel.handle(
        Inbound::Qq {
            account: "main".to_owned(),
            incoming: Incoming::Event(Box::new(group_message("3", "30004", "再问一次", true))),
        },
        now(),
    );
    assert!(frames(&out).is_empty(), "队列忙时不投递");

    // 回合结束：积压合并成一次投递。
    let out = kernel.handle(
        Inbound::Adapter(AdapterEvent::Frame {
            conn: 1,
            frame: Box::new(Frame::TurnEnd(qqab_proto::TurnEnd {
                memory: "alice".to_owned(),
                ticket: first.ticket.clone(),
                status: qqab_proto::TurnStatus::Ok,
            })),
        }),
        now(),
    );

    let merged = only_deliver(&out);
    assert_ne!(merged.ticket, first.ticket, "每次投递一个新票");
    let text = merged
        .blocks
        .iter()
        .find_map(|block| match block {
            qqab_proto::Block::Text { text } => Some(text.clone()),
            _ => None,
        })
        .expect("应有文本块");
    // 积压的两条都在同一批里，不是两个回合。
    assert!(text.contains("围观"), "积压的第一条要在：{text}");
    assert!(text.contains("再问一次"), "积压的第二条也要在：{text}");
}

#[test]
fn no_adapter_means_the_turn_is_abandoned_and_retried_not_lost() {
    // 没有适配器的运行时。
    let config = config();
    let personas = Personas::load(&config, std::path::Path::new(".")).expect("应能加载人格");
    let pool = Pool::new(&config.accounts);
    let pipeline = Pipeline::new(Limits::default(), 100, Duration::minutes(10));
    let mut kernel = Kernel::new(config, personas, pipeline, pool);
    let (action_tx, _action_rx) = channel();
    kernel.handle(
        Inbound::LinkUp {
            account: "main".to_owned(),
            actions: action_tx,
        },
        now(),
    );

    let out = kernel.handle(
        Inbound::Qq {
            account: "main".to_owned(),
            incoming: Incoming::Event(Box::new(group_message("1", "30002", "在吗", true))),
        },
        now(),
    );

    assert!(frames(&out).is_empty(), "没有适配器，发不出去");
    assert!(
        out.iter().any(|item| matches!(
            item,
            Outbound::Log { level: "warn", message } if message.contains("重投")
        )),
        "要说明将重投"
    );
    // 关键：消息没丢，票价也回收了——下一次能重新开始。
    assert_eq!(kernel.pipeline().archive().retained("alice"), 1);
    assert_eq!(
        kernel
            .pipeline()
            .archive()
            .unread("alice", &qq("20001"))
            .len(),
        1
    );

    // 适配器上线：立刻重投同一批。
    let (out_tx, _out_rx) = channel();
    let out = kernel.handle(
        Inbound::Adapter(AdapterEvent::Connected {
            conn: 2,
            out: out_tx,
            peer: "late".to_owned(),
        }),
        now(),
    );
    assert!(
        frames(&out)
            .iter()
            .any(|frame| matches!(frame, Frame::Deliver(_))),
        "适配器一上线就重投，实际出站 {:?}",
        frames(&out)
            .iter()
            .map(std::mem::discriminant)
            .collect::<Vec<_>>()
    );
}

/// 一个配了接入令牌的内核，其余与 [`kernel`] 相同。
fn kernel_with_token(token: &str) -> (Kernel, std::sync::mpsc::Receiver<String>) {
    let mut text = config_text();
    text.push_str(&format!("\n[adapter]\ntoken = \"{token}\"\n"));
    let config = Config::parse(&text).expect("应能解析");
    let personas = Personas::load(&config, std::path::Path::new(".")).expect("应能加载人格");
    let pool = Pool::new(&config.accounts);
    let pipeline = Pipeline::new(Limits::default(), 100, Duration::minutes(10));
    let mut kernel = Kernel::new(config, personas, pipeline, pool);

    let (action_tx, _action_rx) = channel();
    kernel.handle(
        Inbound::LinkUp {
            account: "main".to_owned(),
            actions: action_tx,
        },
        now(),
    );

    let (out_tx, out_rx) = channel();
    kernel.handle(
        Inbound::Adapter(AdapterEvent::Connected {
            conn: 1,
            out: out_tx,
            peer: "test".to_owned(),
        }),
        now(),
    );
    (kernel, out_rx)
}

#[test]
fn hello_with_matching_token_is_accepted() {
    let (mut kernel, _rx) = kernel_with_token("s3cret");
    let out = kernel.handle(
        Inbound::Adapter(AdapterEvent::Frame {
            conn: 1,
            frame: Box::new(Frame::Hello(qqab_proto::Hello {
                id: "h-1".to_owned(),
                version: qqab_proto::PROTOCOL_VERSION,
                adapter: "remote".to_owned(),
                token: Some("s3cret".to_owned()),
            })),
        }),
        now(),
    );
    assert!(
        frames(&out)
            .iter()
            .any(|frame| matches!(frame, Frame::HelloOk(_))),
        "令牌正确应通过"
    );
}

#[test]
fn hello_with_wrong_or_missing_token_is_rejected_and_closed() {
    // 跨机器形态下，令牌是唯一的身份来源：错的和没给的一样拒绝。
    for presented in [Some("wrong"), None] {
        let (mut kernel, _rx) = kernel_with_token("s3cret");
        let out = kernel.handle(
            Inbound::Adapter(AdapterEvent::Frame {
                conn: 1,
                frame: Box::new(Frame::Hello(qqab_proto::Hello {
                    id: "h-1".to_owned(),
                    version: qqab_proto::PROTOCOL_VERSION,
                    adapter: "remote".to_owned(),
                    token: presented.map(|value| value.to_owned()),
                })),
            }),
            now(),
        );
        assert!(
            frames(&out)
                .iter()
                .any(|frame| matches!(frame, Frame::HelloReject(_))),
            "令牌 {presented:?} 应被拒绝"
        );
        assert!(
            out.iter()
                .any(|item| matches!(item, Outbound::CloseAdapter { conn: 1 })),
            "拒绝之后要断开"
        );
    }
}

#[test]
fn hello_without_token_is_fine_when_none_configured() {
    // 本机形态不配令牌：不给令牌照样通过。
    let (mut kernel, _rx) = kernel();
    let out = kernel.handle(
        Inbound::Adapter(AdapterEvent::Frame {
            conn: 1,
            frame: Box::new(Frame::Hello(qqab_proto::Hello {
                id: "h-1".to_owned(),
                version: qqab_proto::PROTOCOL_VERSION,
                adapter: "local".to_owned(),
                token: None,
            })),
        }),
        now(),
    );
    assert!(
        frames(&out)
            .iter()
            .any(|frame| matches!(frame, Frame::HelloOk(_))),
        "没配令牌时缺省字段不影响握手"
    );
}

#[test]
fn hello_version_mismatch_is_rejected_and_closed() {
    let (mut kernel, _rx) = kernel();
    let out = kernel.handle(
        Inbound::Adapter(AdapterEvent::Frame {
            conn: 1,
            frame: Box::new(Frame::Hello(qqab_proto::Hello {
                id: "h-1".to_owned(),
                version: 99,
                adapter: "future".to_owned(),
                token: None,
            })),
        }),
        now(),
    );

    let frames = frames(&out);
    assert!(
        frames
            .iter()
            .any(|frame| matches!(frame, Frame::HelloReject(_))),
        "主版本不同应拒绝"
    );
    assert!(
        out.iter()
            .any(|item| matches!(item, Outbound::CloseAdapter { conn: 1 })),
        "拒绝之后要断开，而不是让它继续说"
    );
}

#[test]
fn same_version_hello_is_accepted() {
    let (mut kernel, _rx) = kernel();
    let out = kernel.handle(
        Inbound::Adapter(AdapterEvent::Frame {
            conn: 1,
            frame: Box::new(Frame::Hello(qqab_proto::Hello {
                id: "h-1".to_owned(),
                version: qqab_proto::PROTOCOL_VERSION,
                adapter: "dsh".to_owned(),
                token: None,
            })),
        }),
        now(),
    );

    assert!(
        frames(&out)
            .iter()
            .any(|frame| matches!(frame, Frame::HelloOk(_)))
    );
}

#[test]
fn ping_is_answered_with_pong() {
    let (mut kernel, _rx) = kernel();
    let out = kernel.handle(
        Inbound::Adapter(AdapterEvent::Frame {
            conn: 1,
            frame: Box::new(Frame::Ping(qqab_proto::Ping {
                id: "p-1".to_owned(),
            })),
        }),
        now(),
    );

    let pong = frames(&out)
        .into_iter()
        .find_map(|frame| match frame {
            Frame::Pong(pong) => Some(pong),
            _ => None,
        })
        .expect("应回 pong");
    assert_eq!(pong.id, "p-1", "响应要带同一个 id");
}

#[test]
fn adapter_disconnect_fails_inflight_tool_calls() {
    let (mut kernel, _rx) = kernel();
    let out = kernel.handle(
        Inbound::Qq {
            account: "main".to_owned(),
            incoming: Incoming::Event(Box::new(group_message("1", "30002", "在吗", true))),
        },
        now(),
    );
    let deliver = only_deliver(&out);

    // 发起一个工具调用，动作还没回来。
    kernel.handle(
        Inbound::Adapter(AdapterEvent::Frame {
            conn: 1,
            frame: Box::new(Frame::ToolCall(qqab_proto::ToolCall {
                id: "t-1".to_owned(),
                ticket: deliver.ticket.clone(),
                memory: "alice".to_owned(),
                tool: "qq_send_message".to_owned(),
                args: json!({"text": "在"}),
                as_account: None,
            })),
        }),
        now(),
    );

    // 适配器断线：那个调用必须收到失败，而不是永远悬着。
    let out = kernel.handle(
        Inbound::Adapter(AdapterEvent::Closed {
            conn: 1,
            reason: "崩溃".to_owned(),
        }),
        now(),
    );

    let error = frames(&out)
        .into_iter()
        .find_map(|frame| match frame {
            Frame::ToolResult(result) => result.error,
            _ => None,
        })
        .expect("在途调用应有失败回执");
    assert_eq!(error.code, qqab_proto::ErrorCode::Internal);
}

// ---------------------------------------------------------------------------
// 一个记忆名订阅两个会话
// ---------------------------------------------------------------------------

/// 一台设备、一个记忆名，订阅两个群。策略与设备 id 都一样——差别只在范围。
///
/// 这就是「一个 agent 会话处理两个聊天会话」的形态：记忆、人格、队列键全是一份，
/// 只有范围不同。
const TWO_SCOPES: &str = r#"
[[accounts]]
id = "main"
self_id = "10001"
ws_url = "ws://127.0.0.1:6700"

[[personas]]
id = "whale"
text = "你是……"

[[devices]]
id = "alice-phone"
persona = "whale"
memory = "alice"
accounts = ["main"]

  [[devices.scopes]]
  conversation = "20001"
  receive = ["main"]
  send = ["main"]

  [[devices.scopes]]
  conversation = "20003"
  receive = ["main"]
  send = ["main"]

  [devices.wake]
  at_mention = true
  name_mention = true
"#;

/// 与 [`kernel`] 相同，但用 [`TWO_SCOPES`] 这份配置。
fn kernel_two_scopes() -> Kernel {
    let config = Config::parse(TWO_SCOPES).expect("配置应能解析");
    let personas = Personas::load(&config, std::path::Path::new(".")).expect("应能加载人格");
    let pool = Pool::new(&config.accounts);
    let pipeline = Pipeline::new(Limits::default(), 100, Duration::minutes(10));
    let mut kernel = Kernel::new(config, personas, pipeline, pool);

    let (action_tx, _action_rx) = channel();
    kernel.handle(
        Inbound::LinkUp {
            account: "main".to_owned(),
            actions: action_tx,
        },
        now(),
    );
    let (out_tx, _out_rx) = channel();
    kernel.handle(
        Inbound::Adapter(AdapterEvent::Connected {
            conn: 1,
            out: out_tx,
            peer: "test".to_owned(),
        }),
        now(),
    );
    kernel
}

fn message_in(group: &str, id: &str, sender: &str, text: &str, at_self: bool) -> ChatEvent {
    ChatEvent::Message(ChatMessage {
        conversation: ConversationKey::new("qq", group),
        reply_to: None,
        message_id: id.to_owned(),
        ..match group_message(id, sender, text, at_self) {
            ChatEvent::Message(message) => message,
            other => panic!("期望消息，得到 {other:?}"),
        }
    })
}

#[test]
fn a_turn_in_one_group_does_not_strand_the_other_groups_messages() {
    // chat1 的回合进行中，chat2 收到 @。回合结束后 chat2 那一批必须投出去——
    // 不能等 chat2 的下一条新消息。
    let mut kernel = kernel_two_scopes();

    let out = kernel.handle(
        Inbound::Qq {
            account: "main".to_owned(),
            incoming: Incoming::Event(Box::new(message_in(
                "20001",
                "1",
                "30002",
                "甲群在吗",
                true,
            ))),
        },
        now(),
    );
    let first = only_deliver(&out);
    assert_eq!(first.scope.conversation, "20001");

    // chat2 的消息到达：队列按记忆名串行，先积压。
    let out = kernel.handle(
        Inbound::Qq {
            account: "main".to_owned(),
            incoming: Incoming::Event(Box::new(message_in(
                "20003",
                "1",
                "30002",
                "乙群在吗",
                true,
            ))),
        },
        now(),
    );
    assert!(frames(&out).is_empty(), "队列忙时不投递");

    // chat1 回合结束。
    let out = kernel.handle(
        Inbound::Adapter(AdapterEvent::Frame {
            conn: 1,
            frame: Box::new(Frame::TurnEnd(qqab_proto::TurnEnd {
                memory: "alice".to_owned(),
                ticket: first.ticket.clone(),
                status: qqab_proto::TurnStatus::Ok,
            })),
        }),
        now(),
    );

    let merged = only_deliver(&out);
    assert_eq!(
        merged.scope.conversation, "20003",
        "投出去的是积压的乙群，不是刚结束的甲群"
    );
    let text = merged
        .blocks
        .iter()
        .find_map(|block| match block {
            qqab_proto::Block::Text { text } => Some(text.clone()),
            _ => None,
        })
        .expect("应有文本块");
    assert!(text.contains("乙群在吗"), "乙群的消息不能丢：{text}");
}

#[test]
fn the_tool_targets_the_group_of_the_batch_that_was_actually_delivered() {
    // 收尾时从甲群轮转到乙群。这一回合的 `qq_send_message` 必须发进乙群，
    // 而不是调用方手里那个已经过时的甲群。
    let mut kernel = kernel_two_scopes();

    let out = kernel.handle(
        Inbound::Qq {
            account: "main".to_owned(),
            incoming: Incoming::Event(Box::new(message_in(
                "20001",
                "1",
                "30002",
                "甲群在吗",
                true,
            ))),
        },
        now(),
    );
    let first = only_deliver(&out);

    kernel.handle(
        Inbound::Qq {
            account: "main".to_owned(),
            incoming: Incoming::Event(Box::new(message_in(
                "20003",
                "1",
                "30002",
                "乙群在吗",
                true,
            ))),
        },
        now(),
    );

    // 收尾：投乙群，并且上下文跟着换成乙群。
    let out = kernel.handle(
        Inbound::Adapter(AdapterEvent::Frame {
            conn: 1,
            frame: Box::new(Frame::TurnEnd(qqab_proto::TurnEnd {
                memory: "alice".to_owned(),
                ticket: first.ticket.clone(),
                status: qqab_proto::TurnStatus::Ok,
            })),
        }),
        now(),
    );
    let second = only_deliver(&out);
    assert_eq!(second.scope.conversation, "20003");

    // 这一回合发一句话：必须是乙群。
    let out = kernel.handle(
        Inbound::Adapter(AdapterEvent::Frame {
            conn: 1,
            frame: Box::new(Frame::ToolCall(qqab_proto::ToolCall {
                id: "t-1".to_owned(),
                ticket: second.ticket.clone(),
                memory: "alice".to_owned(),
                tool: "qq_send_message".to_owned(),
                args: json!({"text": "在"}),
                as_account: None,
            })),
        }),
        now(),
    );

    let act = out
        .iter()
        .find_map(|outbound| match outbound {
            Outbound::Act { request, .. } => Some(request.action.clone()),
            _ => None,
        })
        .expect("应产生一次动作");
    assert_eq!(
        act,
        qqab_kernel::chat::ChatAction::Send {
            to: ReplyAddress {
                platform: "qq".to_owned(),
                address: "20003".to_owned(),
            },
            text: "在".to_owned(),
        },
        "目标群来自被投出去的那一批，不是调用方的参数"
    );
}

#[test]
fn retries_of_two_groups_are_both_kept_and_both_eventually_delivered() {
    // 适配器不在线时两个会话各自失败。按记忆名记重投只会留下最后一条，另一条永久消失。
    // 串行队列一次只服务一个会话，所以这里是「都留着，逐个投」而不是「同时投」。
    let config = Config::parse(TWO_SCOPES).expect("配置应能解析");
    let personas = Personas::load(&config, std::path::Path::new(".")).expect("应能加载人格");
    let pool = Pool::new(&config.accounts);
    let pipeline = Pipeline::new(Limits::default(), 100, Duration::minutes(10));
    let mut kernel = Kernel::new(config, personas, pipeline, pool);

    let (action_tx, _action_rx) = channel();
    kernel.handle(
        Inbound::LinkUp {
            account: "main".to_owned(),
            actions: action_tx,
        },
        now(),
    );

    // 两个群各来一条 @，都没有适配器可投。
    for (group, text) in [("20001", "甲群在吗"), ("20003", "乙群在吗")] {
        kernel.handle(
            Inbound::Qq {
                account: "main".to_owned(),
                incoming: Incoming::Event(Box::new(message_in(group, "1", "30002", text, true))),
            },
            now(),
        );
    }

    // 适配器上线：先投一个会话（队列按记忆名串行，另一个留着）。
    let (out_tx, _out_rx) = channel();
    let out = kernel.handle(
        Inbound::Adapter(AdapterEvent::Connected {
            conn: 2,
            out: out_tx,
            peer: "late".to_owned(),
        }),
        now(),
    );
    let first_delivery = only_deliver(&out);
    let first_group = first_delivery.scope.conversation.clone();

    // 这一回合结束，然后把时钟推到重投到点：另一个会话必须被投出去。
    let out = kernel.handle(
        Inbound::Adapter(AdapterEvent::Frame {
            conn: 2,
            frame: Box::new(Frame::TurnEnd(qqab_proto::TurnEnd {
                memory: "alice".to_owned(),
                ticket: first_delivery.ticket.clone(),
                status: qqab_proto::TurnStatus::Ok,
            })),
        }),
        now(),
    );
    let mut delivered: Vec<String> = frames(&out)
        .iter()
        .filter_map(|frame| match frame {
            Frame::Deliver(deliver) => Some(deliver.scope.conversation.clone()),
            _ => None,
        })
        .collect();

    // 收尾没有立刻带出另一个会话也没关系——重投会把它带出来。
    if delivered.is_empty() {
        let later = now() + REDELIVER_AFTER + Duration::seconds(1);
        let out = kernel.handle(Inbound::Tick, later);
        delivered = frames(&out)
            .iter()
            .filter_map(|frame| match frame {
                Frame::Deliver(deliver) => Some(deliver.scope.conversation.clone()),
                _ => None,
            })
            .collect();
    }

    let other = if first_group == "20001" {
        "20003"
    } else {
        "20001"
    };
    assert!(
        delivered.contains(&other.to_owned()),
        "另一个会话必须被投出去，实际投出 {delivered:?}（第一个是 {first_group}）"
    );
}
