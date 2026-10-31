//! 结构化配置补丁：管理页表单提交的修改，外科手术式写回 TOML（ADR-0009）。
//!
//! 为什么不是「JSON 反序列化再整个重新生成 TOML」：那会把用户手写的注释和排版
//! 全毁掉。用 toml_edit 在原文档上打补丁，只动表单碰过的键——`toml` crate 本身
//! 就是 toml_edit 的封装，解析行为完全一致。
//!
//! 补丁原子性：任何一条 op 失败就整体报错，一个字节都不落盘（落盘前的校验、
//! 备份由 console 的保存流程统一管，这里只负责「改文本」）。

use toml_edit::{value, DocumentMut, Item, Table};

/// 应用一组补丁操作，返回（改完的 TOML 全文，人话改动清单）。
pub fn apply_patch(text: &str, ops: &[serde_json::Value]) -> Result<(String, Vec<String>), String> {
    let mut doc: DocumentMut = text
        .parse()
        .map_err(|error| format!("现有配置解析不开（先在 TOML 编辑器里修好）：{error}"))?;
    let mut changes = Vec::new();

    for (index, op) in ops.iter().enumerate() {
        let kind = op.get("op").and_then(serde_json::Value::as_str).unwrap_or("");
        let where_ = format!("第 {} 条补丁（{kind}）", index + 1);
        match kind {
            "session" => session_op(&mut doc, op, &mut changes),
            "subscription" => subscription_op(&mut doc, op, &mut changes),
            "persona" => persona_op(&mut doc, op, &mut changes),
            "memory" => memory_op(&mut doc, op, &mut changes),
            "connection" => connection_op(&mut doc, op, &mut changes),
            "global" => global_op(&mut doc, op, &mut changes),
            other => Err(format!("不认识的 op 类型「{other}」（能用的：session / subscription / persona / memory / connection / global）")),
        }
        .map_err(|error| format!("{where_}：{error}"))?;
    }

    Ok((doc.to_string(), changes))
}

// ---------------------------------------------------------------------------
// 各类操作
// ---------------------------------------------------------------------------

/// 会话级修改：`{"op":"session","id":"main","patch":{"persona":"p2","model":null}}`。
/// 只改出现的键；`model` 给 null 表示清掉（= 由 harness 侧决定）。
fn session_op(
    doc: &mut DocumentMut,
    op: &serde_json::Value,
    changes: &mut Vec<String>,
) -> Result<(), String> {
    let id = string_field(op, "id")?;
    let patch = op
        .get("patch")
        .and_then(serde_json::Value::as_object)
        .ok_or("要带 patch 对象")?;
    let table = session_table_mut(doc, &id).ok_or(format!("没有会话 {id}"))?;

    for (key, val) in patch {
        match key.as_str() {
            "name" | "persona" => {
                let text = val.as_str().ok_or(format!("{key} 要是字符串"))?;
                table.insert(key.as_str(), value(text.to_owned()));
                changes.push(format!("会话 {id}：{key} → {text}"));
            }
            "model" => match val {
                serde_json::Value::Null => {
                    table.remove("model");
                    changes.push(format!("会话 {id}：模型清掉（harness 侧自定）"));
                }
                serde_json::Value::String(text) => {
                    table.insert("model", value(text.clone()));
                    changes.push(format!("会话 {id}：模型 → {text}"));
                }
                _ => return Err("model 要是字符串或 null".into()),
            },
            "memories" => {
                let list = str_list(val).map_err(|_| "memories 要是字符串数组".to_owned())?;
                table.insert("memories", str_array(&list));
                changes.push(format!("会话 {id}：记忆 → [{}]", list.join(", ")));
            }
            other => return Err(format!("会话 patch 不认识字段 {other}（能改：name / persona / model / memories）")),
        }
    }
    Ok(())
}

