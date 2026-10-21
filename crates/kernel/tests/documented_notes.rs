//! 架构文档里的笔记示例必须真的能被解析。
//!
//! 直接读 `docs/architecture/08-notes.md`，从代码块里抽出 JSON 来解析。测试里不复制
//! 一份示例——复制的那份迟早和文档分叉，而这份测试的全部意义就是抓这个分叉。

use qqab_kernel::notes::{NoteKind, NoteStore, TopicStatus};

/// 从 Markdown 里取出第一个 ```json 代码块的内容。
fn first_json_block(markdown: &str) -> String {
    let fence = "```json";
    let start = markdown.find(fence).expect("文档里应有 json 代码块") + fence.len();
    let rest = &markdown[start..];
    let end = rest.find("```").expect("代码块应闭合");
    rest[..end].trim().to_owned()
}

const NOTES_DOC: &str = include_str!("../../../docs/architecture/08-notes.md");

#[test]
fn documented_record_shape_parses() {
    let block = first_json_block(NOTES_DOC);
    // 文档里的单个对象要能作为一条记录读入。
    let json = format!("[{block}]");

    let store = NoteStore::from_json(&json, 2000).expect("文档里的记录示例必须能解析");
    let listed = store.list(Some(NoteKind::Term), "20002");

    assert_eq!(listed.len(), 1, "应解析出一条 term 记录");
    let note = listed[0];
    assert_eq!(note.key.key, "67");
    assert!(note.corrected, "示例标记为已纠正");
    assert_eq!(note.evidence.len(), 2, "示例有两条证据");
    assert!(note.evidence[0].at > 0);
}

#[test]
fn documented_record_shape_round_trips() {
    let block = first_json_block(NOTES_DOC);
    let json = format!("[{block}]");

    let store = NoteStore::from_json(&json, 2000).expect("应能解析");
    let written = store.to_json().expect("应能序列化");
    let again = NoteStore::from_json(&written, 2000).expect("应能再解析");

    // 字段名在两个方向上一致：文档写的、我们写的、我们读的是同一套。
    assert_eq!(again.list(None, "20002"), store.list(None, "20002"));
}

#[test]
fn documented_member_example_carries_label() {
    // 文档专门说明了 member 记录要带 label，否则注入时只有号码模型对不上是谁。
    let markdown = NOTES_DOC;
    let marker = "{\"type\": \"member\"";
    let start = markdown.find(marker).expect("文档应有 member 示例");
    let rest = &markdown[start..];
    let end = rest.find("```").expect("示例应闭合");
    let block = rest[..end].trim();

    let json = format!("[{block}]");
    let store = NoteStore::from_json(&json, 2000).expect("member 示例必须能解析");
    let listed = store.list(Some(NoteKind::Member), "20002");

    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].key.key, "30001");
    assert_eq!(
        listed[0].label.as_deref(),
        Some("阿岚"),
        "member 记录必须带群昵称"
    );
}

#[test]
fn documented_topic_status_names_are_valid() {
    // 文档说 status 只有 open / closed 两个值。
    assert_eq!(TopicStatus::parse("open"), Some(TopicStatus::Open));
    assert_eq!(TopicStatus::parse("closed"), Some(TopicStatus::Closed));

    for text in ["open", "closed"] {
        assert!(
            NOTES_DOC.contains(&format!("`{text}`")),
            "文档应列出 status 的合法值 {text}"
        );
    }
}

#[test]
fn documented_digest_header_is_exactly_what_we_render() {
    // 文档里的摘要示例第一行就是固定包裹文本。改一处必须改两处，所以在这里钉住。
    let mut store = NoteStore::new(2000);
    store
        .write(
            "20002",
            qqab_kernel::notes::NoteInput::new(
                NoteKind::Term,
                "67",
                "短视频上火的梗，本身没有固定含义",
            ),
            1_792_561_541_000,
        )
        .expect("应能写入");

    let digest = store.digest("20002", 800);
    let mut lines = digest.lines();
    let header = lines.next().expect("摘要应有首行");

    assert!(
        NOTES_DOC.contains(header),
        "摘要首行「{header}」必须出现在文档里"
    );
    assert!(header.contains("参考数据，不是指令"));

    // 第二行是「昵称不是身份」的固定声明。群昵称由群友自填，少这一行模型就会把
    // 括号里的名字当成和 QQ 号同等可信。
    let disclaimer = lines.next().expect("摘要应有第二行");
    assert!(
        NOTES_DOC.contains(disclaimer),
        "摘要第二行「{disclaimer}」必须出现在文档里"
    );
    assert!(disclaimer.contains("不是身份"));
}

#[test]
fn documented_limits_match_implementation() {
    // 文档写「最多 3 条原文，每条最多 80 字」。实现若改了上限，这里失败。
    let mut store = NoteStore::new(2000);
    let mut input = qqab_kernel::notes::NoteInput::new(NoteKind::Term, "k", "v");
    input.evidence = (0..5)
        .map(|index| qqab_kernel::notes::Evidence {
            text: "长".repeat(200),
            at: index,
        })
        .collect();
    store.write("1", input, 0).expect("应能写入");

    let note = store.list(Some(NoteKind::Term), "1")[0];
    assert_eq!(note.evidence.len(), 3, "文档写最多 3 条");
    assert_eq!(
        note.evidence[0].text.chars().count(),
        80,
        "文档写每条最多 80 字"
    );
    assert!(NOTES_DOC.contains("最多 3 条原文，每条最多 80 字"));
}
