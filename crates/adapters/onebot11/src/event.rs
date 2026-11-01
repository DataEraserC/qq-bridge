//! OneBot 11 事件 → 投递记录草稿。
//!
//! 消息、拍一拍、其它群事件走同一条路（ADR-0005）：都变成一条 [`RecordDraft`]，交给桥
//! 记流水、定未读、定叫醒。心跳这类「不是房间里发生的事」直接忽略，不进流水。
//!
//! 房间编号约定（第一段是**连接名**，不是平台名：同一个平台可以有两个号）：
//! - 群 → `<连接>:group/<群号>`
//! - 私聊 → `<连接>:private/<对方号>`

use acb_kernel::{Element, RecordDraft, RoomId, Sender, SenderRole};
use serde_json::Value;

use crate::segment;

/// 一条事件的处理结果。
#[derive(Debug, Clone, PartialEq)]
pub enum Parsed {
    /// 值得记进流水的一条。
    Record(RecordDraft),
    /// 不记，说明原因（只进日志）。
    Ignored(String),
}

/// 事件解析失败：格式不对或缺关键字段。缺字段说明对面不按 OneBot 11 说话，不能猜。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EventError {
    #[error("事件缺少 {0}")]
    Missing(&'static str),
    #[error("房间标识不合法：{0}")]
    BadRoom(String),
}

/// 解析一条 OneBot 11 上报事件。
///
/// `admin_qqs` 是这条连接点名的系统管理员账号。发送者命中就标 `SystemAdmin`；
/// 否则读事件自带的 `sender.role`（`owner`→群主、`admin`→群管理员、`member`→普通群友）。
/// 事件没带 `sender`（拍一拍这类 notice 常见）时，昵称和群角色都判不出，交给调用方
/// 用 `get_group_member_info` 兜底。
pub fn parse(connection: &str, event: &Value, admin_qqs: &[String]) -> Result<Parsed, EventError> {
    let self_id = id_of(event.get("self_id"));
    let at = event
        .get("time")
        .and_then(Value::as_i64)
        .map(|seconds| seconds * 1000)
        .unwrap_or_else(now_millis);

    match post_type(event) {
        Some("message") => message(connection, event, &self_id, at, admin_qqs),
        // 自己发出去的消息回执。桥在发送时已经记过流水了，再记一遍会重复。
        Some("message_sent") => Ok(Parsed::Ignored("自己消息的回执".into())),
        Some("notice") => notice(connection, event, &self_id, at, admin_qqs),
        // 心跳每几秒一次，进流水只会把记录撑爆。
        Some("meta_event") => Ok(Parsed::Ignored("心跳/生命周期".into())),
        Some("request") => request(connection, event, at, admin_qqs),
        other => Ok(Parsed::Ignored(format!(
            "不认识的事件：{}",
            other.unwrap_or("没有 post_type")
        ))),
    }
}

fn post_type(event: &Value) -> Option<&str> {
    event.get("post_type").and_then(Value::as_str)
}

fn message(
    connection: &str,
    event: &Value,
    self_id: &str,
    at: i64,
    admin_qqs: &[String],
) -> Result<Parsed, EventError> {
    let sender_id = id_of(event.get("user_id"));
    if sender_id.is_empty() {
        return Err(EventError::Missing("user_id"));
    }

    let room = match event.get("message_type").and_then(Value::as_str) {
        Some("group") => {
            let group = id_of(event.get("group_id"));
            if group.is_empty() {
                return Err(EventError::Missing("group_id"));
            }
            room(connection, &format!("group/{group}"))?
        }
        Some("private") => room(connection, &format!("private/{sender_id}"))?,
        _ => return Err(EventError::Missing("message_type")),
    };

    let (elements, at_self) =
        segment::from_onebot(event.get("message").unwrap_or(&Value::Null), self_id);

    Ok(Parsed::Record(
        RecordDraft::message(
            room,
            sender_of(event, &sender_id, admin_qqs),
            at,
            elements,
            at_self,
        )
        // 平台消息编号留着：别人引用了某条消息时，桥要靠它反查「被引用的是谁发的」。
        .with_platform_id(id_of(event.get("message_id"))),
    ))
}

