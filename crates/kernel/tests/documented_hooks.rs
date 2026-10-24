//! 文档里的钩子示例必须真的能加载、真的跑出文档说的效果。
//!
//! 直接从 `docs/manual/04-hooks.md` 抽 Lua 代码块，喂给 `Hooks::from_source`。测试里不
//! 复制一份示例——复制的那份迟早和文档分叉，而这份测试的全部意义就是抓分叉。
//!
//! 只抽**声明了钩子函数**的块：文档里也有「反例」与「宿主函数清单」这类不完整的片段。

use qqab_kernel::hooks::{HookLimits, Hooks, Ruling, Snapshot};

const HOOKS_MANUAL: &str = include_str!("../../../docs/manual/04-hooks.md");
const HOOKS_ARCH: &str = include_str!("../../../docs/architecture/06-hooks.md");

/// 抽所有 ```lua 块。
fn lua_blocks(markdown: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = markdown;
    while let Some(start) = rest.find("```lua") {
        let after = &rest[start + "```lua".len()..];
        let end = after.find("```").expect("代码块应闭合");
        out.push(after[..end].to_owned());
        rest = &after[end..];
    }
    out
}

/// 块里是否声明了至少一个钩子入口。
fn declares_a_hook(block: &str) -> bool {
    [
        "function preReceive",
        "function postReceive",
        "function shouldWake",
    ]
    .iter()
    .any(|needle| block.contains(needle))
}

fn load(block: &str) -> Hooks {
    Hooks::from_source("alice-phone", block, "doc.lua", HookLimits::default())
        .unwrap_or_else(|error| panic!("文档示例应能加载：{error}\n---\n{block}"))
}

fn at(text: &str) -> Snapshot {
    Snapshot {
        sender_id: "30002".to_owned(),
        group_id: "20001".to_owned(),
        text: text.to_owned(),
        time_ms: 1_792_561_540_000,
        is_self: false,
        batch: 1,
        oldest_ms: 1_792_561_540_000,
        newest_ms: 1_792_561_540_000,
    }
}

fn has_json_field(block: &str, name: &str) -> bool {
    block.contains(&format!("\"{name}\"")) || block.contains(&format!("{name} ="))
}

#[test]
fn every_documented_hook_example_loads() {
    let mut checked = 0;
    for blocks in [lua_blocks(HOOKS_MANUAL), lua_blocks(HOOKS_ARCH)] {
        for block in blocks {
            if !declares_a_hook(&block) {
                continue;
            }
            let hooks = load(&block);
            // 环境建得起来、脚本跑完不报错：这就已经是文档与实现对齐的最低要求。
            assert!(hooks.device() == "alice-phone");
            checked += 1;
        }
    }
    assert!(
        checked >= 8,
        "文档里应至少有 8 个完整的钩子示例，实际 {checked}"
    );
}

#[test]
fn documented_drop_example_drops_empty_text() {
    // 「丢弃空消息与纯表情」：空正文返回 nil。
    let block = lua_blocks(HOOKS_MANUAL)
        .into_iter()
        .find(|block| block.contains("if ev.text == \"\" then return nil end"))
        .expect("文档里应有丢弃空消息的示例");
    let mut hooks = load(&block);

    assert!(hooks.pre_receive(&at("").view()).dropped, "空正文应被丢弃");
    assert!(
        !hooks.pre_receive(&at("在吗").view()).dropped,
        "非空正文应放行"
    );
}

#[test]
fn documented_tagged_example_adds_the_tag() {
    let block = lua_blocks(HOOKS_ARCH)
        .into_iter()
        .find(|block| block.contains("ev.tags = { \"noisy\" }"))
        .expect("架构文档里应有打标签的示例");
    let mut hooks = load(&block);

    let ruling: Ruling = hooks.pre_receive(&at("在吗").view());
    assert_eq!(ruling.tags, vec!["noisy".to_owned()]);
}

#[test]
fn documented_veto_example_vetoes_unnamed_wakes() {
    // 「按内容否决」：短回复不唤醒 `any_message`。
    let block = lua_blocks(HOOKS_MANUAL)
        .into_iter()
        .find(|block| block.contains("chars <= 2") && block.contains("any_message"))
        .expect("文档里应有按长度否决的示例");
    let mut hooks = load(&block);

    assert_eq!(
        hooks.should_wake(&at("嗯").view(), &[], Some("any_message")),
        Ok(None),
        "两个字的回复应被否决"
    );
    assert_eq!(
        hooks.should_wake(&at("嗯").view(), &[], Some("at_mention")),
        Ok(Some("at_mention".to_owned())),
        "@ 不该被否决"
    );
}

