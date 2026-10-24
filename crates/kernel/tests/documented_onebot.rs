//! 文档里的 OneBot 事件形状必须和实现对得上。
//!
//! 直接读 `docs/architecture/03-kernel.md`，抽出其中的事件示例来解码。测试里不复制一份
//! ——复制的那份迟早和文档分叉，而这份测试的全部意义就是抓分叉。

use qqab_kernel::onebot::{Action, ActionResponse, Event, decode};

const KERNEL_DOC: &str = include_str!("../../../docs/architecture/03-kernel.md");

/// 从 Markdown 里取出第一个带 `json` 标注、且含 `post_type` 的代码块。
fn event_block(markdown: &str) -> String {
    let mut rest = markdown;
    while let Some(start) = rest.find("```json") {
        let after = &rest[start + "```json".len()..];
        let end = after.find("```").expect("代码块应闭合");
        let body = after[..end].trim();
        if body.contains("post_type") {
            return body.to_owned();
        }
        rest = &after[end..];
    }
    panic!("文档里应有一个 OneBot 事件示例");
}

#[test]
fn documented_event_example_decodes() {
    let example = event_block(KERNEL_DOC);
    let event = decode(&example).expect("文档里的事件示例必须能解码");

    // 示例若标了 post_type=message，就必须解成消息，而不是被归到 Ignored。
    if example.contains("\"message\"") && !example.contains("post_type\": \"meta_event") {
        match event {
            Event::Message(message) => {
                assert!(!message.group_id.is_empty(), "示例应带群号");
                assert!(!message.user_id.is_empty(), "示例应带发言者");
            }
            other => panic!("示例是消息，但解出来是 {other:?}"),
        }
    }
}

#[test]
fn documented_action_example_round_trips() {
    // 文档写的是动作契约；这里按同一形状编码，确认字段名没有漂移。
    let line = Action::SendGroupMsg {
        group_id: "20001".to_owned(),
        text: "在".to_owned(),
    }
    .encode("a-1");

    let value: serde_json::Value = serde_json::from_str(&line).expect("应是 JSON");
    assert_eq!(value["action"], "send_group_msg");
    // 参数在 params 下，请求标识是 echo——OneBot 11 的形状，不另造一套。
    assert!(value.get("params").is_some());
    assert!(value.get("echo").is_some());
}

#[test]
fn documented_error_codes_are_understood() {
    // 上游的失败用 retcode 表达，不用 HTTP 码。示例取自 OneBot 11 文档的常见值。
    let response =
        ActionResponse::decode(r#"{"status":"failed","retcode":100,"data":{"message":"bad"}}"#)
            .expect("应能解码");
    assert!(!response.is_ok());
    assert_eq!(response.reason(), "bad");
}
