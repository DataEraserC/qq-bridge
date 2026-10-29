//! OneBot 11 事件 → 投递记录草稿。
//!
//! 消息、拍一拍、其它群事件走同一条路（ADR-0005）：都变成一条 [`RecordDraft`]，交给桥
//! 记流水、定未读、定叫醒。心跳这类「不是房间里发生的事」直接忽略，不进流水。
//!
//! 房间编号约定：
//! - 群 → `qq:group/<群号>`
//! - 私聊 → `qq:private/<对方号>`

use acb_kernel::{Element, RecordDraft, RoomId, Sender};
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
pub fn parse(platform: &str, event: &Value) -> Result<Parsed, EventError> {
    let self_id = id_of(event.get("self_id"));
    let at = event
        .get("time")
        .and_then(Value::as_i64)
        .map(|seconds| seconds * 1000)
        .unwrap_or_else(now_millis);

    match post_type(event) {
        Some("message") => message(platform, event, &self_id, at),
        // 自己发出去的消息回执。桥在发送时已经记过流水了，再记一遍会重复。
        Some("message_sent") => Ok(Parsed::Ignored("自己消息的回执".into())),
        Some("notice") => notice(platform, event, &self_id, at),
        // 心跳每几秒一次，进流水只会把记录撑爆。
        Some("meta_event") => Ok(Parsed::Ignored("心跳/生命周期".into())),
        Some("request") => request(platform, event, at),
        other => Ok(Parsed::Ignored(format!(
            "不认识的事件：{}",
            other.unwrap_or("没有 post_type")
        ))),
    }
}

fn post_type(event: &Value) -> Option<&str> {
    event.get("post_type").and_then(Value::as_str)
}

fn message(platform: &str, event: &Value, self_id: &str, at: i64) -> Result<Parsed, EventError> {
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
            room(platform, &format!("group/{group}"))?
        }
        Some("private") => room(platform, &format!("private/{sender_id}"))?,
        _ => return Err(EventError::Missing("message_type")),
    };

    let (elements, at_self) =
        segment::from_onebot(event.get("message").unwrap_or(&Value::Null), self_id);

    Ok(Parsed::Record(
        RecordDraft::message(
            room,
            Sender {
                id: sender_id,
                name: display_name(event),
            },
            at,
            elements,
            at_self,
        )
        // 平台消息编号留着：别人引用了某条消息时，桥要靠它反查「被引用的是谁发的」。
        .with_platform_id(id_of(event.get("message_id"))),
    ))
}

fn notice(platform: &str, event: &Value, self_id: &str, at: i64) -> Result<Parsed, EventError> {
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
    let room = notice_room(platform, event, &sender_id)?;
    let sender = Sender {
        id: sender_id.clone(),
        name: display_name(event),
    };

    // 拍一拍：被拍的是 target_id。拍到我，`at_self` 才为真（唤醒策略据此判定）。
    if notice_type == "notify" && event.get("sub_type").and_then(Value::as_str) == Some("poke") {
        let target = id_of(event.get("target_id"));
        let at_self = !self_id.is_empty() && target == self_id;
        let body = if at_self {
            "[拍了拍我]".to_owned()
        } else {
            format!("[拍了拍 {target}]")
        };
        return Ok(Parsed::Record(RecordDraft::event(
            room,
            sender,
            at,
            "poke",
            vec![Element::text(body)],
            at_self,
        )));
    }

    // 其它通知（进退群、禁言、撤回……）也是房间里发生的事，记一条可读的。
    let body = describe_notice(notice_type, event);
    Ok(Parsed::Record(RecordDraft::event(
        room,
        sender,
        at,
        notice_type,
        vec![Element::text(body)],
        false,
    )))
}

fn request(platform: &str, event: &Value, at: i64) -> Result<Parsed, EventError> {
    let request_type = event
        .get("request_type")
        .and_then(Value::as_str)
        .ok_or(EventError::Missing("request_type"))?;
    let sender_id = id_of(event.get("user_id"));
    let room = notice_room(platform, event, &sender_id)?;
    let body = format!("[请求 {request_type}] {}", comment_of(event));

    Ok(Parsed::Record(RecordDraft::event(
        room,
        Sender {
            id: sender_id,
            name: display_name(event),
        },
        at,
        format!("request_{request_type}"),
        vec![Element::text(body)],
        false,
    )))
}

