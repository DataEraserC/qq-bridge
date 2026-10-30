//! 工具结果的统一出口：预算、分页、落盘（ADR-0022）。
//!
//! 为什么要有这一层：任何「列一堆东西」的接口都会长大——群成员、好友、搜索结果、
//! 合并转发正文、记忆里 grep 出来的命中。以前的做法是超过 8000 字符就截断成一段
//! 字符串塞进 `preview`：模型拿到的是半截 JSON，既解析不了，也不知道到底有多少、
//! 还剩多少。这不是「上限调大一点」能解决的——大群里真有一千多人。
//!
//! 现在三条路，优先级从高到低：
//! 1. **装得下** → 原样返回，一个字节都不动；
//! 2. **主体是一个数组** → 按条分页，带 `total` / `returned` / `has_more` / `next_offset`，
//!    模型能自己接着取；
//! 3. **还是装不下**（单条就巨大，或根本没有数组）→ 落盘，回路径 + 开头一段。
//!
//! 预算是按**字节**算的，不是按条数：这一层要挡的是「撑爆上下文」，条数管不了这件事。
//!
//! 落盘不是「把麻烦丢给用户」：harness 侧（dsh / opencode）本来就有文件读写，
//! 拿到路径能自己翻。同时 head 里给前 2 KB，够判断「要不要去读全文」。

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

/// 单次工具结果给模型的预算：24 KiB。约 6～8k token，一条工具结果占到这么多已经很重了。
///
/// 选 24 KiB 而不是更小：五十来个方法的目录清单、几十人的成员列表都要装得下，
/// 这些是正常操作，不该逼模型分页。真正的大东西（上千人、整份转发）才走分页。
pub const MAX_RESULT_BYTES: usize = 24 * 1024;

/// 分页时给信封留的余量：`_page` 本身也要占字节。
const PAGE_OVERHEAD: usize = 512;

/// 落盘时至少留这么多字给 head，让调用方不读文件也能判断下一步。
const HEAD_BYTES: usize = 2048;

/// 单个落盘文件的上限。超过就只写到这个大小——不要因为一个坏结果把盘写满。
pub const MAX_SPILL_BYTES: usize = 4 * 1024 * 1024;

/// 落盘目录最多留几个文件（新的留下，旧的删）。
pub const MAX_SPILL_FILES: usize = 50;

/// 把结果裁进预算。返回的永远是**合法结构**，绝不是半截 JSON 字符串。
pub fn fit(label: &str, value: Value, spill_dir: &Path) -> Value {
    let size = size_of(&value);
    if size <= MAX_RESULT_BYTES {
        return value;
    }
    if let Some(paged) = page(value.clone(), MAX_RESULT_BYTES) {
        let paged_size = size_of(&paged);
        if paged_size <= MAX_RESULT_BYTES {
            return paged;
        }
    }
    spill(label, value, spill_dir)
}

/// 结果有多大（序列化后的字节数）。
pub fn size_of(value: &Value) -> usize {
    serde_json::to_vec(value).map(|bytes| bytes.len()).unwrap_or(0)
}

/// 结果里那个「主体数组」在哪：顶层就是数组，返回 `None`；在顶层某个字段里，返回字段名。
///
/// 挑最长的那一个。平台结果千奇百怪（`data` / `list` / `members` / 直接一个数组），
/// 与其为每个平台写一条规则，不如认「最长数组就是主体」——错了也只是分错东西，
/// 不会丢数据（`_page.total` 会带出真实条数）。
pub(crate) fn array_slot(value: &Value) -> Option<(Option<String>, usize)> {
    match value {
        Value::Array(items) => Some((None, items.len())),
        Value::Object(map) => map
            .iter()
            .filter_map(|(key, item)| item.as_array().map(|items| (Some(key.clone()), items.len())))
            .max_by_key(|(_, len)| *len),
        _ => None,
    }
}

/// 按条分页。分不出「第一条都装不下」这种情况就返回 `None`，交给落盘。
fn page(value: Value, budget: usize) -> Option<Value> {
    let (slot, total) = array_slot(&value)?;
    if total == 0 {
        return None;
    }
    let items: Vec<Value> = match (&slot, &value) {
        (None, Value::Array(items)) => items.clone(),
        (Some(key), Value::Object(map)) => map.get(key)?.as_array()?.clone(),
        _ => return None,
    };

    let room = budget.saturating_sub(PAGE_OVERHEAD);
    let mut kept: Vec<Value> = Vec::new();
    let mut used = 0usize;
    for item in items {
        let size = size_of(&item) + 1;
        if used + size > room {
            break;
        }
        used += size;
        kept.push(item);
    }
    if kept.is_empty() {
        return None;
    }

    let envelope = json!({
        "total": total,
        "offset": 0,
        "returned": kept.len(),
        "has_more": kept.len() < total,
        "next_offset": kept.len(),
        "note": "结果太长，只给了开头这些（不是全部）。接着取：platform_call 带 offset/limit；\
                 翻流水用 get_history 的 before；其余接口用 offset/limit。",
    });

    match slot {
        None => Some(json!({ "items": kept, "_page": envelope })),
        Some(key) => {
            let mut object = value;
            if let Value::Object(map) = &mut object {
                map.insert(key, Value::Array(kept));
                map.insert("_page".to_owned(), envelope);
            }
            Some(object)
        }
    }
}

