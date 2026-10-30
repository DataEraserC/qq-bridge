//! 查询：把「拉全表、客户端自己过滤」这件事收进桥里（ADR-0023）。
//!
//! 起因很具体：agent 想问「这个群有几个管理员」，能做的只有把成员列表整份拉回来、
//! 自己数。列表一大就分页、就烧上下文，而且「几个管理员」这种问题本来不该需要全表。
//!
//! 这一层刻意**不认识任何平台**：它作用在「一堆 JSON 对象」上，字段名靠**看数据**得出来
//! （把头几条的键并起来当白名单）。所以 `group.get_member_list` 也好、`friend.list` 也好、
//! 以后 Discord 的成员列表也好，全都直接可用，不用为每个方法写一遍。
//!
//! 字段白名单是「从数据推出来的」而不是「写死的」：写死的表在平台加字段那天就过期了，
//! 而推出来的表永远和这次拿到的数据一致。传错字段照样报 `bad_params`，并把真实可用的
//! 字段列出来——这是 agent 一轮就能改对的关键。

use serde::Deserialize;
use serde_json::{Map, Value};

/// 一次查询。四个字段都可以不给；都不给就是这个数组原样。
#[derive(Debug, Default, Clone, Deserialize)]
pub struct Query {
    /// 过滤条件，全部满足才留下（AND）。写成 `"where": [{"field": "...", "op": "eq", "value": ...}]`。
    #[serde(default, rename = "where")]
    pub filter: Vec<Filter>,
    /// 只要这些字段。字段名可以是 `sender.id` 这样的路径。
    #[serde(default)]
    pub fields: Option<Vec<String>>,
    /// 排序：`"at desc"` / `"user_id"`（默认升序）。
    #[serde(default)]
    pub order_by: Option<String>,
    /// 按这个字段分组计数。给了它，返回的就是分组结果，不是原数组。
    #[serde(default)]
    pub group_by: Option<String>,
}

/// 一条过滤条件。
#[derive(Debug, Clone, Deserialize)]
pub struct Filter {
    /// 字段名，支持 `sender.id` 这种路径。
    pub field: String,
    /// 比较方式，默认 `eq`。
    #[serde(default)]
    pub op: Option<String>,
    /// 比较值。`in` 要数组，`not_null` 不要值，其余按标量比。
    #[serde(default)]
    pub value: Value,
}

/// 认识的比较方式。故意只有八个：够表达「查一下」，再多就是让人记语法。
pub const OPS: &[&str] = &[
    "eq", "ne", "in", "gt", "gte", "lt", "lte", "contains", "not_null",
];

/// 查询出错。两种，都带「合法的有哪些」——否则 agent 只能猜。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryError {
    /// 字段不存在。
    UnknownField { field: String, available: Vec<String> },
    /// 比较方式不认识。
    UnknownOp { op: String },
    /// `in` 配了非数组、`not_null` 配了值之类。
    BadValue { field: String, detail: String },
    /// `order_by` 写成了三个词。
    BadOrder { text: String },
}

impl std::fmt::Display for QueryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownField { field, available } => write!(
                formatter,
                "[bad_field] 没有字段 {field}；这批数据里有的字段：{}",
                available.join(" / ")
            ),
            Self::UnknownOp { op } => write!(
                formatter,
                "[bad_params] 不认识的比较方式 {op}；能用的：{}",
                OPS.join(" / ")
            ),
            Self::BadValue { field, detail } => {
                write!(formatter, "[bad_params] 字段 {field} 的值不对：{detail}")
            }
            Self::BadOrder { text } => write!(
                formatter,
                "[bad_params] order_by 要写成「字段」或「字段 desc/asc」，收到「{text}」"
            ),
        }
    }
}

/// 查询结果：要么是一批条目，要么是分组计数。
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// 过滤/排序/投影之后的条目。
    Items { items: Vec<Value>, matched: usize },
    /// 分组计数。`key` 是分组字段的取值（字符串化），按条数从多到少排。
    Groups { groups: Vec<(String, usize)>, matched: usize },
}