/// 订阅修改。两种形态：
/// - 删除：`{"op":"subscription","session":"main","room":"qq:group/123","remove":true}`
/// - 增改：`{"op":"subscription","session":"main","connection":"qq","room":"group/456","wake":{...}}`
///   已存在就只动 wake 里出现的键（null = 删键回到默认），不存在就整条新建。
/// `room` 写 `qq:group/123`（带连接前缀）或 `group/123`（配 connection 字段）都行。
fn subscription_op(
    doc: &mut DocumentMut,
    op: &serde_json::Value,
    changes: &mut Vec<String>,
) -> Result<(), String> {
    let session_id = string_field(op, "session")?;
    let session = session_table_mut(doc, &session_id).ok_or(format!("没有会话 {session_id}"))?;

    if op.get("remove").and_then(serde_json::Value::as_bool).unwrap_or(false) {
        let room = string_field(op, "room")?;
        let (connection, path) = split_room(&room, op)?;
        let subs = session
            .get_mut("subscriptions")
            .and_then(Item::as_array_of_tables_mut)
            .ok_or(format!("会话 {session_id} 没有任何订阅"))?;
        let index = subs
            .iter()
            .position(|t| room_matches(t, &connection, &path))
            .ok_or(format!("会话 {session_id} 没有 {connection}:{path} 的订阅"))?;
        subs.remove(index);
        changes.push(format!("会话 {session_id}：删掉订阅 {connection}:{path}"));
        return Ok(());
    }

    let (connection, path) = split_room(&string_field(op, "room")?, op)?;
    let subs = session
        .get_mut("subscriptions")
        .and_then(Item::as_array_of_tables_mut)
        .ok_or(format!("会话 {session_id} 没有 [[sessions.subscriptions]] 段，先用 TOML 编辑器建一条"))?;

    if let Some(existing) = subs.iter_mut().find(|t| room_matches(t, &connection, &path)) {
        if let Some(wake) = op.get("wake").and_then(serde_json::Value::as_object) {
            set_wake(existing, wake, &format!("{session_id}/{connection}:{path}"), changes)?;
        } else {
            changes.push(format!("会话 {session_id}：订阅 {connection}:{path} 无改动"));
        }
        return Ok(());
    }

    // 新建一条订阅。
    let mut sub = Table::new();
    sub.insert("connection", value(connection.clone()));
    sub.insert("room", value(path.clone()));
    if let Some(wake) = op.get("wake").and_then(serde_json::Value::as_object) {
        let mut wake_table = Table::new();
        fill_wake(&mut wake_table, wake)?;
        sub.insert("wake", Item::Table(wake_table));
    }
    subs.push(sub);
    changes.push(format!("会话 {session_id}：新增订阅 {connection}:{path}"));
    Ok(())
}

/// 人格卡新增或更新：`{"op":"persona","id":"p","name":"..","text":".."}`。
fn persona_op(
    doc: &mut DocumentMut,
    op: &serde_json::Value,
    changes: &mut Vec<String>,
) -> Result<(), String> {
    upsert_named_table(doc, "personas", op, &["name", "text"], changes)
}

/// 记忆仓库声明新增或更新：`{"op":"memory","id":"m","name":"..","kind":"git","location":".."}`。
fn memory_op(
    doc: &mut DocumentMut,
    op: &serde_json::Value,
    changes: &mut Vec<String>,
) -> Result<(), String> {
    upsert_named_table(doc, "memories", op, &["name", "kind", "location"], changes)
}