/// 落盘兜底：完整内容写文件，回路径 + 开头一段。
fn spill(label: &str, value: Value, spill_dir: &Path) -> Value {
    let size = size_of(&value);
    let text = serde_json::to_string(&value).unwrap_or_else(|_| "{}".to_owned());
    let head = head_of(&text, HEAD_BYTES);

    let path = write_spill(label, &text, spill_dir);
    let mut out = json!({
        "spilled": true,
        "bytes": size,
        "total_chars": text.chars().count(),
        "head": head,
        "note": format!(
            "结果有 {size} 字节，超出单次返回上限（{MAX_RESULT_BYTES}）。完整内容在文件里：\
             能用 bash / read 就直接读它（一行 JSON）；只要开头的话 head 里已经是前 {HEAD_BYTES} 字节。"
        ),
    });
    match path {
        Some(path) => {
            out["path"] = json!(path.display().to_string());
        }
        None => {
            // 写盘也失败：别假装成功，也别把整个结果塞回去撑爆上下文。
            out["spilled"] = json!(false);
            out["note"] = json!(format!(
                "结果有 {size} 字节，超出上限（{MAX_RESULT_BYTES}），而且落盘也失败了。\
                 换更细的参数（filter / limit / group）再查一次。"
            ));
        }
    }
    out
}

fn head_of(text: &str, bytes: usize) -> String {
    let mut out = String::new();
    for ch in text.chars() {
        if out.len() + ch.len_utf8() > bytes {
            break;
        }
        out.push(ch);
    }
    out
}

/// 写一个落盘文件。文件名带时间和序号，同毫秒内多次落盘也不会互相覆盖。
fn write_spill(label: &str, text: &str, spill_dir: &Path) -> Option<PathBuf> {
    std::fs::create_dir_all(spill_dir).ok()?;
    let label: String = label
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '-'
            }
        })
        .collect();
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis())
        .unwrap_or(0);
    let mut path = spill_dir.join(format!("{label}-{stamp}.json"));
    let mut seq = 1;
    while path.exists() {
        path = spill_dir.join(format!("{label}-{stamp}-{seq}.json"));
        seq += 1;
    }

    let body = if text.len() > MAX_SPILL_BYTES {
        // 截到字符边界再写：留半截 UTF-8 会写出一份读不回来的文件。
        format!(
            "{}\n<!-- 已截断：原 {} 字节，这里只写了前 {} 字节 -->",
            head_of(text, MAX_SPILL_BYTES),
            text.len(),
            MAX_SPILL_BYTES
        )
    } else {
        text.to_owned()
    };
    std::fs::write(&path, body).ok()?;
    prune_spill(spill_dir);
    Some(path)
}

/// 落盘目录别无限长：只留最新的 [`MAX_SPILL_FILES`] 个。
fn prune_spill(spill_dir: &Path) {
    let Ok(entries) = std::fs::read_dir(spill_dir) else {
        return;
    };
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                return None;
            }
            let modified = entry.metadata().ok()?.modified().ok()?;
            Some((modified, path))
        })
        .collect();
    if files.len() <= MAX_SPILL_FILES {
        return;
    }
    files.sort();
    let drop = files.len() - MAX_SPILL_FILES;
    for (_, path) in files.into_iter().take(drop) {
        let _ = std::fs::remove_file(path);
    }
}