fn notice(
    connection: &str,
    event: &Value,
    self_id: &str,
    at: i64,
    admin_qqs: &[String],
) -> Result<Parsed, EventError> {
    let notice_type = event
        .get("notice_type")
        .and_then(Value::as_str)
        .ok_or(EventError::Missing("notice_type"))?;
    let sender_id = {
        let user = id_of(event.get("user_id"));
        if user.is_empty() {
            id_of(event.get("target_id"))
        } else {
            user
        }
    };
    let room = notice_room(connection, event, &sender_id)?;
    let sender = sender_of(event, &sender_id, admin_qqs);

    // 拍一拍：被拍的是 target_id。拍到我，`at_self` 才为真（唤醒策略据此判定）。
    if notice_type == "notify" && event.get("sub_type").and_then(Value::as_str) == Some("poke") {
        let target = id_of(event.get("target_id"));
        let at_self = !self_id.is_empty() && target == self_id;
        let body = if at_self {
            "[拍了拍我]".to_owned()
        } else {
            format!("[拍了拍 {target}]")
        };
        return Ok(Parsed::Record(
            RecordDraft::event(
                room,
                sender,
                at,
                "poke",
                vec![Element::text(body.clone())],
                at_self,
            )
            .with_event_meta(
                vec!["poke".to_owned(), "notice".to_owned()],
                Some(event.clone()),
                Some(body),
            ),
        ));
    }

    // 其它通知（进退群、禁言、撤回……）也是房间里发生的事，记一条可读的。
    // 认得的事件给模板人话；不认得的（无解析实现）降级：文本只给类型名，
    // 原始事件 JSON 装进 Opaque 一并存流水。投递时渲染层不给载荷、只给流水
    // 编号，agent 想看细节用 fetch_records 按编号拉（ADR-0004 补充裁定）。
    let mut elements = Vec::new();
    let summary = match describe_notice(notice_type, event) {
        Some(body) => {
            elements.push(Element::text(body.clone()));
            Some(body)
        }
        None => {
            let text = format!("[{notice_type}]（未解析事件）");
            elements.push(Element::text(text.clone()));
            elements.push(Element::Opaque {
                platform: "qq".to_owned(),
                payload: event.clone(),
            });
            Some(text)
        }
    };
    Ok(Parsed::Record(
        RecordDraft::event(room, sender, at, notice_type, elements, false).with_event_meta(
            vec!["notice".to_owned(), notice_type.to_owned()],
            Some(event.clone()),
            summary,
        ),
    ))
}

fn request(
    connection: &str,
    event: &Value,
    at: i64,
    admin_qqs: &[String],
) -> Result<Parsed, EventError> {
    let request_type = event
        .get("request_type")
        .and_then(Value::as_str)
        .ok_or(EventError::Missing("request_type"))?;
    let sender_id = id_of(event.get("user_id"));
    let room = notice_room(connection, event, &sender_id)?;
    let body = format!("[请求 {request_type}] {}", comment_of(event));

    Ok(Parsed::Record(
        RecordDraft::event(
            room,
            sender_of(event, &sender_id, admin_qqs),
            at,
            format!("request_{request_type}"),
            vec![Element::text(body.clone())],
            false,
        )
        .with_event_meta(
            vec!["request".to_owned(), request_type.to_owned()],
            Some(event.clone()),
            Some(body),
        ),
    ))
}

/// 事件发生在哪个房间：有群号就是群，否则算私聊。
fn notice_room(connection: &str, event: &Value, who: &str) -> Result<RoomId, EventError> {
    let group = id_of(event.get("group_id"));
    if !group.is_empty() {
        return room(connection, &format!("group/{group}"));
    }
    if who.is_empty() {
        return Err(EventError::Missing("group_id / user_id"));
    }
    room(connection, &format!("private/{who}"))
}

fn room(connection: &str, part: &str) -> Result<RoomId, EventError> {
    RoomId::new(connection, part).map_err(|error| EventError::BadRoom(error.to_string()))
}

/// 显示名优先用群名片，其次昵称，都没有就用号码。
fn display_name(event: &Value) -> String {
    let sender = event.get("sender");
    for key in ["card", "nickname"] {
        if let Some(name) = sender
            .and_then(|value| value.get(key))
            .and_then(Value::as_str)
        {
            if !name.trim().is_empty() {
                return name.to_owned();
            }
        }
    }
    id_of(event.get("user_id"))
}

/// 构造发送者，顺手判身份：命中 admin_qqs 是系统管理员，否则看事件自带的 `sender.role`。
fn sender_of(event: &Value, sender_id: &str, admin_qqs: &[String]) -> Sender {
    let role = if admin_qqs.iter().any(|qq| qq == sender_id) {
        Some(SenderRole::SystemAdmin)
    } else {
        role_of(event)
    };
    Sender {
        id: sender_id.to_owned(),
        name: display_name(event),
        role,
    }
}