/// 连接修改：`{"op":"connection","id":"qq","patch":{"url":"..","token":"..","account":".."}}`。
/// 改这些**要重启桥**（守护线程攥着旧连接信息），清单会在保存结果里标出来。
fn connection_op(
    doc: &mut DocumentMut,
    op: &serde_json::Value,
    changes: &mut Vec<String>,
) -> Result<(), String> {
    let id = string_field(op, "id")?;
    let patch = op
        .get("patch")
        .and_then(serde_json::Value::as_object)
        .ok_or("要带 patch 对象")?;
    let table = doc
        .get_mut("connections")
        .and_then(Item::as_array_of_tables_mut)
        .and_then(|arr| {
            arr.iter_mut()
                .find(|t| t.get("id").and_then(Item::as_str) == Some(id.as_str()))
        })
        .ok_or(format!("没有连接 {id}"))?;

    for (key, val) in patch {
        match key.as_str() {
            "url" | "account" => {
                let text = val.as_str().ok_or(format!("{key} 要是字符串"))?;
                table.insert(key.as_str(), value(text.to_owned()));
                changes.push(format!("连接 {id}：{key} 更新（要重启）"));
            }
            "token" => match val {
                serde_json::Value::Null => {
                    table.remove("token");
                    changes.push(format!("连接 {id}：令牌清掉（要重启）"));
                }
                serde_json::Value::String(text) => {
                    table.insert("token", value(text.clone()));
                    changes.push(format!("连接 {id}：令牌更新（要重启）"));
                }
                _ => return Err("token 要是字符串或 null".into()),
            },
            other => return Err(format!("连接 patch 不认识字段 {other}（能改：url / token / account）")),
        }
    }
    Ok(())
}