/// 按 `offset` / `limit` 切结果里的主数组，并把分页信封挂上去（`_page`）。
///
/// 平台方法本身不认识 offset——这是**桥这一层**的分页：平台照旧一次给全，
/// 桥切片后再交给模型。好处是不用为了分页去改五十个方法的签名。
pub fn slice(value: Value, offset: usize, limit: usize) -> Value {
    let Some((slot, total)) = array_slot(&value) else {
        return value;
    };
    let items: Vec<Value> = match (&slot, &value) {
        (None, Value::Array(items)) => items.clone(),
        (Some(key), Value::Object(map)) => match map.get(key).and_then(Value::as_array) {
            Some(items) => items.clone(),
            None => return value,
        },
        _ => return value,
    };
    if offset >= total && total > 0 {
        // 越界不是错误，是一个空页——模型据此知道「翻到底了」。
        let envelope = json!({
            "total": total,
            "offset": offset,
            "returned": 0,
            "has_more": false,
            "next_offset": Value::Null,
            "note": "offset 已经越过总数，这一页是空的。",
        });
        return attach(value, slot, Vec::new(), envelope);
    }

    let taken: Vec<Value> = items.into_iter().skip(offset).take(limit).collect();
    let returned = taken.len();
    let has_more = offset + returned < total;
    let envelope = json!({
        "total": total,
        "offset": offset,
        "returned": returned,
        "has_more": has_more,
        "next_offset": if has_more { json!(offset + returned) } else { Value::Null },
    });
    attach(value, slot, taken, envelope)
}

fn attach(value: Value, slot: Option<String>, items: Vec<Value>, envelope: Value) -> Value {
    match slot {
        None => json!({ "items": items, "_page": envelope }),
        Some(key) => {
            let mut object = value;
            if let Value::Object(map) = &mut object {
                map.insert(key, Value::Array(items));
                map.insert("_page".to_owned(), envelope);
            }
            object
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn dir() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "acb-render-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        path
    }

    #[test]
    fn small_results_pass_through_untouched() {
        let value = json!({"members": [{"id": 1}, {"id": 2}]});
        let out = fit("test", value.clone(), &dir());
        assert_eq!(out, value, "装得下就一个字节都不动，不加信封");
    }

    #[test]
    fn big_lists_are_paged_with_a_total() {
        let members: Vec<Value> = (0..2000).map(|i| json!({ "id": i, "name": "x" })).collect();
        let value = json!({ "data": members, "retcode": 0 });
        let out = fit("group.get_member_list", value, &dir());

        assert_eq!(out["_page"]["total"], 2000, "总数一定要给，哪怕是截的");
        assert_eq!(out["_page"]["has_more"], true);
        assert_eq!(out["_page"]["offset"], 0);
        let returned = out["_page"]["returned"].as_u64().expect("有数字");
        assert!(returned > 0 && returned < 2000);
        assert_eq!(out["data"].as_array().expect("还是数组").len(), returned as usize);
        assert_eq!(out["retcode"], 0, "不认识的字断不能丢");
        assert!(size_of(&out) <= MAX_RESULT_BYTES);
    }

    #[test]
    fn top_level_arrays_are_wrapped_not_truncated() {
        let items: Vec<Value> = (0..3000).map(|i| json!({ "id": i })).collect();
        let out = fit("friend.list", Value::Array(items), &dir());
        assert_eq!(out["_page"]["total"], 3000);
        assert!(out["items"].as_array().expect("数组还在").len() > 0);
        // 关键：结果必须是能解析的 JSON，不是半截字符串。
        assert!(serde_json::to_string(&out).is_ok());
    }

    #[test]
    fn one_huge_item_is_spilled_to_a_file() {
        let dir = dir();
        let huge = json!({ "text": "あ".repeat(40_000) });
        let out = fit("forward.get_content", huge, &dir);

        assert_eq!(out["spilled"], true);
        let path = out["path"].as_str().expect("要给路径");
        assert!(std::path::Path::new(path).exists(), "文件得真的在");
        assert!(out["head"].as_str().expect("给开头").chars().count() > 0);
        assert!(out["bytes"].as_u64().expect("给字节数") > MAX_RESULT_BYTES as u64);
    }

    #[test]
    fn slice_reports_paging_so_the_model_can_continue() {
        let value = json!({ "members": (0..10).map(|i| json!({"id": i})).collect::<Vec<_>>() });
        let first = slice(value.clone(), 0, 4);
        assert_eq!(first["_page"]["total"], 10);
        assert_eq!(first["_page"]["returned"], 4);
        assert_eq!(first["_page"]["has_more"], true);
        assert_eq!(first["_page"]["next_offset"], 4);

        let second = slice(value.clone(), 8, 4);
        assert_eq!(second["_page"]["returned"], 2);
        assert_eq!(second["_page"]["has_more"], false);
        assert!(second["_page"]["next_offset"].is_null());

        let past = slice(value, 99, 4);
        assert_eq!(past["_page"]["returned"], 0);
        assert_eq!(past["_page"]["total"], 10, "越界也要知道自己错过了多少");
    }

    #[test]
    fn slice_leaves_non_list_results_alone() {
        let value = json!({"ok": true, "platform_id": "p1"});
        assert_eq!(slice(value.clone(), 0, 10), value);
    }
}
