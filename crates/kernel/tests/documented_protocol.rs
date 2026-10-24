//! 协议文档与帧定义必须对得上。
//!
//! 直接读 `docs/architecture/05-protocol.md` 里的 `deliver` 字段表，与 `Deliver` 结构实际的
//! 序列化结果比对。这张表是适配器作者唯一会读的东西——它写少了字段，适配器就少实现一条；
//! 写多了字段，适配器就实现一个内核不发的字段。

use qqab_proto::{Block, Deliver, Frame, Scope, decode, encode};

const PROTOCOL_DOC: &str = include_str!("../../../docs/architecture/05-protocol.md");

/// 取 `### deliver` 小节里那张字段表的字段名，按文档顺序。
fn documented_deliver_fields() -> Vec<String> {
    let start = PROTOCOL_DOC
        .find("### `deliver`")
        .expect("文档里应有 deliver 小节");
    let body = &PROTOCOL_DOC[start..];
    let table_start = body.find("| 字段 |").expect("应有字段表");
    let table = &body[table_start..];
    let table_end = table[2..]
        .find("\n\n")
        .map(|offset| offset + 2)
        .expect("字段表应有结束");

    table[..table_end]
        .lines()
        .skip(2) // 表头与分隔行
        .filter_map(|line| {
            let cell = line.strip_prefix('|')?.split('|').next()?.trim();
            let name = cell.strip_prefix('`')?.strip_suffix('`')?;
            Some(name.to_owned())
        })
        .collect()
}

fn sample_deliver() -> Deliver {
    Deliver {
        id: "d-1".into(),
        ticket: "t1-1792561540309".into(),
        memory: "alice".into(),
        persona: "你是……".into(),
        blocks: vec![Block::Text {
            text: "在吗".into(),
        }],
        scope: Scope {
            group: "20001".into(),
            group_name: None,
            send_accounts: vec!["main".into()],
        },
        wake_reason: "at_mention".into(),
        notes: String::new(),
    }
}

#[test]
fn deliver_table_lists_every_field() {
    let documented = documented_deliver_fields();
    assert!(!documented.is_empty(), "字段表不该是空的");

    let line = encode(&Frame::Deliver(sample_deliver())).expect("应能编码");
    let value: serde_json::Value = serde_json::from_str(&line).expect("应是 JSON");
    let object = value.as_object().expect("应是对象");

    let mut actual: Vec<&str> = object
        .keys()
        .map(String::as_str)
        .filter(|key| *key != "type")
        .collect();
    actual.sort_unstable();

    let mut expected: Vec<&str> = documented.iter().map(String::as_str).collect();
    expected.sort_unstable();

    assert_eq!(
        actual, expected,
        "协议文档的 deliver 字段表与帧定义漂移了：文档 {documented:?}"
    );
}

#[test]
fn documented_example_line_decodes() {
    // 帧格式小节里那行示例必须能解成 Deliver——文档里的示例是适配器作者的第一个输入。
    // 示例可以跨行，从 ```json 围栏里整块取出来解析。
    let start = PROTOCOL_DOC
        .find("```json")
        .expect("文档里应有 json 代码块")
        + "```json".len();
    let rest = &PROTOCOL_DOC[start..];
    let block = &rest[..rest.find("```").expect("代码块应闭合")];
    let line = block.trim().replace('\n', " ");

    assert!(
        matches!(decode(&line), Ok(Frame::Deliver(_))),
        "文档示例行必须能解码：{line}"
    );
}

#[test]
fn ticket_is_required_in_deliver() {
    // 票是工具调用的唯一身份来源。文档表里少了它，适配器就没法把票带回去。
    assert!(
        documented_deliver_fields()
            .iter()
            .any(|name| name == "ticket"),
        "deliver 的字段表必须列出 ticket"
    );

    let without = r#"{"type":"deliver","id":"d-1","memory":"alice","persona":"p","blocks":[],"scope":{"group":"1"},"wake_reason":"any_message"}"#;
    assert!(decode(without).is_err(), "没有票的 deliver 结构非法");
}