/// 事件发生在哪个房间：有群号就是群，否则算私聊。
fn notice_room(platform: &str, event: &Value, who: &str) -> Result<RoomId, EventError> {
    let group = id_of(event.get("group_id"));
    if !group.is_empty() {
        return room(platform, &format!("group/{group}"));
    }
    if who.is_empty() {
        return Err(EventError::Missing("group_id / user_id"));
    }
    room(platform, &format!("private/{who}"))
}

fn room(platform: &str, part: &str) -> Result<RoomId, EventError> {
    RoomId::new(platform, part).map_err(|error| EventError::BadRoom(error.to_string()))
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

fn describe_notice(notice_type: &str, event: &Value) -> String {
    let operator = id_of(event.get("operator_id"));
    let target = id_of(event.get("target_id"));
    match notice_type {
        "group_increase" => format!("[{notice_type}] {target} 加入"),
        "group_decrease" => format!("[{notice_type}] {target} 离开"),
        "group_ban" => {
            let duration = event
                .get("duration")
                .and_then(Value::as_i64)
                .unwrap_or_default();
            format!("[{notice_type}] {target} 被 {operator} 禁言 {duration} 秒")
        }
        "group_recall" | "friend_recall" => {
            format!("[{notice_type}] {operator} 撤回了一条消息")
        }
        "group_admin" => format!("[{notice_type}] {target} 的管理员身份变了"),
        other => format!("[{other}] {target}"),
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
        let Parsed::Record(draft) = parse("qq", &group_message()).expect("能解析") else {
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
        let Parsed::Record(draft) = parse("qq", &event).expect("能解析") else {
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
        let Parsed::Record(draft) = parse("qq", &event).expect("能解析") else {
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
        let Parsed::Record(draft) = parse("qq", &event).expect("能解析") else {
            panic!("拍一拍要记下来");
        };
        assert_eq!(
            draft.kind,
            acb_proto::RecordKind::Event {
                event: "poke".into()
            }
        );
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
        let Parsed::Record(draft) = parse("qq", &event).expect("能解析") else {
            panic!("拍别人也要记");
        };
        assert!(!draft.at_self, "拍的不是我，不该叫醒我");
        assert_eq!(draft.elements, vec![Element::text("[拍了拍 10002]")]);
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
        let Parsed::Record(draft) = parse("qq", &event).expect("能解析") else {
            panic!("进退群也要记");
        };
        assert_eq!(
            draft.kind,
            acb_proto::RecordKind::Event {
                event: "group_increase".into()
            }
        );
        assert!(matches!(draft.elements[0], Element::Text { .. }));
    }

    #[test]
    fn heartbeats_are_not_recorded() {
        let event =
            json!({"post_type": "meta_event", "meta_event_type": "heartbeat", "self_id": 999});
        assert_eq!(
            parse("qq", &event).expect("能解析"),
            Parsed::Ignored("心跳/生命周期".into())
        );
    }

    #[test]
    fn own_message_echoes_are_not_recorded_again() {
        let mut event = group_message();
        event["post_type"] = json!("message_sent");
        assert!(matches!(
            parse("qq", &event).expect("能解析"),
            Parsed::Ignored(_)
        ));
    }

    #[test]
    fn group_message_without_group_id_is_an_error() {
        let mut event = group_message();
        event.as_object_mut().expect("是对象").remove("group_id");
        assert_eq!(parse("qq", &event), Err(EventError::Missing("group_id")));
    }

    #[test]
    fn platform_name_comes_from_config_not_hardcoded() {
        let Parsed::Record(draft) = parse("qq-test", &group_message()).expect("能解析") else {
            panic!("应当成一条记录");
        };
        assert_eq!(draft.room.platform, "qq-test");
    }
}