/// 作用在一个对象数组上。
pub fn apply(items: &[Value], query: &Query) -> Result<Outcome, QueryError> {
    let available = field_names(items);
    for filter in &query.filter {
        let op = filter.op.as_deref().unwrap_or("eq");
        if !OPS.contains(&op) {
            return Err(QueryError::UnknownOp { op: op.to_owned() });
        }
        if op == "in" && !filter.value.is_array() {
            return Err(QueryError::BadValue {
                field: filter.field.clone(),
                detail: "op=in 时 value 要给数组，如 [\"admin\", \"owner\"]".to_owned(),
            });
        }
        check_field(&filter.field, &available, items)?;
    }
    if let Some(order) = &query.order_by {
        let field = order_field(order)?;
        check_field(field, &available, items)?;
    }
    if let Some(group) = &query.group_by {
        check_field(group, &available, items)?;
    }
    if let Some(fields) = &query.fields {
        for field in fields {
            check_field(field, &available, items)?;
        }
    }

    let mut kept: Vec<Value> = items
        .iter()
        .filter(|item| query.filter.iter().all(|filter| matches(item, filter)))
        .cloned()
        .collect();
    let matched = kept.len();

    if let Some(group) = &query.group_by {
        let mut counts: Vec<(String, usize)> = Vec::new();
        for item in &kept {
            let key = text_of(item, group);
            match counts.iter_mut().find(|(known, _)| *known == key) {
                Some((_, count)) => *count += 1,
                None => counts.push((key, 1)),
            }
        }
        // 条数多的在前；条数相同按名字排，保证同样的输入给同样的输出。
        counts.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        return Ok(Outcome::Groups {
            groups: counts,
            matched,
        });
    }

    if let Some(order) = &query.order_by {
        let (field, descending) = order_spec(order)?;
        kept.sort_by(|a, b| {
            let order = compare(&pick(a, field), &pick(b, field));
            if descending {
                order.reverse()
            } else {
                order
            }
        });
    }

    if let Some(fields) = &query.fields {
        kept = kept
            .iter()
            .map(|item| project(item, fields))
            .collect::<Vec<_>>();
    }

    Ok(Outcome::Items { items: kept, matched })
}

/// 结果里所有出现过的顶层字段，按首次出现的顺序去重。给人看的白名单就是它。
fn field_names(items: &[Value]) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for item in items {
        let Some(map) = item.as_object() else {
            continue;
        };
        for key in map.keys() {
            if !names.contains(key) {
                names.push(key.clone());
            }
        }
    }
    names
}

/// 字段存在吗。路径写法（`sender.id`）按第一段判：第一段在，就算认识——
/// 深层字段是否真的有值，交给「取不到就当空」处理，不必为它写一套 schema 递归。
fn check_field(field: &str, available: &[String], items: &[Value]) -> Result<(), QueryError> {
    let head = field.split('.').next().unwrap_or(field);
    if available.iter().any(|known| known == head) {
        return Ok(());
    }
    // 数组里没有对象（比如是纯字符串列表）时，只允许 `value` 这一个虚拟字段。
    if items.iter().all(|item| !item.is_object()) && head == "value" {
        return Ok(());
    }
    Err(QueryError::UnknownField {
        field: field.to_owned(),
        available: available.to_vec(),
    })
}

/// 取一个字段的值，支持 `a.b.c` 路径。取不到给 `Null`。
fn pick(item: &Value, field: &str) -> Value {
    let mut current = item;
    for part in field.split('.') {
        match current {
            Value::Object(map) => match map.get(part) {
                Some(next) => current = next,
                None => return Value::Null,
            },
            Value::Array(items) => {
                // 数组下标也认：`members.0.name`。
                let Some(index) = part.parse::<usize>().ok() else {
                    return Value::Null;
                };
                match items.get(index) {
                    Some(next) => current = next,
                    None => return Value::Null,
                }
            }
            _ => return Value::Null,
        }
    }
    current.clone()
}

