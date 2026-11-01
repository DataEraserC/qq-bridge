//! conformance 测试用的最小外置适配器。
//!
//! 它证明的是一个外部进程只靠 wire 协议就能接进桥：说 hello、发事件、
//! 回发送/调用结果、按 shutdown 退出。平台逻辑是假的，协议是真的。

use std::io::{BufRead, Write};

use acb_adapter_api::{
    AdapterMessage, CoreMessage, WireAdapterInfo, WireError, WireMethodSpec, WireOutgoingKind,
    WireOutgoingRecord, WireParamSpec, WireRecord, WireResult, decode_line, encode_line,
};
use acb_proto::{Element, RecordKind, RoomId, Sender};
use serde_json::{Value, json};

fn main() -> std::io::Result<()> {
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();

    write_message(
        &mut stdout,
        &AdapterMessage::Hello {
            name: "fixture".into(),
            catalog: catalog(),
            info: WireAdapterInfo {
                events: vec!["knock".into(), "poke".into()],
                event_tags: vec!["notice".into(), "poke".into()],
                supports_at: true,
                supports_reply: true,
            },
        },
    )?;
    write_message(
        &mut stdout,
        &AdapterMessage::Event {
            record: WireRecord {
                room: RoomId::new("fixture", "group/1").expect("测试房间合法"),
                kind: RecordKind::Message,
                sender: Sender {
                    id: "10001".into(),
                    name: "夹具".into(),
                    role: None,
                },
                at: 1_700_000_000_000,
                elements: vec![Element::text("hello from fixture")],
                at_self: false,
                platform_id: Some("fx-event".into()),
                reply_to_self: false,
            },
        },
    )?;

    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let message = match decode_line::<CoreMessage>(&line) {
            Ok(message) => message,
            Err(error) => {
                eprintln!("fixture：读不懂 core 消息：{error}");
                continue;
            }
        };

        match message {
            CoreMessage::Describe { id } => {
                write_message(
                    &mut stdout,
                    &AdapterMessage::Reply {
                        id,
                        result: WireResult::Catalog { catalog: catalog() },
                    },
                )?;
            }
            CoreMessage::Decide { id, .. } => {
                write_message(
                    &mut stdout,
                    &AdapterMessage::Reply {
                        id,
                        result: WireResult::Error {
                            error: WireError::Unsupported {
                                detail: "这个 fixture 不是策略 provider".into(),
                            },
                        },
                    },
                )?;
            }
            CoreMessage::Send { id, .. } => {
                write_message(
                    &mut stdout,
                    &AdapterMessage::Reply {
                        id,
                        result: WireResult::Sent {
                            platform_id: format!("fx-{id}"),
                            at: 1_700_000_000_000,
                        },
                    },
                )?;
            }
            CoreMessage::Call {
                id, method: method_name, params, ..
            } => {
                let result = match method_name.as_str() {
                    "fixture.echo" => WireResult::Call {
                        value: params,
                        record: None,
                    },
                    "fixture.send" => {
                        let text = params
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned();
                        WireResult::Call {
                            value: json!({ "message_id": format!("fx-{id}") }),
                            record: Some(WireOutgoingRecord {
                                kind: WireOutgoingKind::Message,
                                elements: vec![Element::text(text)],
                                platform_id: Some(format!("fx-{id}")),
                                at: Some(1_700_000_000_000),
                            }),
                        }
                    }
                    "fixture.fail" => WireResult::Error {
                        error: WireError::BadParams {
                            detail: "fixture 故意失败".into(),
                        },
                    },
                    // 不回执直接退出：测试进程被杀后能不能自动重启。
                    "fixture.exit" => std::process::exit(0),
                    other => WireResult::Error {
                        error: WireError::UnknownMethod {
                            method: other.to_owned(),
                            suggestion: Some("fixture.echo".into()),
                        },
                    },
                };
                write_message(
                    &mut stdout,
                    &AdapterMessage::Reply { id, result },
                )?;
            }
            CoreMessage::Shutdown => break,
        }
    }

    Ok(())
}

fn catalog() -> Vec<WireMethodSpec> {
    let text_param = WireParamSpec {
        name: "text".into(),
        ty: "string".into(),
        required: true,
        desc: "正文".into(),
        values: Vec::new(),
    };
    vec![
        WireMethodSpec {
            name: "fixture.echo".into(),
            summary: "回显收到的参数".into(),
            tags: vec!["send".into()],
            risk: "read".into(),
            scope: "room".into(),
            params: vec![text_param.clone()],
            targets: Vec::new(),
            effect: "read".into(),
            record: "none".into(),
        },
        WireMethodSpec {
            name: "fixture.send".into(),
            summary: "假装发一条消息并记账".into(),
            tags: vec!["send".into(), "message".into()],
            risk: "write".into(),
            scope: "room".into(),
            params: vec![text_param],
            targets: Vec::new(),
            effect: "send".into(),
            record: "message".into(),
        },
        WireMethodSpec {
            name: "fixture.fail".into(),
            summary: "永远返回参数错误".into(),
            tags: vec![ "system".into() ],
            risk: "read".into(),
            scope: "room".into(),
            params: Vec::new(),
            targets: Vec::new(),
            effect: "read".into(),
            record: "none".into(),
        },
        WireMethodSpec {
            name: "fixture.exit".into(),
            summary: "直接退出进程（测试自动重启）".into(),
            tags: vec!["system".into()],
            risk: "write".into(),
            scope: "room".into(),
            params: Vec::new(),
            targets: Vec::new(),
            effect: "write".into(),
            record: "none".into(),
        },
    ]
}

fn write_message<W: Write>(stdout: &mut W, message: &AdapterMessage) -> std::io::Result<()> {
    let line = encode_line(message).map_err(std::io::Error::other)?;
    writeln!(stdout, "{line}")?;
    stdout.flush()
}