/// 这条 poke 事件缺发送者昵称吗？缺就返回 `(群号, 用户号)`，供调用方查
/// `get_group_member_info` 补昵称/群名片。查不到或本来就有名字就返回 `None`。
///
/// OneBot 的 notice/poke 经常只带 `user_id` / `target_id`，不带 `sender`；直接入库
/// 会把昵称显示成 QQ 号。
pub fn poke_missing_sender(event: &Value) -> Option<(String, String)> {
    let is_poke = event.get("post_type").and_then(Value::as_str) == Some("notice")
        && event.get("notice_type").and_then(Value::as_str) == Some("notify")
        && event.get("sub_type").and_then(Value::as_str) == Some("poke");
    if !is_poke {
        return None;
    }
    let has_name = event.get("sender").is_some_and(|sender| {
        ["card", "nickname"].iter().any(|key| {
            sender
                .get(*key)
                .and_then(Value::as_str)
                .is_some_and(|name| !name.trim().is_empty())
        })
    });
    if has_name {
        return None;
    }
    let group = id_of(event.get("group_id"));
    let user = id_of(event.get("user_id"));
    if group.is_empty() || user.is_empty() {
        return None;
    }
    Some((group, user))
}

/// 事件自带 `sender.role` → 身份。`owner` 群主、`admin` 群管理员、`member` 及空 = 普通群友。
fn role_of(event: &Value) -> Option<SenderRole> {
    match event
        .get("sender")
        .and_then(|sender| sender.get("role"))
        .and_then(Value::as_str)
    {
        Some("owner") => Some(SenderRole::Owner),
        Some("admin") => Some(SenderRole::Admin),
        _ => None,
    }
}

/// 认得的通知事件给一句人话；不认得的返回 `None`——调用方降级存原始载荷。
fn describe_notice(notice_type: &str, event: &Value) -> Option<String> {
    let operator = id_of(event.get("operator_id"));
    let target = id_of(event.get("target_id"));
    match notice_type {
        "group_increase" => Some(format!("[{notice_type}] {target} 加入")),
        "group_decrease" => Some(format!("[{notice_type}] {target} 离开")),
        "group_ban" => {
            let duration = event
                .get("duration")
                .and_then(Value::as_i64)
                .unwrap_or_default();
            Some(format!("[{notice_type}] {target} 被 {operator} 禁言 {duration} 秒"))
        }
        "group_recall" | "friend_recall" => {
            Some(format!("[{notice_type}] {operator} 撤回了一条消息"))
        }
        "group_admin" => Some(format!("[{notice_type}] {target} 的管理员身份变了")),
        _ => None,
    }
}