/// 全局标量：`{"op":"global","patch":{"delivery_limit":50,"max_records_per_room":1000,
/// "max_age_days":30,"default_on_turn_error":["log"]}}`。null = 删键回默认。
fn global_op(
    doc: &mut DocumentMut,
    op: &serde_json::Value,
    changes: &mut Vec<String>,
) -> Result<(), String> {
    let patch = op
        .get("patch")
        .and_then(serde_json::Value::as_object)
        .ok_or("要带 patch 对象")?;

    for (key, val) in patch {
        match key.as_str() {
            "delivery_limit" => {
                set_or_remove_int(doc, "delivery_limit", val, "单轮投递上限", changes)?;
            }
            "max_records_per_room" | "max_age_days" => {
                if doc.get("retention").is_none() {
                    doc.insert("retention", Item::Table(Table::new()));
                }
                let table = doc
                    .get_mut("retention")
                    .and_then(Item::as_table_mut)
                    .ok_or("[retention] 不是表，先用 TOML 编辑器修")?;
                match val {
                    serde_json::Value::Null => {
                        table.remove(key.as_str());
                        changes.push(format!("{key}：删掉（回默认）"));
                    }
                    serde_json::Value::Number(number) => {
                        let n = number.as_u64().ok_or(format!("{key} 要是非负整数"))?;
                        table.insert(key.as_str(), value(n as i64));
                        changes.push(format!("{key} → {n}"));
                    }
                    _ => return Err(format!("{key} 要是数字或 null")),
                }
            }
            "default_on_turn_error" => {
                let list = match val {
                    serde_json::Value::Null => Vec::new(),
                    other => str_list(other).map_err(|_| "default_on_turn_error 要是字符串数组".to_owned())?,
                };
                doc.insert("default_on_turn_error", str_array(&list));
                changes.push(format!("默认失败通知 → [{}]", list.join(", ")));
            }
            other => return Err(format!("global patch 不认识字段 {other}（能改：delivery_limit / max_records_per_room / max_age_days / default_on_turn_error）")),
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 通用小件
// ---------------------------------------------------------------------------

/// 按会话 id 找 `[[sessions]]` 里的那张表。
fn session_table_mut<'a>(doc: &'a mut DocumentMut, id: &str) -> Option<&'a mut Table> {
    doc.get_mut("sessions")
        .and_then(Item::as_array_of_tables_mut)?
        .iter_mut()
        .find(|t| t.get("id").and_then(Item::as_str) == Some(id))
}

/// `room` 可以写 `qq:group/123`（从页面显示串直接来）或裸路径 `group/123`
/// （那就要另带 connection 字段）。返回（连接名, 路径）。
fn split_room(room: &str, op: &serde_json::Value) -> Result<(String, String), String> {
    if let Some((connection, path)) = room.split_once(':') {
        return Ok((connection.to_owned(), path.to_owned()));
    }
    let connection = string_field(op, "connection")
        .map_err(|_| format!("room 写的是裸路径 {room}，要么带上连接前缀（{room} 前面加 连接名:），要么另给 connection 字段"))?;
    Ok((connection, room.to_owned()))
}

/// 裸表是不是指向（connection, path）这条订阅。
fn room_matches(table: &Table, connection: &str, path: &str) -> bool {
    table.get("connection").and_then(Item::as_str) == Some(connection)
        && table.get("room").and_then(Item::as_str) == Some(path)
}

/// 在已存在的订阅表上改 wake：只动出现的键，null = 删键回默认。
fn set_wake(
    sub: &mut Table,
    wake: &serde_json::Map<String, serde_json::Value>,
    label: &str,
    changes: &mut Vec<String>,
) -> Result<(), String> {
    if sub.get_mut("wake").and_then(Item::as_table_mut).is_none() {
        sub.insert("wake", Item::Table(Table::new()));
    }
    let wake_table = sub
        .get_mut("wake")
        .and_then(Item::as_table_mut)
        .ok_or("wake 不是表")?;
    fill_wake(wake_table, wake)?;
    changes.push(format!("订阅 {label}：唤醒策略更新"));
    Ok(())
}

/// 往 wake 表里填字段（新建和修改共用）。
fn fill_wake(
    wake_table: &mut Table,
    wake: &serde_json::Map<String, serde_json::Value>,
) -> Result<(), String> {
    for (key, val) in wake {
        match key.as_str() {
            "at_mention" | "reply" | "name_mention" | "poke" => {
                let flag = val.as_bool().ok_or(format!("wake.{key} 要是布尔"))?;
                wake_table.insert(key.as_str(), value(flag));
            }
            "accumulate" => match val {
                serde_json::Value::Null => {
                    wake_table.remove("accumulate");
                }
                serde_json::Value::Number(number) => {
                    let n = number.as_u64().ok_or("wake.accumulate 要是非负整数")?;
                    wake_table.insert("accumulate", value(n as i64));
                }
                _ => return Err("wake.accumulate 要是数字或 null".into()),
            },
            "names" | "events" | "keywords" | "ignore_speakers" => match val {
                serde_json::Value::Null => {
                    wake_table.remove(key.as_str());
                }
                other => {
                    let list = str_list(other).map_err(|_| format!("wake.{key} 要是字符串数组或 null"))?;
                    wake_table.insert(key.as_str(), str_array(&list));
                }
            },
            other => return Err(format!("wake 不认识字段 {other}")),
        }
    }
    Ok(())
}

/// 人格/记忆这类「按 id 找数组条目，找不到就新建」的通用逻辑。
fn upsert_named_table(
    doc: &mut DocumentMut,
    array_key: &str,
    op: &serde_json::Value,
    fields: &[&str],
    changes: &mut Vec<String>,
) -> Result<(), String> {
    let id = string_field(op, "id")?;
    let array = doc
        .get_mut(array_key)
        .and_then(Item::as_array_of_tables_mut)
        .ok_or(format!("配置里没有 [[{array_key}]] 段"))?;

    let existing = array
        .iter_mut()
        .find(|t| t.get("id").and_then(Item::as_str) == Some(id.as_str()));
    let label = format!("{array_key}/{id}");
    if let Some(table) = existing {
        for field in fields {
            if let Some(text) = op.get(*field).and_then(serde_json::Value::as_str) {
                table.insert(*field, value(text.to_owned()));
                changes.push(format!("{label}：{field} 更新"));
            }
        }
        return Ok(());
    }

    let mut table = Table::new();
    table.insert("id", value(id.clone()));
    for field in fields {
        let text = op
            .get(*field)
            .and_then(serde_json::Value::as_str)
            .ok_or(format!("新建 {label} 缺字段 {field}"))?;
        table.insert(*field, value(text.to_owned()));
    }
    array.push(table);
    changes.push(format!("{label}：新增"));
    Ok(())
}

fn string_field(op: &serde_json::Value, key: &str) -> Result<String, String> {
    op.get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .ok_or(format!("缺字段 {key}（要是字符串）"))
}

fn str_list(val: &serde_json::Value) -> Result<Vec<String>, ()> {
    val.as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_owned))
                .collect()
        })
        .ok_or(())
}

