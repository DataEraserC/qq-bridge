//! 出站：把桥要发的内容变成 OneBot 动作，等平台回 `message_id`。
//!
//! 发动作是「塞进队列 → 等 echo 回包」，真正的写 socket 由 [`crate::link::pump`] 那个线程做。
//! 这样桥那边调 `send` 的时候不需要碰 socket，也就不可能和读线程抢着写。

use std::sync::mpsc;
use std::time::Duration;

use acb_kernel::{Element, Outbound, OutboundError, RoomId, Sent};
use serde_json::{Value, json};

use crate::link::{Action, LinkError, LinkSlot};
use crate::segment::to_onebot;

/// 等平台回话的上限。超了就报错，不无限挂着。
pub const SEND_TIMEOUT: Duration = Duration::from_secs(10);

/// OneBot 出站通道。
pub struct OneBotOutbound {
    slot: LinkSlot,
    /// 这个适配器只负责这个平台，别的平台发过来直接拒。
    platform: String,
    timeout: Duration,
    seq: u64,
}

impl OneBotOutbound {
    pub fn new(slot: LinkSlot, platform: impl Into<String>) -> Self {
        Self {
            slot,
            platform: platform.into(),
            timeout: SEND_TIMEOUT,
            seq: 0,
        }
    }

    /// 换一个等回话的时限（测试用）。
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

impl Outbound for OneBotOutbound {
    fn send(
        &mut self,
        _device: &str,
        room: &RoomId,
        _account: &str,
        elements: &[Element],
    ) -> Result<Sent, OutboundError> {
        if room.platform != self.platform {
            return Err(OutboundError::Unavailable(format!(
                "这个适配器只管 {}，收到 {}",
                self.platform, room.platform
            )));
        }

        let (action, params) = action_of(room, elements)?;
        let Some(sender) = self.slot.current() else {
            return Err(OutboundError::Unavailable(format!(
                "{} 还没连上",
                self.platform
            )));
        };

        self.seq += 1;
        let echo = format!("acb-{}", self.seq);
        let (reply_tx, reply_rx) = mpsc::channel();
        sender
            .send(Action {
                body: json!({"action": action, "params": params, "echo": echo}),
                reply: reply_tx,
            })
            .map_err(|_| OutboundError::Unavailable("连接刚断了".into()))?;

        let response = reply_rx
            .recv_timeout(self.timeout)
            .map_err(|_| OutboundError::Unavailable("等平台回话超时".into()))?
            .map_err(|error: LinkError| OutboundError::Unavailable(error.to_string()))?;

        parse_response(response)
    }
}

/// 房间写法决定调哪个动作。
fn action_of(room: &RoomId, elements: &[Element]) -> Result<(&'static str, Value), OutboundError> {
    let message =
        to_onebot(elements).map_err(|error| OutboundError::Unsupported(error.to_string()))?;

    if let Some(group) = room.room.strip_prefix("group/") {
        return Ok((
            "send_group_msg",
            json!({"group_id": group, "message": message}),
        ));
    }
    if let Some(user) = room.room.strip_prefix("private/") {
        return Ok((
            "send_private_msg",
            json!({"user_id": user, "message": message}),
        ));
    }
    Err(OutboundError::Unsupported(format!(
        "不认识的房间写法：{}（QQ 要写成 group/群号 或 private/号码）",
        room.room
    )))
}

/// 平台回包 → 发送结果。
fn parse_response(value: Value) -> Result<Sent, OutboundError> {
    let retcode = value.get("retcode").and_then(Value::as_i64).unwrap_or(-1);
    // 0 = 成功；1 = 已受理（异步发送的实现会这么回）。
    if retcode != 0 && retcode != 1 {
        let detail = value
            .get("wording")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .or_else(|| value.get("message").and_then(Value::as_str))
            .unwrap_or("平台没说原因");
        return Err(OutboundError::Rejected(format!(
            "{detail}（retcode {retcode}）"
        )));
    }

    let platform_id = match value.get("data").and_then(|data| data.get("message_id")) {
        Some(Value::Number(number)) => number.to_string(),
        Some(Value::String(text)) => text.clone(),
        _ => String::new(),
    };

    Ok(Sent {
        platform_id,
        at: now_millis(),
    })
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

    #[test]
    fn group_room_maps_to_send_group_msg() {
        let room = RoomId::new("qq", "group/456").expect("合法");
        let (action, params) = action_of(&room, &[Element::text("hi")]).expect("能编");
        assert_eq!(action, "send_group_msg");
        assert_eq!(params["group_id"], "456");
        assert_eq!(params["message"][0]["type"], "text");
    }

    #[test]
    fn private_room_maps_to_send_private_msg() {
        let room = RoomId::new("qq", "private/10001").expect("合法");
        let (action, params) = action_of(&room, &[Element::text("hi")]).expect("能编");
        assert_eq!(action, "send_private_msg");
        assert_eq!(params["user_id"], "10001");
    }

    #[test]
    fn unknown_room_shape_is_refused() {
        let room = RoomId::new("qq", "123").expect("合法");
        let error = action_of(&room, &[Element::text("hi")]).expect_err("认不出房间");
        assert!(matches!(error, OutboundError::Unsupported(_)));
    }

    #[test]
    fn ok_response_yields_the_message_id() {
        let sent = parse_response(json!({
            "status": "ok",
            "retcode": 0,
            "data": {"message_id": 9001}
        }))
        .expect("应当成功");
        assert_eq!(sent.platform_id, "9001");
    }

    #[test]
    fn error_response_is_rejected_with_the_reason() {
        let error = parse_response(json!({
            "status": "failed",
            "retcode": 100,
            "wording": "群号不存在"
        }))
        .expect_err("应当报错");
        match error {
            OutboundError::Rejected(detail) => assert!(detail.contains("群号不存在")),
            other => panic!("应当是拒收，收到 {other:?}"),
        }
    }

    #[test]
    fn missing_retcode_is_treated_as_failure() {
        let error = parse_response(json!({"echo": "x"})).expect_err("没有 retcode 不算成功");
        assert!(matches!(error, OutboundError::Rejected(_)));
    }

    #[test]
    fn sending_before_the_connection_is_up_is_refused() {
        let mut outbound = OneBotOutbound::new(LinkSlot::new(), "qq");
        let room = RoomId::new("qq", "group/456").expect("合法");

        let error = outbound
            .send("main", &room, "999", &[Element::text("x")])
            .expect_err("没连上就不该假装发成功");
        assert!(matches!(error, OutboundError::Unavailable(_)));
    }

    #[test]
    fn a_room_on_another_platform_is_refused() {
        let mut outbound = OneBotOutbound::new(LinkSlot::new(), "qq");
        let room = RoomId::new("discord", "123").expect("合法");

        let error = outbound
            .send("main", &room, "999", &[Element::text("x")])
            .expect_err("一个适配器只管一个平台");
        assert!(matches!(error, OutboundError::Unavailable(_)));
    }
}