/// 取一个字段的文本形式，分组用。
fn text_of(item: &Value, field: &str) -> String {
    match pick(item, field) {
        Value::Null => "null".to_owned(),
        Value::String(text) => text,
        other => other.to_string(),
    }
}

fn matches(item: &Value, filter: &Filter) -> bool {
    // 纯值数组：字段名 `value` 就是指元素本身。
    let actual = if filter.field == "value" && !item.is_object() {
        item.clone()
    } else {
        pick(item, &filter.field)
    };
    let op = filter.op.as_deref().unwrap_or("eq");
    match op {
        "not_null" => !actual.is_null(),
        "eq" => equal(&actual, &filter.value),
        "ne" => !equal(&actual, &filter.value),
        "in" => match filter.value.as_array() {
            Some(values) => values.iter().any(|value| equal(&actual, value)),
            None => false,
        },
        "contains" => contains(&actual, &filter.value),
        "gt" => compare(&actual, &filter.value) == std::cmp::Ordering::Greater,
        "gte" => compare(&actual, &filter.value) != std::cmp::Ordering::Less,
        "lt" => compare(&actual, &filter.value) == std::cmp::Ordering::Less,
        "lte" => compare(&actual, &filter.value) != std::cmp::Ordering::Greater,
        _ => false,
    }
}

/// 相等：数字按数值比（`1` 和 `1.0` 相等），其余按结构比。
fn equal(left: &Value, right: &Value) -> bool {
    match (number(left), number(right)) {
        (Some(left), Some(right)) => (left - right).abs() < f64::EPSILON,
        _ => left == right,
    }
}

fn contains(actual: &Value, needle: &Value) -> bool {
    match (actual, needle) {
        (Value::String(text), Value::String(needle)) => text.contains(needle.as_str()),
        (Value::Array(items), _) => items.iter().any(|item| equal(item, needle)),
        // 对象里任何一个字符串字段含这个词也算：模型不用知道内容是嵌在哪一层。
        (Value::Object(map), Value::String(needle)) => map
            .values()
            .any(|value| value.as_str().is_some_and(|text| text.contains(needle.as_str()))),
        _ => false,
    }
}

/// 排序用的大小比较。数字按数值、字符串按字典序、布尔按假<真、空值永远排最后。
fn compare(left: &Value, right: &Value) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (left, right) {
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Null, _) => Ordering::Greater,
        (_, Value::Null) => Ordering::Less,
        _ => match (number(left), number(right)) {
            (Some(left), Some(right)) => left.partial_cmp(&right).unwrap_or(Ordering::Equal),
            _ => text_of(left, "").cmp(&text_of(right, "")),
        },
    }
}

fn number(value: &Value) -> Option<f64> {
    value.as_f64()
}

/// `"at desc"` → (`"at"`, true)。
fn order_spec(text: &str) -> Result<(&str, bool), QueryError> {
    let parts: Vec<&str> = text.split_whitespace().collect();
    match parts.as_slice() {
        [field] => Ok((field, false)),
        [field, direction] if direction.eq_ignore_ascii_case("desc") => Ok((field, true)),
        [field, direction] if direction.eq_ignore_ascii_case("asc") => Ok((field, false)),
        _ => Err(QueryError::BadOrder {
            text: text.to_owned(),
        }),
    }
}

fn order_field(text: &str) -> Result<&str, QueryError> {
    order_spec(text).map(|(field, _)| field)
}

/// 只留这些字段。路径字段会还原成嵌套结构（`sender.id` → `{"sender": {"id": ...}}`）。
fn project(item: &Value, fields: &[String]) -> Value {
    let mut out = Map::new();
    for field in fields {
        let value = pick(item, field);
        let mut parts = field.split('.').rev();
        let leaf = parts.next().unwrap_or(field);
        let mut wrapped = Value::Null;
        let mut first = true;
        for part in std::iter::once(leaf).chain(parts) {
            if first {
                wrapped = value.clone();
                first = false;
            }
            let mut map = Map::new();
            map.insert(part.to_owned(), wrapped);
            wrapped = Value::Object(map);
        }
        merge(&mut out, wrapped);
    }
    Value::Object(out)
}