fn str_array(values: &[String]) -> Item {
    let mut array = toml_edit::Array::new();
    for item in values {
        array.push(item.clone());
    }
    value(array)
}

/// 顶层整数键：数字 → 设值；null → 删键。
fn set_or_remove_int(
    doc: &mut DocumentMut,
    key: &str,
    val: &serde_json::Value,
    label: &str,
    changes: &mut Vec<String>,
) -> Result<(), String> {
    match val {
        serde_json::Value::Null => {
            doc.remove(key);
            changes.push(format!("{label}：删掉（回默认）"));
            Ok(())
        }
        serde_json::Value::Number(number) => {
            let n = number
                .as_u64()
                .ok_or(format!("{label} 要是非负整数"))?;
            doc.insert(key, value(n as i64));
            changes.push(format!("{label} → {n}"));
            Ok(())
        }
        _ => Err(format!("{label} 要是数字或 null")),
    }
}

#[cfg(test)]
mod tests {
    use super::apply_patch;

    fn base() -> String {
        r#"
# 用户手写的注释：机器人主会话
[[personas]]
id = "p"
name = "人格"
text = "你是桥。"

[[harnesses]]
id = "dsh"
name = "DSH"

[[connections]]
id = "qq"
platform = "qq"
kind = "onebot11"
url = "ws://127.0.0.1:3001"
account = "bot"
token = "conn-secret"

[[sessions]]
id = "main"
harness = "dsh"
persona = "p"
token = "t-main"

[[sessions.subscriptions]]
connection = "qq"
room = "group/123"

[sessions.subscriptions.wake]
at_mention = true
keywords = ["喂"]
"#
        .to_owned()
    }

    fn parse_ops(value: serde_json::Value) -> Vec<serde_json::Value> {
        value.as_array().expect("测试里都是数组").to_vec()
    }

    #[test]
    fn wake_patch_keeps_comments_and_only_touched_keys() {
        let text = base();
        let ops = parse_ops(serde_json::json!([
            {"op": "subscription", "session": "main", "room": "qq:group/123",
             "wake": {"keywords": ["喂", "在吗"], "accumulate": 10}}
        ]));
        let (new_text, changes) = super::apply_patch(&text, &ops).expect("补丁能打");
        // 手写注释原样保留。
        assert!(new_text.contains("# 用户手写的注释：机器人主会话"));
        // 没碰过的键原样保留。
        assert!(new_text.contains("at_mention = true"));
        // 改过的键生效，且整份还能被 Config::parse 接受。
        assert!(new_text.contains("accumulate = 10"));
        let config = acb_kernel::Config::parse(&new_text).expect("补丁后配置合法");
        let sub = &config.session("main").expect("会话在").subscriptions[0];
        assert_eq!(sub.wake.keywords, vec!["喂".to_owned(), "在吗".to_owned()]);
        assert_eq!(sub.wake.accumulate, Some(10));
        assert!(!changes.is_empty());
    }