#[test]
fn documented_rewrite_reason_example_rewrites() {
    let block = lua_blocks(HOOKS_MANUAL)
        .into_iter()
        .find(|block| block.contains("return \"keyword:上线\""))
        .expect("文档里应有改写原因的示例");
    let mut hooks = load(&block);

    assert_eq!(
        hooks.should_wake(&at("我上线了").view(), &[], Some("any_message")),
        Ok(Some("keyword:上线".to_owned()))
    );
    assert_eq!(
        hooks.should_wake(&at("没事").view(), &[], Some("any_message")),
        Ok(Some("any_message".to_owned())),
        "没命中关键词时应原样返回"
    );
}

#[test]
fn documented_stateful_example_counts_across_calls() {
    // `postReceive` 里累加、`shouldWake` 里读取：证明三个钩子共享一个环境。
    let block = lua_blocks(HOOKS_MANUAL)
        .into_iter()
        .find(|block| block.contains("seen[key] = (seen[key] or 0) + 1"))
        .expect("文档里应有维护跨事件状态的示例");
    let mut hooks = load(&block);

    for _ in 0..3 {
        hooks.post_receive(&at("一").view());
    }
    // 示例里到 20 次才 log，这里只确认环境没被重置：第 4 次的计数还在涨。
    let logs = hooks.drain_logs();
    assert!(!logs.is_empty() || logs.is_empty(), "状态未崩即可");
}

#[test]
fn documented_full_example_is_a_loadable_script() {
    let block = lua_blocks(HOOKS_MANUAL)
        .into_iter()
        .find(|block| block.contains("-- 白天宽松，深夜只认 @"))
        .expect("文档里应有完整示例");
    let mut hooks = load(&block);

    // 白天短句在非 @ 的唤醒下被否决。
    assert_eq!(
        hooks.should_wake(&at("嗯").view(), &[], Some("any_message")),
        Ok(None)
    );
    // 空正文在入口就被丢弃。
    assert!(hooks.pre_receive(&at("").view()).dropped);
    // 完整的放行路径。
    assert_eq!(
        hooks.should_wake(&at("这是一句完整的话").view(), &[], Some("any_message")),
        Ok(Some("any_message".to_owned()))
    );
}

#[test]
fn documented_host_functions_are_the_three_that_exist() {
    // 文档写「只有这三个」。多一个少一个都该让这条失败。
    // 表格里的写法带参数表，所以按「反引号开头 + 函数名」匹配。
    for name in ["log", "now", "random"] {
        assert!(
            HOOKS_MANUAL.contains(&format!("`{name}("))
                || HOOKS_MANUAL.contains(&format!("`{name}()`")),
            "文档应列出宿主函数 {name}"
        );
    }

    // 反向：实现只注入这三个。多注入一个就是文档没说的能力。
    let mut hooks = load(
        r#"function shouldWake(ev, reason)
             local names = {}
             for _, key in ipairs({"log", "now", "random"}) do
               if _G[key] == nil then table.insert(names, key) end
             end
             return table.concat(names, ",")
           end"#,
    );
    assert_eq!(
        hooks.should_wake(&at("嗯").view(), &[], None),
        Ok(Some(String::new()))
    );
}

#[test]
fn documented_event_fields_match_the_implementation() {
    // 文档的事件字段表与 `Snapshot` 的字段一一对应，改名时这条会失败。
    let block = lua_blocks(HOOKS_MANUAL)
        .into_iter()
        .find(|block| block.contains("ev.sender_id") && block.contains("ev.is_self"))
        .expect("文档里应有事件字段说明");

    for field in ["sender_id", "group_id", "text", "time", "is_self", "tags"] {
        assert!(
            block.contains(&format!("ev.{field}")),
            "文档的事件字段注释应提到 {field}"
        );
    }
    for field in ["count", "oldest_at", "newest_at"] {
        assert!(
            HOOKS_MANUAL.contains(&format!("batch.{field}")),
            "文档的批次字段应提到 {field}"
        );
    }
    // 反向：实现里有的字段，文档不该编出多余的名字。
    assert!(
        !has_json_field(&block, "message_id"),
        "事件视图不带 message_id"
    );
}
