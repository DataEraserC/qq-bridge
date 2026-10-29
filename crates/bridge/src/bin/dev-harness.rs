//! 开发用的最小 harness：连上桥，看到一轮就回一句话，然后确认水位。
//!
//! 真 harness（DSH / WorkBuddy / Trae）还没接进来之前，用它验「桥 ↔ 平台」这一段：
//!
//! ```text
//! cargo run -p acb-bridge --bin dev-harness -- --endpoint 127.0.0.1:7700 --token 你的令牌
//! ```
//!
//! 它只做三件事：把投递过来的记录打出来、回一句固定的话、确认这些记录已处理。
//! 不做思考，不做判断——真实对话能力在 harness 那一侧。

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::process::ExitCode;
use std::time::Duration;

use acb_proto::element::flatten_text;
use acb_proto::frame::{PROTOCOL_VERSION, decode, encode};
use acb_proto::{Frame, Ticket};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("dev-harness：{error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let options = Options::parse(std::env::args().skip(1))?;

    let stream = TcpStream::connect(&options.endpoint)?;
    stream.set_read_timeout(Some(Duration::from_secs(3600)))?;
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream);

    send(
        &mut writer,
        &Frame::Hello {
            version: PROTOCOL_VERSION,
            token: Some(options.token.clone()),
        },
    )?;

    match read(&mut reader)? {
        Frame::Ready { device } => println!("已绑定到设备 {device}"),
        other => return Err(format!("握手失败：收到 {}", other.kind()).into()),
    }

    loop {
        let frame = match read(&mut reader) {
            Ok(frame) => frame,
            // 桥把手断掉了：正常收工。
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(error) => return Err(error.into()),
        };

        match frame {
            Frame::TurnStart { delivery } => {
                println!(
                    "\n--- 开轮：{}（{} 条记录）---",
                    delivery.room,
                    delivery.records.len()
                );
                for record in &delivery.records {
                    println!(
                        "  #{} {}：{}",
                        record.id,
                        record.sender.name,
                        flatten_text(&record.elements)
                    );
                    // 桥附的旁白（重投说明之类）另起一行，别和人家说的话混在一起。
                    if let Some(note) = &record.note {
                        println!("    ※ {note}");
                    }
                }

                if !delivery.records.is_empty() {
                    reply(&mut writer, &mut reader, &delivery.ticket, &options.reply)?;
                    ack(
                        &mut writer,
                        &mut reader,
                        &delivery.ticket,
                        &delivery.records,
                    )?;
                }

                send(
                    &mut writer,
                    &Frame::TurnEnd {
                        ticket: delivery.ticket,
                        error: None,
                    },
                )?;

                if options.once {
                    return Ok(());
                }
            }
            Frame::Ping => send(&mut writer, &Frame::Pong)?,
            Frame::Pong | Frame::Log { .. } => {}
            other => println!("收到 {}", other.kind()),
        }
    }
}

/// 回一句话，把工具结果等回来。
fn reply(
    writer: &mut TcpStream,
    reader: &mut BufReader<TcpStream>,
    ticket: &Ticket,
    text: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    send(
        writer,
        &Frame::ToolCall {
            id: "dev-reply".into(),
            ticket: ticket.clone(),
            tool: "send_message".into(),
            args: serde_json::json!({
                "elements": [{"type": "text", "text": text}]
            }),
        },
    )?;

    match read(reader)? {
        Frame::ToolResult { id, result, error } => {
            if let Some(error) = error {
                eprintln!("发言失败：{}（{:?}）", error.message, error.code);
            } else {
                let sent = result.unwrap_or_default();
                println!(
                    "  已发出，平台消息号 {}",
                    sent.get("platform_id")
                        .and_then(|id| id.as_str())
                        .unwrap_or("-")
                );
            }
            assert_eq!(id, "dev-reply");
        }
        other => eprintln!("等工具结果，收到 {}", other.kind()),
    }
    Ok(())
}

/// 确认这批记录处理完了，水位才会往前推。
fn ack(
    writer: &mut TcpStream,
    reader: &mut BufReader<TcpStream>,
    ticket: &Ticket,
    records: &[acb_proto::Record],
) -> Result<(), Box<dyn std::error::Error>> {
    let ids: Vec<u64> = records.iter().map(|record| record.id).collect();
    send(
        writer,
        &Frame::ToolCall {
            id: "dev-ack".into(),
            ticket: ticket.clone(),
            tool: "ack_messages".into(),
            args: serde_json::json!({ "ids": ids }),
        },
    )?;

    match read(reader)? {
        Frame::ToolResult { result, error, .. } => {
            if let Some(error) = error {
                eprintln!("确认失败：{}（{:?}）", error.message, error.code);
            } else if let Some(watermark) = result
                .unwrap_or_default()
                .get("watermark")
                .and_then(|value| value.as_u64())
            {
                println!("  水位推到 #{watermark}");
            }
        }
        other => eprintln!("等确认结果，收到 {}", other.kind()),
    }
    Ok(())
}

fn send(writer: &mut TcpStream, frame: &Frame) -> Result<(), Box<dyn std::error::Error>> {
    writer.write_all(encode(frame)?.as_bytes())?;
    writer.flush()?;
    Ok(())
}

fn read(reader: &mut BufReader<TcpStream>) -> std::io::Result<Frame> {
    let mut line = String::new();
    reader.read_line(&mut line)?;
    if line.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "桥把连接关了",
        ));
    }
    decode(&line).map_err(|error| std::io::Error::other(error.to_string()))
}

struct Options {
    endpoint: String,
    token: String,
    reply: String,
    once: bool,
}

impl Options {
    fn parse(args: impl Iterator<Item = String>) -> Result<Self, Box<dyn std::error::Error>> {
        let mut endpoint = "127.0.0.1:7700".to_owned();
        let mut token = String::new();
        let mut reply = "收到".to_owned();
        let mut once = false;
        let mut args = args.peekable();

        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--endpoint" => endpoint = args.next().ok_or("--endpoint 后面要跟地址")?,
                "--token" => token = args.next().ok_or("--token 后面要跟令牌")?,
                "--reply" => reply = args.next().ok_or("--reply 后面要跟内容")?,
                "--once" => once = true,
                "--help" | "-h" => {
                    println!(
                        "用法：dev-harness --token 令牌 [--endpoint 127.0.0.1:7700] \
                         [--reply 回复内容] [--once]"
                    );
                    std::process::exit(0);
                }
                other => return Err(format!("不认识的参数：{other}").into()),
            }
        }

        if token.trim().is_empty() {
            return Err("必须给 --token，跟配置里设备的 token 一致".into());
        }

        Ok(Self {
            endpoint,
            token,
            reply,
            once,
        })
    }
}
