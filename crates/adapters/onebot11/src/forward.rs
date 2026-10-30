//! 合并转发（「聊天记录」卡片）展开成一层带路径的节点。
//!
//! 平台一次只给一层：外层节点里可能又只是一个转发 id，想看到最里面那句「对的对的」
//! 得逐层再调一次。以前这件事落在模型身上——三次调用、三份原始 JSON、自己在脑子里
//! 对齐，还很容易把嵌套关系看错。这里是适配器该干的活：**按需递归取，摊平成一张
//! 带 `path` / `depth` 的表**，模型看见的就是「谁在第几层说了什么」。
//!
//! 三条硬约束：
//! - **有上限**：`depth`（默认 3、最多 5）和 `max_nodes`（默认 200、最多 500）。转发可以
//!   套转发，没有上限就是无限次平台往返。
//! - **防环**：同一个转发 id 只展开一次，防止平台数据自引用。
//! - **不丢**：每个节点都带上原始 `content` 段，模型要转发回去时照抄即可。

use acb_kernel::CallError;
use serde_json::{Value, json};

/// 默认展开几层。
pub const DEFAULT_DEPTH: usize = 3;
/// 最多展开几层——再深就是「转发里的转发里的转发」，收益已经很低。
pub const MAX_DEPTH: usize = 5;
/// 默认最多摊多少条节点。
pub const DEFAULT_MAX_NODES: usize = 200;
/// 节点上限的天花板。再多就该让人自己去平台里看了。
pub const MAX_MAX_NODES: usize = 500;

/// 展开一棵合并转发。
///
/// `fetch` 按转发 id 向平台取一层（已经剥掉外壳的 `data`）；测试里换成假数据即可，
/// 不用起平台。
pub fn expand(
    root: &str,
    depth: usize,
    max_nodes: usize,
    fetch: &mut impl FnMut(&str) -> Result<Value, CallError>,
) -> Result<Value, CallError> {
    let depth = depth.clamp(1, MAX_DEPTH);
    let max_nodes = max_nodes.clamp(1, MAX_MAX_NODES);

    let mut nodes: Vec<Value> = Vec::new();
    let mut visited: Vec<String> = vec![root.to_owned()];
    let mut truncated = false;

    walk(
        root,
        0,
        depth,
        max_nodes,
        "",
        fetch,
        &mut nodes,
        &mut visited,
        &mut truncated,
    )?;

    // 摊平后的一句话总结：嵌套再深，读这一段也知道最里面说了什么。
    let leaf_text = nodes
        .iter()
        .filter_map(|node| node.get("text").and_then(Value::as_str))
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n");

    Ok(json!({
        "id": root,
        "depth": depth,
        "returned": nodes.len(),
        "has_more": truncated,
        "nodes": nodes,
        "leaf_text": leaf_text,
    }))
}

#[allow(clippy::too_many_arguments)]
fn walk(
    id: &str,
    level: usize,
    depth: usize,
    max_nodes: usize,
    prefix: &str,
    fetch: &mut impl FnMut(&str) -> Result<Value, CallError>,
    out: &mut Vec<Value>,
    visited: &mut Vec<String>,
    truncated: &mut bool,
) -> Result<(), CallError> {
    let payload = fetch(id)?;

    for (index, node) in nodes_of(&payload).into_iter().enumerate() {
        if out.len() >= max_nodes {
            *truncated = true;
            return Ok(());
        }

        let path = if prefix.is_empty() {
            index.to_string()
        } else {
            format!("{prefix}.{index}")
        };
        let content = content_of(&node);
        let nested = forward_id_of(&content);
        let text = flatten(&content);

        let mut entry = serde_json::Map::new();
        entry.insert("path".to_owned(), json!(path));
        entry.insert("depth".to_owned(), json!(level));
        entry.insert("sender".to_owned(), sender_of(&node));
        if let Some(at) = node.get("time").and_then(Value::as_i64) {
            // 平台给的是秒；核心里时间一律毫秒。
            entry.insert("at".to_owned(), json!(at.saturating_mul(1000)));
        }
        entry.insert("text".to_owned(), json!(text));
        if let Some(nested) = &nested {
            entry.insert("forward_id".to_owned(), json!(nested));
        }
        // 原始段照抄，模型要原样发出去时不用再猜。
        entry.insert("content".to_owned(), Value::Array(content));
        out.push(Value::Object(entry));

        if level + 1 >= depth {
            continue;
        }
        let Some(nested) = nested else { continue };
        if visited.contains(&nested) {
            continue;
        }
        visited.push(nested.clone());
        walk(
            &nested,
            level + 1,
            depth,
            max_nodes,
            &path,
            fetch,
            out,
            visited,
            truncated,
        )?;
        if *truncated {
            return Ok(());
        }
    }

    Ok(())
}