    #[test]
    fn add_and_remove_subscription() {
        let text = base();
        let ops = parse_ops(serde_json::json!([
            {"op": "subscription", "session": "main", "connection": "qq", "room": "group/456"}
        ]));
        let (new_text, _) = super::apply_patch(&text, &ops).expect("能加");
        let config = acb_kernel::Config::parse(&new_text).expect("合法");
        assert_eq!(config.session("main").expect("在").subscriptions.len(), 2);

        let ops = parse_ops(serde_json::json!([
            {"op": "subscription", "session": "main", "room": "qq:group/123", "remove": true}
        ]));
        let (new_text, changes) = super::apply_patch(&new_text, &ops).expect("能删");
        assert!(changes[0].contains("删掉订阅"));
        let config = acb_kernel::Config::parse(&new_text).expect("合法");
        let subs = &config.session("main").expect("在").subscriptions;
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].room.to_string(), "qq:group/456");
    }

    #[test]
    fn session_model_null_clears_key() {
        let text = base().replace("persona = \"p\"", "persona = \"p\"\nmodel = \"gpt-x\"");
        let ops = parse_ops(serde_json::json!([
            {"op": "session", "id": "main", "patch": {"model": null, "name": "主力"}}
        ]));
        let (new_text, _) = super::apply_patch(&text, &ops).expect("能打");
        assert!(!new_text.contains("gpt-x"), "model 键被删掉");
        assert!(new_text.contains("name = \"主力\""));
        let config = acb_kernel::Config::parse(&new_text).expect("合法");
        assert_eq!(config.session("main").expect("在").model, None);
    }

    #[test]
    fn persona_upsert_creates_when_missing() {
        let text = base();
        let ops = parse_ops(serde_json::json!([
            {"op": "persona", "id": "p2", "name": "二号", "text": "你是二号桥。"}
        ]));
        let (new_text, changes) = super::apply_patch(&text, &ops).expect("能加");
        assert!(changes[0].contains("新增"));
        let config = acb_kernel::Config::parse(&new_text).expect("合法");
        assert_eq!(config.personas.len(), 2);
        assert_eq!(config.personas[1].name, "二号");
    }

    #[test]
    fn global_scalars_and_errors() {
        let text = base();
        let ops = parse_ops(serde_json::json!([
            {"op": "global", "patch": {"delivery_limit": 50, "max_age_days": 7}}
        ]));
        let (new_text, changes) = super::apply_patch(&text, &ops).expect("能打");
        assert!(new_text.contains("delivery_limit = 50"));
        assert!(new_text.contains("max_age_days = 7"));
        assert_eq!(changes.len(), 2);
        let config = acb_kernel::Config::parse(&new_text).expect("合法");
        assert_eq!(config.delivery_limit, 50);
        assert_eq!(config.retention.max_age_days, 7);

        // 不认识的 op 整体报错。
        let ops = parse_ops(serde_json::json!([
            {"op": "global", "patch": {"delivery_limit": 1}},
            {"op": "nonsense"}
        ]));
        let error = super::apply_patch(&text, &ops).expect_err("该报错");
        assert!(error.contains("第 2 条"), "错误要指明第几条：{error}");

        // 不存在的会话报错。
        let ops = parse_ops(serde_json::json!([
            {"op": "session", "id": "ghost", "patch": {"name": "x"}}
        ]));
        let error = super::apply_patch(&text, &ops).expect_err("该报错");
        assert!(error.contains("ghost"));
    }

    #[test]
    fn connection_patch_updates_url_and_token() {
        let text = base();
        let ops = parse_ops(serde_json::json!([
            {"op": "connection", "id": "qq", "patch": {"url": "ws://127.0.0.1:3002", "token": null}}
        ]));
        let (new_text, changes) = super::apply_patch(&text, &ops).expect("能打");
        assert!(new_text.contains("ws://127.0.0.1:3002"));
        assert!(!new_text.contains("conn-secret"), "token 键被删");
        assert!(changes.iter().all(|c| c.contains("要重启")));
        let config = acb_kernel::Config::parse(&new_text).expect("合法");
        assert_eq!(config.connections[0].token, None);
    }
}
