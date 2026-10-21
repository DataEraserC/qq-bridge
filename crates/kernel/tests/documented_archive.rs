//! 架构文档里的归档示例与上限必须和实现对得上。
//!
//! 直接读 `docs/architecture/03-kernel.md`，从代码块里抽出 JSON 来解析，从表格里抽出上限
//! 来比对。测试里不复制一份——复制的那份迟早和文档分叉，而这份测试的全部意义就是抓分叉。

use qqab_kernel::archive::{
    Archive, DEFAULT_CONTEXT_MESSAGES, DEFAULT_MAX_BYTES, DEFAULT_MAX_MESSAGES, Limits, render,
};

const KERNEL_DOC: &str = include_str!("../../../docs/architecture/03-kernel.md");

/// 从 Markdown 里取出第一个 ```json 代码块的内容。
fn first_json_block(markdown: &str) -> String {
    let fence = "```json";
    let start = markdown.find(fence).expect("文档里应有 json 代码块") + fence.len();
    let rest = &markdown[start..];
    let end = rest.find("```").expect("代码块应闭合");
    rest[..end].trim().to_owned()
}

/// 取上限表格里某一行的「默认」单元格。
fn documented_default(name: &str) -> String {
    let needle = format!("| `{name}` |");
    let line = KERNEL_DOC
        .lines()
        .find(|line| line.trim_start().starts_with(&needle))
        .unwrap_or_else(|| panic!("文档里应有 {name} 这一行上限"));
    let cell = line.split('|').nth(2).expect("表格行应有三列").trim();
    cell.to_owned()
}

#[test]
fn documented_defaults_match_implementation() {
    assert_eq!(
        documented_default("max_messages")
            .parse::<usize>()
            .expect("条数是数字"),
        DEFAULT_MAX_MESSAGES
    );
    assert_eq!(
        documented_default("context_messages")
            .parse::<usize>()
            .expect("条数是数字"),
        DEFAULT_CONTEXT_MESSAGES
    );

    let bytes = documented_default("max_bytes");
    let mib = bytes
        .strip_suffix(" MiB")
        .unwrap_or_else(|| panic!("字节上限应写成 MiB，文档是 {bytes:?}"))
        .parse::<usize>()
        .expect("MiB 前面是数字");
    assert_eq!(mib * 1024 * 1024, DEFAULT_MAX_BYTES);
}

#[test]
fn documented_shard_parses() {
    let json = first_json_block(KERNEL_DOC);
    let archive = Archive::from_json(&json, Limits::default()).expect("文档里的分片示例必须能解析");

    assert_eq!(archive.watermark("alice", "20001"), 34);
    assert_eq!(archive.retained("alice"), 2);
    // 序号 33 在水位之前，34 是自己发的——两条都不是未读。
    assert!(archive.unread("alice", "20001").is_empty());

    let context = archive.context("alice", "20001", 10);
    assert_eq!(context.len(), 2);
    assert_eq!(context[0].message_id, "7451");
    assert!(context[0].at_self, "示例标了 @");
    assert_eq!(context[0].sender_name.as_deref(), Some("小明"));
    assert!(context[1].own, "示例里第一条自己发的是 own");
    assert_eq!(context[1].sender_id, "10001");
}

#[test]
fn documented_render_matches_implementation() {
    let json = first_json_block(KERNEL_DOC);
    let archive = Archive::from_json(&json, Limits::default()).expect("应能解析");

    // 号码是身份，括号里是显示名；被点到的消息带 @你 前缀。
    assert_eq!(
        render(&archive.context("alice", "20001", 10)),
        "30002（小明）：@你 在吗\n我：在"
    );
}

#[test]
fn documented_shard_round_trips() {
    let json = first_json_block(KERNEL_DOC);
    let archive = Archive::from_json(&json, Limits::default()).expect("应能解析");

    let written = archive.to_json().expect("应能序列化");
    let again = Archive::from_json(&written, Limits::default()).expect("应能再解析");

    assert_eq!(
        again.to_json().expect("再序列化"),
        written,
        "文档写的、我们写的、我们读的是同一套字段名"
    );
}