/// 把嵌套结构并进目标对象（同名对象递归合并，标量后者覆盖）。
fn merge(into: &mut Map<String, Value>, value: Value) {
    let Value::Object(map) = value else {
        return;
    };
    for (key, value) in map {
        match (into.get_mut(&key), value) {
            (Some(Value::Object(existing)), Value::Object(incoming)) => {
                let mut merged = std::mem::take(existing);
                merge(&mut merged, Value::Object(incoming));
                into.insert(key, Value::Object(merged));
            }
            (_, value) => {
                into.insert(key, value);
            }
        }
    }
}

/// 作用在一个「结果对象」上：找到里面最长的数组，查它，再放回去。
///
/// 为什么认「最长数组」而不是写死 `data`：平台结果形状千奇百怪，
/// 写死的 key 在下一个平台上就失效；认最长数组最多是挑错数组，不会丢数据。
pub fn apply_to_value(value: Value, query: &Query) -> Result<Value, QueryError> {
    let Some((slot, _)) = crate::render::array_slot(&value) else {
        return Ok(value);
    };
    let items: Vec<Value> = match (&slot, &value) {
        (None, Value::Array(items)) => items.clone(),
        (Some(key), Value::Object(map)) => match map.get(key).and_then(Value::as_array) {
            Some(items) => items.clone(),
            None => return Ok(value),
        },
        _ => return Ok(value),
    };

    let outcome = apply(&items, query)?;
    Ok(match (outcome, slot) {
        (Outcome::Groups { groups, matched }, _) => {
            let groups: Vec<Value> = groups
                .into_iter()
                .map(|(key, count)| serde_json::json!({ "key": key, "count": count }))
                .collect();
            serde_json::json!({
                "matched": matched,
                "group_by": query.group_by,
                "groups": groups,
                "note": "这是分组计数，不是明细。要明细就别给 group_by。",
            })
        }
        (Outcome::Items { items, matched }, None) => {
            serde_json::json!({ "matched": matched, "items": items })
        }
        (Outcome::Items { items, matched }, Some(key)) => {
            let mut object = value;
            if let Value::Object(map) = &mut object {
                map.insert(key, Value::Array(items));
                map.insert("matched".to_owned(), serde_json::json!(matched));
            }
            object
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn members() -> Vec<Value> {
        vec![
            json!({"user_id": 1, "nickname": "老王", "role": "admin"}),
            json!({"user_id": 2, "nickname": "小李", "role": "member"}),
            json!({"user_id": 3, "nickname": "老张", "role": "admin"}),
            json!({"user_id": 4, "nickname": "小王", "role": "member"}),
        ]
    }

    fn query(value: Value) -> Query {
        serde_json::from_value(value).expect("测试里的查询都合法")
    }

    #[test]
    fn filters_by_equality_and_counts_groups() {
        let q = query(json!({
            "where": [{"field": "role", "op": "eq", "value": "admin"}],
            "group_by": "role",
        }));
        match apply(&members(), &q).expect("能查") {
            Outcome::Groups { groups, matched } => {
                assert_eq!(matched, 2);
                assert_eq!(groups, vec![("admin".to_owned(), 2)]);
            }
            other => panic!("应当分组，实际 {other:?}"),
        }
    }

    #[test]
    fn in_and_contains_and_comparisons() {
        let q = query(json!({"where": [{"field": "user_id", "op": "gte", "value": 3}]}));
        match apply(&members(), &q).expect("能查") {
            Outcome::Items { items, .. } => assert_eq!(items.len(), 2),
            other => panic!("应当返回条目，实际 {other:?}"),
        }

        let q = query(json!({"where": [{"field": "nickname", "op": "contains", "value": "老"}]}));
        match apply(&members(), &q).expect("能查") {
            Outcome::Items { items, .. } => assert_eq!(items.len(), 2),
            other => panic!("应当返回条目，实际 {other:?}"),
        }

        let q = query(json!({"where": [{"field": "role", "op": "in", "value": ["admin", "owner"]}]}));
        match apply(&members(), &q).expect("能查") {
            Outcome::Items { items, .. } => assert_eq!(items.len(), 2),
            other => panic!("应当返回条目，实际 {other:?}"),
        }
    }

    #[test]
    fn orders_and_projects() {
        let q = query(json!({"order_by": "user_id desc", "fields": ["user_id"]}));
        match apply(&members(), &q).expect("能查") {
            Outcome::Items { items, .. } => {
                assert_eq!(items[0], json!({"user_id": 4}));
                assert_eq!(items[3], json!({"user_id": 1}));
                assert!(items[0].get("nickname").is_none());
            }
            other => panic!("应当返回条目，实际 {other:?}"),
        }
    }

    #[test]
    fn nested_paths_work_for_filter_and_projection() {
        let items = vec![
            json!({"id": 1, "sender": {"id": "10001", "name": "老王"}}),
            json!({"id": 2, "sender": {"id": "10002", "name": "小李"}}),
        ];
        let q = query(json!({
            "where": [{"field": "sender.id", "op": "eq", "value": "10002"}],
            "fields": ["id", "sender.name"],
        }));
        match apply(&items, &q).expect("能查") {
            Outcome::Items { items, .. } => {
                assert_eq!(items.len(), 1);
                assert_eq!(items[0], json!({"id": 2, "sender": {"name": "小李"}}));
            }
            other => panic!("应当返回条目，实际 {other:?}"),
        }
    }

    #[test]
    fn unknown_field_lists_what_is_available() {
        let q = query(json!({"where": [{"field": "rol", "op": "eq", "value": "admin"}]}));
        let error = apply(&members(), &q).expect_err("字段写错就该报错");
        let text = error.to_string();
        assert!(text.contains("[bad_field]"));
        assert!(text.contains("role"), "要把真实字段列出来：{text}");
        assert!(text.contains("nickname"));
    }

    #[test]
    fn plain_value_arrays_can_be_queried_as_value() {
        let items = vec![json!("apple"), json!("banana"), json!("apple pie")];
        let q = query(json!({"where": [{"field": "value", "op": "contains", "value": "apple"}]}));
        match apply(&items, &q).expect("能查") {
            Outcome::Items { items, .. } => assert_eq!(items.len(), 2),
            other => panic!("应当返回条目，实际 {other:?}"),
        }
    }

    #[test]
    fn apply_to_value_keeps_the_original_shape() {
        let value = json!({"retcode": 0, "data": members()});
        let q = query(json!({"where": [{"field": "role", "op": "eq", "value": "admin"}]}));
        let out = apply_to_value(value, &q).expect("能查");
        assert_eq!(out["retcode"], 0, "外壳字段不能丢");
        assert_eq!(out["matched"], 2);
        assert_eq!(out["data"].as_array().expect("还是数组").len(), 2);
    }

    #[test]
    fn bad_order_and_bad_op_are_reported() {
        let q = query(json!({"order_by": "user_id descending fast"}));
        assert!(matches!(
            apply(&members(), &q),
            Err(QueryError::BadOrder { .. })
        ));

        let q = query(json!({"where": [{"field": "role", "op": "like", "value": "x"}]}));
        let error = apply(&members(), &q).expect_err("不认识的比较方式要报错");
        assert!(matches!(error, QueryError::UnknownOp { .. }));
        assert!(error.to_string().contains("eq"), "要把能用的列出来");

        let q = query(json!({"where": [{"field": "role", "op": "in", "value": "admin"}]}));
        assert!(matches!(
            apply(&members(), &q),
            Err(QueryError::BadValue { .. })
        ));
    }
}