/// 平台返回的一层节点。三种形状都见过：`data.messages`、`data.message`、直接数组。
fn nodes_of(payload: &Value) -> Vec<Value> {
    for key in ["messages", "message", "nodes", "content"] {
        if let Some(list) = payload.get(key).and_then(Value::as_array) {
            return list.clone();
        }
    }
    match payload {
        Value::Array(items) => items.clone(),
        _ => Vec::new(),
    }
}

/// 一个节点的内容段。有的实现外面套了一层 `{"type":"node","data":{...}}`。
fn content_of(node: &Value) -> Vec<Value> {
    if node.get("type").and_then(Value::as_str) == Some("node") {
        if let Some(data) = node.get("data") {
            return content_of(data);
        }
    }
    for key in ["content", "message"] {
        if let Some(list) = node.get(key).and_then(Value::as_array) {
            return list.clone();
        }
    }
    Vec::new()
}

fn sender_of(node: &Value) -> Value {
    let inner = if node.get("type").and_then(Value::as_str) == Some("node") {
        node.get("data").unwrap_or(node)
    } else {
        node
    };
    let sender = inner.get("sender").cloned().unwrap_or(Value::Null);
    let id = sender
        .get("user_id")
        .or_else(|| sender.get("uin"))
        .or_else(|| inner.get("user_id"))
        .or_else(|| inner.get("uin"))
        .map(text_of)
        .unwrap_or_default();
    let name = sender
        .get("nickname")
        .or_else(|| sender.get("card"))
        .or_else(|| inner.get("name"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    json!({ "id": id, "name": name })
}

/// 节点内容里嵌着的下一个转发 id。
fn forward_id_of(content: &[Value]) -> Option<String> {
    content.iter().find_map(|segment| {
        if segment.get("type").and_then(Value::as_str) != Some("forward") {
            return None;
        }
        segment
            .get("data")
            .and_then(|data| data.get("id"))
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .map(str::to_owned)
    })
}

/// 段数组 → 一行字。只用于「一眼看懂说了什么」，原始段另存一份。
fn flatten(content: &[Value]) -> String {
    let mut out = String::new();
    for segment in content {
        let kind = segment.get("type").and_then(Value::as_str).unwrap_or("");
        let data = segment.get("data").cloned().unwrap_or(Value::Null);
        let field = |key: &str| data.get(key).and_then(Value::as_str).unwrap_or_default();
        match kind {
            "text" => out.push_str(field("text")),
            "at" => {
                let qq = field("qq");
                if qq == "all" {
                    out.push_str("@全体成员");
                } else {
                    out.push('@');
                    out.push_str(qq);
                }
            }
            "image" => out.push_str("[图片]"),
            "record" => out.push_str("[语音]"),
            "video" => out.push_str("[视频]"),
            "file" => {
                out.push_str("[文件 ");
                out.push_str(field("name"));
                out.push(']');
            }
            "face" => {
                out.push_str("[表情 ");
                out.push_str(field("id"));
                out.push(']');
            }
            "forward" => out.push_str("[合并转发]"),
            "" => {}
            other => {
                out.push('[');
                out.push_str(other);
                out.push(']');
            }
        }
    }
    out
}

fn text_of(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Number(number) => number.to_string(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 一层：两个人说话。不该有任何嵌套字段。
    #[test]
    fn a_flat_forward_is_listed_in_order() {
        let mut fetch = |id: &str| {
            assert_eq!(id, "root");
            Ok(json!({
                "messages": [
                    {"sender": {"user_id": 10001, "nickname": "老王"}, "time": 1_700_000_000,
                     "content": [{"type": "text", "data": {"text": "看这个"}}]},
                    {"sender": {"user_id": 10002, "nickname": "小李"},
                     "content": [{"type": "image", "data": {"url": "u"}}]},
                ]
            }))
        };

        let tree = expand("root", 3, 100, &mut fetch).expect("能展开");
        assert_eq!(tree["returned"], 2);
        assert_eq!(tree["has_more"], false);
        assert_eq!(tree["nodes"][0]["path"], "0");
        assert_eq!(tree["nodes"][0]["depth"], 0);
        assert_eq!(tree["nodes"][0]["sender"]["name"], "老王");
        assert_eq!(tree["nodes"][0]["at"], 1_700_000_000_000i64);
        assert_eq!(tree["nodes"][1]["text"], "[图片]");
        assert_eq!(tree["leaf_text"], "看这个\n[图片]");
        assert!(
            tree["nodes"][0].get("forward_id").is_none(),
            "没有嵌套就别给空字段"
        );
    }

    /// 嵌套：外层那句「转给你」+ 内层「对的对的」，一次调用全给到。
    #[test]
    fn nested_forwards_are_flattened_with_paths_and_leaf_text() {
        let mut fetch = |id: &str| match id {
            "root" => Ok(json!({
                "messages": [
                    {"sender": {"user_id": 1, "nickname": "老王"},
                     "content": [{"type": "text", "data": {"text": "转给你"}}]},
                    {"sender": {"user_id": 2, "nickname": "小李"},
                     "content": [{"type": "forward", "data": {"id": "inner"}}]},
                ]
            })),
            "inner" => Ok(json!({
                "messages": [
                    {"sender": {"user_id": 3, "nickname": "小张"},
                     "content": [{"type": "text", "data": {"text": "对的对的"}}]},
                ]
            })),
            other => panic!("不该去取 {other}"),
        };

        let tree = expand("root", 3, 100, &mut fetch).expect("能展开");
        let nodes = tree["nodes"].as_array().expect("有节点");
        assert_eq!(nodes.len(), 3);
        assert_eq!(nodes[1]["path"], "1");
        assert_eq!(nodes[1]["forward_id"], "inner");
        assert_eq!(nodes[2]["path"], "1.0", "内层的路径挂在外层下面");
        assert_eq!(nodes[2]["depth"], 1);
        assert_eq!(nodes[2]["text"], "对的对的");
        assert!(tree["leaf_text"].as_str().expect("有一句话").contains("对的对的"));
    }

    /// `depth: 1` 就只看一层：内层留个 `forward_id`，让模型自己决定要不要再展开。
    #[test]
    fn depth_one_stops_at_the_outer_layer() {
        let mut calls = 0;
        let mut fetch = |_id: &str| {
            calls += 1;
            Ok(json!({
                "messages": [
                    {"sender": {"user_id": 2}, "content": [
                        {"type": "forward", "data": {"id": "inner"}}
                    ]},
                ]
            }))
        };

        let tree = expand("root", 1, 100, &mut fetch).expect("能展开");
        assert_eq!(tree["returned"], 1);
        assert_eq!(tree["nodes"][0]["forward_id"], "inner");
        assert_eq!(calls, 1, "给了一层就别再往下打");
    }

    /// 套娃有上限：id 绕回自己时不能再取一次。
    #[test]
    fn a_cycle_does_not_loop_forever() {
        let mut fetch = |id: &str| {
            Ok(json!({
                "messages": [
                    {"sender": {"user_id": 1}, "content": [
                        {"type": "forward", "data": {"id": if id == "root" { "b" } else { "root" }}}
                    ]},
                ]
            }))
        };

        let tree = expand("root", 5, 100, &mut fetch).expect("能展开");
        assert_eq!(tree["returned"], 2, "root → b 之后就该收手");
        assert_eq!(tree["has_more"], false);
    }

    /// 节点太多要截断，并且说清楚「还有」，不能悄悄少给。
    #[test]
    fn too_many_nodes_are_cut_and_flagged() {
        let mut fetch = |_id: &str| {
            Ok(json!({
                "messages": (0..10)
                    .map(|index| json!({
                        "sender": {"user_id": index},
                        "content": [{"type": "text", "data": {"text": format!("第 {index} 条")}}],
                    }))
                    .collect::<Vec<_>>()
            }))
        };

        let tree = expand("root", 1, 4, &mut fetch).expect("能展开");
        assert_eq!(tree["returned"], 4);
        assert_eq!(tree["has_more"], true);
    }

    /// 形状容错：有的实现在外面套 `{"type":"node","data":{...}}`。
    #[test]
    fn node_wrapped_in_a_node_segment_still_reads() {
        let mut fetch = |_id: &str| {
            Ok(json!({
                "messages": [
                    {"type": "node", "data": {
                        "sender": {"user_id": 9, "nickname": "小赵"},
                        "content": [{"type": "text", "data": {"text": "里面"}}],
                    }},
                ]
            }))
        };

        let tree = expand("root", 1, 10, &mut fetch).expect("能展开");
        assert_eq!(tree["nodes"][0]["sender"]["id"], "9");
        assert_eq!(tree["nodes"][0]["text"], "里面");
    }

    /// 真机（NapCat）回的载荷长这样：`data.messages[]`，内容是 `message` 而不是 `content`，
    /// 时间单位是秒，发送者在 `sender.user_id/nickname`。这份是从真响应里抄下来改短的——
    /// 平台换字段名时这条会先红。
    #[test]
    fn a_real_napcat_payload_reads() {
        let mut fetch = |_id: &str| {
            Ok(json!({
                "messages": [
                    {
                        "self_id": 3683924053i64, "user_id": 7047694, "time": 1793376467,
                        "message_id": 1400385454, "message_seq": 10831, "real_id": 1400385454,
                        "message_type": "private", "post_type": "message", "sub_type": "friend",
                        "sender": {"user_id": 7047694, "nickname": "一生未见空投"},
                        "raw_message": "", "font": 14, "message_format": "array",
                        "message": [{"type": "image", "data": {"file": "0B127A.png", "summary": "[图片]"}}],
                    },
                    {
                        "self_id": 3683924053i64, "user_id": 1094950020, "time": 1793376485,
                        "message_id": 186438600, "message_seq": 6557, "real_id": 186438600,
                        "sender": {"user_id": 1094950020, "nickname": "yswjkt"},
                        "message": [{"type": "text", "data": {"text": "零点发这个，是想再养一个？"}}],
                    },
                ]
            }))
        };

        let tree = expand("root", 3, 100, &mut fetch).expect("能展开");
        assert_eq!(tree["returned"], 2);
        assert_eq!(tree["nodes"][0]["sender"]["name"], "一生未见空投");
        assert_eq!(tree["nodes"][0]["text"], "[图片]");
        assert_eq!(tree["nodes"][0]["at"], 1_793_376_467_000i64, "秒要换成毫秒");
        assert_eq!(tree["nodes"][1]["text"], "零点发这个，是想再养一个？");
        assert!(tree["leaf_text"].as_str().unwrap_or("").contains("零点发这个"));
    }
}