fn comment_of(event: &Value) -> String {
    event
        .get("comment")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// 号码可能是数字也可能是字符串，两种都认；没有就给空串。
fn id_of(value: Option<&Value>) -> String {
    match value {
        Some(Value::Number(number)) => number.to_string(),
        Some(Value::String(text)) => text.clone(),
        _ => String::new(),
    }
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn group_message() -> Value {
        json!({
            "post_type": "message",
            "message_type": "group",
            "self_id": 999,
            "group_id": 456,
            "user_id": 10001,
            "time": 1_793_000_000,
            "sender": {"nickname": "老王", "card": "老王八"},
            "message": [
                {"type": "at", "data": {"qq": "999"}},
                {"type": "text", "data": {"text": " 在吗"}}
            ]
        })
    }

    #[test]
    fn group_message_becomes_a_record_in_a_group_room() {
        let Parsed::Record(draft) = parse("qq", &group_message(), &[]).expect("能解析") else {
            panic!("群消息应当成一条记录");
        };
        assert_eq!(draft.room, RoomId::new("qq", "group/456").expect("合法"));
        assert_eq!(draft.sender.id, "10001");
        assert_eq!(draft.sender.name, "老王八", "群名片优先");
        assert!(draft.at_self, "被 @ 了");
        assert_eq!(draft.at, 1_793_000_000_000, "秒要换成毫秒");
    }

    #[test]
    fn message_keeps_the_platform_id_and_the_quote_target() {
        let event = json!({
            "post_type": "message",
            "message_type": "group",
            "self_id": 999,
            "group_id": 456,
            "user_id": 10001,
            "message_id": 592852179,
            "message": [
                {"type": "reply", "data": {"id": "592852178"}},
                {"type": "text", "data": {"text": "只发子网"}}
            ]
        });
        let Parsed::Record(draft) = parse("qq", &event, &[]).expect("能解析") else {
            panic!("应当成一条记录");
        };

        assert_eq!(
            draft.platform_id.as_deref(),
            Some("592852179"),
            "自己的平台编号要留着，别人才引用得了我"
        );
        assert!(
            matches!(
                draft.elements.first(),
                Some(Element::Quote { message_id }) if message_id == "592852178"
            ),
            "被引用那条的编号也要留着，否则判不出是不是在回我"
        );
        assert!(!draft.at_self, "引用不算 @，两件事分开记");
    }

    #[test]
    fn private_message_uses_the_sender_as_the_room() {
        let event = json!({
            "post_type": "message",
            "message_type": "private",
            "self_id": 999,
            "user_id": 10001,
            "message": [{"type": "text", "data": {"text": "私聊"}}]
        });
        let Parsed::Record(draft) = parse("qq", &event, &[]).expect("能解析") else {
            panic!("应当成一条记录");
        };
        assert_eq!(
            draft.room,
            RoomId::new("qq", "private/10001").expect("合法")
        );
        assert!(!draft.at_self);
    }

    #[test]
    fn poke_on_me_is_an_event_with_at_self() {
        let event = json!({
            "post_type": "notice",
            "notice_type": "notify",
            "sub_type": "poke",
            "self_id": 999,
            "group_id": 456,
            "user_id": 10001,
            "target_id": 999,
            "sender": {"nickname": "老王"}
        });
        let Parsed::Record(draft) = parse("qq", &event, &[]).expect("能解析") else {
            panic!("拍一拍要记下来");
        };
        assert!(matches!(
            &draft.kind,
            acb_proto::RecordKind::Event { event, tags, summary, .. }
                if event == "poke"
                    && tags.iter().any(|tag| tag == "poke")
                    && summary.as_deref() == Some("[拍了拍我]")
        ));
        assert!(draft.at_self);
        assert_eq!(draft.elements, vec![Element::text("[拍了拍我]")]);
    }

    #[test]
    fn poke_on_someone_else_is_recorded_but_not_aimed_at_me() {
        let event = json!({
            "post_type": "notice",
            "notice_type": "notify",
            "sub_type": "poke",
            "self_id": 999,
            "group_id": 456,
            "user_id": 10001,
            "target_id": 10002
        });
        let Parsed::Record(draft) = parse("qq", &event, &[]).expect("能解析") else {
            panic!("拍别人也要记");
        };
        assert!(!draft.at_self, "拍的不是我，不该叫醒我");
        assert_eq!(draft.elements, vec![Element::text("[拍了拍 10002]")]);
    }

    #[test]
    fn poke_without_sender_asks_for_member_info() {
        let event = json!({
            "post_type": "notice",
            "notice_type": "notify",
            "sub_type": "poke",
            "group_id": 123,
            "user_id": 10001,
            "target_id": 20002
        });
        assert_eq!(
            poke_missing_sender(&event),
            Some(("123".to_owned(), "10001".to_owned()))
        );

        let with_sender = json!({
            "post_type": "notice",
            "notice_type": "notify",
            "sub_type": "poke",
            "group_id": 123,
            "user_id": 10001,
            "sender": { "nickname": "老王", "role": "member" }
        });
        assert_eq!(poke_missing_sender(&with_sender), None);

        let private = json!({
            "post_type": "notice",
            "notice_type": "notify",
            "sub_type": "poke",
            "user_id": 10001
        });
        assert_eq!(
            poke_missing_sender(&private),
            None,
            "私聊没有群号，查不了群成员"
        );
    }

    #[test]
    fn other_notices_are_recorded_as_readable_events() {
        let event = json!({
            "post_type": "notice",
            "notice_type": "group_increase",
            "self_id": 999,
            "group_id": 456,
            "user_id": 10001,
            "target_id": 10002
        });
        let Parsed::Record(draft) = parse("qq", &event, &[]).expect("能解析") else {
            panic!("进退群也要记");
        };
        assert!(matches!(
            &draft.kind,
            acb_proto::RecordKind::Event { event, tags, .. }
                if event == "group_increase"
                    && tags.iter().any(|tag| tag == "group_increase")
        ));
        assert!(matches!(draft.elements[0], Element::Text { .. }));
    }

    /// 没有解析实现的事件：降级存原始 JSON（ADR-0004 补充裁定 2026-10-31）。
    /// 文本只给类型名，载荷整个进 Opaque——流水里一条不丢。
    #[test]
    fn unrecognized_notices_keep_the_raw_event_in_an_opaque_element() {
        let event = json!({
            "post_type": "notice",
            "notice_type": "group_msg_emoji_like",
            "self_id": 999,
            "group_id": 456,
            "user_id": 10001,
            "message_id": 592852179,
            "emoji_id": "5"
        });
        let Parsed::Record(draft) = parse("qq", &event, &[]).expect("能解析") else {
            panic!("不认识的事件也要记，只是不解析");
        };
        assert_eq!(draft.elements.len(), 2, "类型名一行 + 原始载荷一段");
        assert_eq!(
            draft.elements[0],
            Element::text("[group_msg_emoji_like]（未解析事件）")
        );
        match &draft.elements[1] {
            Element::Opaque { platform, payload } => {
                assert_eq!(platform, "qq");
                assert_eq!(payload["notice_type"], "group_msg_emoji_like");
                assert_eq!(payload["emoji_id"], "5", "载荷要原样，一个字段都不能丢");
            }
            other => panic!("第二段应当是 Opaque 原始载荷：{other:?}"),
        }
    }

    /// 认得的事件不附带原始载荷——有解析实现就不必浪费字节。
    #[test]
    fn recognized_notices_do_not_carry_raw_payload() {
        let event = json!({
            "post_type": "notice",
            "notice_type": "group_increase",
            "self_id": 999,
            "group_id": 456,
            "user_id": 10001,
            "target_id": 10002
        });
        let Parsed::Record(draft) = parse("qq", &event, &[]).expect("能解析") else {
            panic!("进退群也要记");
        };
        assert_eq!(draft.elements.len(), 1, "只有人话一行，没有 Opaque");
        assert!(matches!(draft.elements[0], Element::Text { .. }));
    }

    #[test]
    fn heartbeats_are_not_recorded() {
        let event =
            json!({"post_type": "meta_event", "meta_event_type": "heartbeat", "self_id": 999});
        assert_eq!(
            parse("qq", &event, &[]).expect("能解析"),
            Parsed::Ignored("心跳/生命周期".into())
        );
    }

    #[test]
    fn own_message_echoes_are_not_recorded_again() {
        let mut event = group_message();
        event["post_type"] = json!("message_sent");
        assert!(matches!(
            parse("qq", &event, &[]).expect("能解析"),
            Parsed::Ignored(_)
        ));
    }

    #[test]
    fn group_message_without_group_id_is_an_error() {
        let mut event = group_message();
        event.as_object_mut().expect("是对象").remove("group_id");
        assert_eq!(parse("qq", &event, &[]), Err(EventError::Missing("group_id")));
    }

    #[test]
    fn connection_name_comes_from_config_not_hardcoded() {
        let Parsed::Record(draft) = parse("qq-test", &group_message(), &[]).expect("能解析") else {
            panic!("应当成一条记录");
        };
        assert_eq!(draft.room.connection, "qq-test");
    }

    #[test]
    fn admin_qqs_hit_marks_system_admin() {
        let admins = vec!["10001".to_owned()];
        let Parsed::Record(draft) = parse("qq", &group_message(), &admins).expect("能解析") else {
            panic!("应当成一条记录");
        };
        assert_eq!(draft.sender.role, Some(SenderRole::SystemAdmin));
    }

    #[test]
    fn sender_role_owner_becomes_group_owner() {
        let mut event = group_message();
        event["sender"] = json!({"nickname": "老王", "role": "owner"});
        let Parsed::Record(draft) = parse("qq", &event, &[]).expect("能解析") else {
            panic!("应当成一条记录");
        };
        assert_eq!(draft.sender.role, Some(SenderRole::Owner));
    }

    #[test]
    fn sender_role_admin_becomes_group_admin() {
        let mut event = group_message();
        event["sender"] = json!({"nickname": "老王", "role": "admin"});
        let Parsed::Record(draft) = parse("qq", &event, &[]).expect("能解析") else {
            panic!("应当成一条记录");
        };
        assert_eq!(draft.sender.role, Some(SenderRole::Admin));
    }

    #[test]
    fn plain_member_has_no_role() {
        let mut event = group_message();
        event["sender"] = json!({"nickname": "老王", "role": "member"});
        let Parsed::Record(draft) = parse("qq", &event, &[]).expect("能解析") else {
            panic!("应当成一条记录");
        };
        assert_eq!(draft.sender.role, None);
    }
}
