//! 出站：把桥要发的内容变成 OneBot 动作，等平台回 `message_id`。
//!
//! 发动作是「塞进队列 → 等 echo 回包」，真正的写 socket 由 [`crate::link::pump`] 那个线程做。
//! 这样桥那边调 `send` 的时候不需要碰 socket，也就不可能和读线程抢着写。

use std::sync::mpsc;
use std::time::Duration;

use acb_kernel::{CallError, Element, MethodSpec, Outbound, OutboundError, RoomId, Sent};
use serde_json::{Value, json};

use crate::catalog::{ROUTES, Route, Target, catalog};
use crate::forward;
use crate::link::{Action, LinkError, LinkSlot};
use crate::segment::to_onebot;

/// 等平台回话的上限。超了就报错，不无限挂着。
pub const SEND_TIMEOUT: Duration = Duration::from_secs(10);

// 平台方法表在 `catalog.rs`：那里的每一行都是「人也做得到的事」。

/// OneBot 出站通道。一条连接一个。
pub struct OneBotOutbound {
    slot: LinkSlot,
    /// 这条连接的连接名，别的连接发过来直接拒。
    connection: String,
    timeout: Duration,
    seq: u64,
}

impl OneBotOutbound {
    pub fn new(slot: LinkSlot, connection: impl Into<String>) -> Self {
        Self {
            slot,
            connection: connection.into(),
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
    fn send(&mut self, room: &RoomId, elements: &[Element]) -> Result<Sent, OutboundError> {
        if room.connection != self.connection {
            return Err(OutboundError::Unavailable(format!(
                "这条连接叫 {}，收到 {}",
                self.connection, room.connection
            )));
        }

        let (action, params) = action_of(room, elements)?;
        let Some(sender) = self.slot.current() else {
            return Err(OutboundError::Unavailable(format!(
                "{} 还没连上",
                self.connection
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

    fn catalog(&self, _connection: &str) -> &'static [MethodSpec] {
        catalog()
    }

    fn call(&mut self, room: &RoomId, method: &str, params: &Value) -> Result<Value, CallError> {
        if room.connection != self.connection {
            return Err(CallError::Forbidden(format!(
                "这条连接叫 {}，收到 {} 的调用",
                self.connection, room.connection
            )));
        }

        // 合并转发要一层层取，模型不该为这件事点三次（ADR-0024）。
        if method == "message.forward_content" {
            return self.forward_tree(params);
        }

        let route = ROUTES
            .iter()
            .find(|route| route.method == method)
            .ok_or_else(|| CallError::UnknownMethod {
                method: method.to_owned(),
                suggestion: None,
            })?;

        let action = action_of_route(route, room)?;
        let payload = call_payload(route, room, params)?;
        let response = self.dispatch(action, payload)?;
        parse_call_response(method, response)
    }
}

impl OneBotOutbound {
    /// 展开一条合并转发：按 `depth` / `max_nodes` 递归取，摊平成带路径的节点表。
    ///
    /// 递归放在适配器里，是因为「一次只给一层」是平台的规矩，不该让模型替我们跑循环。
    fn forward_tree(&mut self, params: &Value) -> Result<Value, CallError> {
        let id = params
            .get("id")
            .and_then(|value| match value {
                Value::String(text) => Some(text.clone()),
                Value::Number(number) => Some(number.to_string()),
                _ => None,
            })
            .filter(|id| !id.is_empty())
            .ok_or_else(|| {
                CallError::BadParams(
                    "要 id：消息里 forward 段的 data.id（整段内容看起来像 [合并转发] 时用它）"
                        .into(),
                )
            })?;

        let depth = params
            .get("depth")
            .and_then(Value::as_u64)
            .map(|value| value as usize)
            .unwrap_or(forward::DEFAULT_DEPTH);
        let max_nodes = params
            .get("max_nodes")
            .and_then(Value::as_u64)
            .map(|value| value as usize)
            .unwrap_or(forward::DEFAULT_MAX_NODES);

        let mut fetch = |id: &str| {
            let raw = self.dispatch("get_forward_msg", json!({ "id": id }))?;
            parse_call_response("message.forward_content", raw)
        };
        forward::expand(&id, depth, max_nodes, &mut fetch)
    }

    /// 塞一个动作进队列、等平台按 echo 回话。`send` 和 `call` 共用这一条通道。
    fn dispatch(&mut self, action: &str, params: Value) -> Result<Value, CallError> {
        let Some(sender) = self.slot.current() else {
            return Err(CallError::Unavailable(format!(
                "{} 还没连上",
                self.connection
            )));
        };

        self.seq += 1;
        let echo = format!("acb-call-{}", self.seq);
        let (reply_tx, reply_rx) = mpsc::channel();
        sender
            .send(Action {
                body: json!({"action": action, "params": params, "echo": echo}),
                reply: reply_tx,
            })
            .map_err(|_| CallError::Unavailable("连接刚断了".into()))?;

        reply_rx
            .recv_timeout(self.timeout)
            .map_err(|_| CallError::Unavailable("等平台回话超时".into()))?
            .map_err(|error: LinkError| CallError::Unavailable(error.to_string()))
    }
}

/// 这条路由在**这个房间**里该调哪个动作。
///
/// 只有少数方法按场所分叉（拍一拍在 QQ 里就是两个接口：`group_poke` / `friend_poke`）。
/// 分不出来就明确报错，别猜一个发出去。
fn action_of_route(route: &Route, room: &RoomId) -> Result<&'static str, CallError> {
    match route.target {
        Target::Fixed(action) => Ok(action),
        Target::ByRoom { group, private } => match room.path.as_slice() {
            [kind, _] if kind == "group" => Ok(group),
            [kind, _] if kind == "private" => Ok(private),
            _ => Err(CallError::BadParams(format!(
                "{} 要分群聊和私聊，但房间 {} 既不是 group/ 也不是 private/",
                route.method,
                room.key()
            ))),
        },
    }
}

/// 组装真正要发给平台的参数。
///
/// 能从房间推出来的参数（群里推 `group_id`、私聊推 `user_id`）就自动补上，
/// 模型不用重复填；推不出来又没给，明确报参数不对，别让平台那边回一个看不懂的错。
fn call_payload(route: &Route, room: &RoomId, params: &Value) -> Result<Value, CallError> {
    let mut payload = match params {
        Value::Object(map) => map.clone(),
        Value::Null => serde_json::Map::new(),
        _ => return Err(CallError::BadParams("params 要给一个对象（map）".into())),
    };

    // 房间目标参数（group_id / user_id）只能从当前房间推导，**不允许在 params 里换房间**。
    // 要操作同一会话的另一个房间，用 platform_call 的 room 参数，而不是偷换 group_id。
    match (route.from_group, route.from_private) {
        (Some(group_key), Some(private_key)) => {
            match room.path.as_slice() {
                [kind, id] if kind == "group" => {
                    fill_room_target(&mut payload, route.method, group_key, room, id)?;
                    // user_id 在这类方法里可能有两种身份：
                    // - poke.send 在群里：user_id 是“拍谁”，不是房间目标，必须保留；
                    // - 其它（如 message.send_forward）：user_id 指向另一个私聊房间，拒绝。
                    if route.method != "poke.send" && payload.contains_key(private_key) {
                        return Err(CallError::BadParams(format!(
                            "{} 的 {} 指向另一个私聊房间；当前房间是 {}。\
                             要发到私聊，请用 room 参数选那个私聊房间，不要在 params 里改目标",
                            route.method,
                            private_key,
                            room
                        )));
                    }
                    if route.method == "poke.send" && !payload.contains_key(private_key) {
                        return Err(CallError::BadParams(
                            "poke.send 在群里要给 user_id：拍谁就写谁".into(),
                        ));
                    }
                }
                [kind, id] if kind == "private" => {
                    fill_room_target(&mut payload, route.method, private_key, room, id)?;
                    if payload.contains_key(group_key) {
                        return Err(CallError::BadParams(format!(
                            "{} 的 {} 指向另一个群；当前房间是 {}。\
                             要发到群，请用 room 参数选那个群，不要在 params 里改目标",
                            route.method,
                            group_key,
                            room
                        )));
                    }
                }
                _ => {
                    return Err(CallError::BadParams(format!(
                        "{} 需要群聊或私聊房间，但房间 {} 既不是 group/ 也不是 private/",
                        route.method,
                        room.key()
                    )));
                }
            }
        }
        (Some(key), None) => fill_matching_group_target(&mut payload, route.method, key, room)?,
        (None, Some(key)) => fill_matching_private_target(&mut payload, route.method, key, room)?,
        (None, None) => {}
    }

    Ok(Value::Object(payload))
}

/// 群方法：当前房间必须是 group；params 里给了 group_id 就只能等于当前群。
fn fill_matching_group_target(
    payload: &mut serde_json::Map<String, Value>,
    method: &str,
    key: &str,
    room: &RoomId,
) -> Result<(), CallError> {
    let Some(id) = group_id_of(room) else {
        if let Some(value) = payload.get(key) {
            let target = value.as_str().unwrap_or_default();
            return Err(CallError::BadParams(format!(
                "{} 的 {} 指向群 {target}，但当前房间是 {}；\
                 要操作它，把 platform_call 的 room 改成 \"{}\"（并确认本会话订阅了它），\
                 不要在 params 里换目标",
                method,
                key,
                room,
                target_room_text(room, "group", target)
            )));
        }
        return Err(CallError::BadParams(format!(
            "{} 要 {}，但房间 {} 不是群类型，params 里得自己给",
            method,
            key,
            room.key()
        )));
    };
    fill_room_target(payload, method, key, room, id)
}

/// 私聊方法：当前房间必须是 private；params 里给了 user_id 就只能等于当前私聊对象。
fn fill_matching_private_target(
    payload: &mut serde_json::Map<String, Value>,
    method: &str,
    key: &str,
    room: &RoomId,
) -> Result<(), CallError> {
    let Some(id) = private_id_of(room) else {
        if let Some(value) = payload.get(key) {
            let target = value.as_str().unwrap_or_default();
            return Err(CallError::BadParams(format!(
                "{} 的 {} 指向私聊 {target}，但当前房间是 {}；\
                 要操作它，把 platform_call 的 room 改成 \"{}\"（并确认本会话订阅了它），\
                 不要在 params 里换目标",
                method,
                key,
                room,
                target_room_text(room, "private", target)
            )));
        }
        return Err(CallError::BadParams(format!(
            "{} 要 {}，但房间 {} 不是私聊类型，params 里得自己给",
            method,
            key,
            room.key()
        )));
    };
    fill_room_target(payload, method, key, room, id)
}

/// 把房间目标参数填成房间里的 id；如果 params 里已有不同的值，拒绝。
fn fill_room_target(
    payload: &mut serde_json::Map<String, Value>,
    method: &str,
    key: &str,
    room: &RoomId,
    room_id: &str,
) -> Result<(), CallError> {
    match payload.get(key) {
        Some(value) => {
            let given = value.as_str().unwrap_or_default();
            if given != room_id {
                let kind = if key == "group_id" { "group" } else { "private" };
                return Err(CallError::BadParams(format!(
                    "{} 的 {} 是 {given}，但当前房间是 {room}；\
                     不要在 params 里换目标。要操作它，把 platform_call 的 room 改成 \"{}\"\
                     （并确认本会话订阅了它）",
                    method,
                    key,
                    target_room_text(room, kind, given)
                )));
            }
        }
        None => {
            payload.insert(key.to_owned(), Value::String(room_id.to_owned()));
        }
    }
    Ok(())
}

/// 给错误信息一个能直接照抄的 room 值：用当前连接的连接名 + 目标类型 + 目标 id。
fn target_room_text(room: &RoomId, kind: &str, id: &str) -> String {
    format!("{}:{}/{}", room.connection, kind, id)
}

fn group_id_of(room: &RoomId) -> Option<&str> {
    match room.path.as_slice() {
        [kind, id] if kind == "group" => Some(id),
        _ => None,
    }
}

fn private_id_of(room: &RoomId) -> Option<&str> {
    match room.path.as_slice() {
        [kind, id] if kind == "private" => Some(id),
        _ => None,
    }
}

/// 平台回包 → 调用结果。
///
/// 读方法的成功结果在 `data` 里。失败按 retcode 分类：让模型能分清「我参数错了」「我没权限」
/// 「我们还没实现」这三件事，而不是一律当成失败（ADR-0020）。
fn parse_call_response(method: &str, value: Value) -> Result<Value, CallError> {
    let retcode = value.get("retcode").and_then(Value::as_i64).unwrap_or(-1);
    // 0 = 成功；1 = 已受理（部分实现异步处理也这么回）。
    if retcode == 0 || retcode == 1 {
        return Ok(value.get("data").cloned().unwrap_or(Value::Null));
    }

    let detail = value
        .get("wording")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .or_else(|| value.get("message").and_then(Value::as_str))
        .unwrap_or("平台没说原因");
    let reason = format!("{detail}（retcode {retcode}）");

    Err(match retcode {
        // go-cqhttp 系沿用 HTTP 风格码。
        1400 => CallError::BadParams(reason),
        1401 | 1403 => CallError::Forbidden(reason),
        1404 => CallError::NotImplemented(reason),
        _ => CallError::UnsupportedPlatform(format!("{method}：{reason}")),
    })
}

/// 房间写法决定调哪个动作。路径是「段/段」，第一段说场所类型。
fn action_of(room: &RoomId, elements: &[Element]) -> Result<(&'static str, Value), OutboundError> {
    let message =
        to_onebot(elements).map_err(|error| OutboundError::Unsupported(error.to_string()))?;

    match room.path.as_slice() {
        [kind, id] if kind == "group" => Ok((
            "send_group_msg",
            json!({"group_id": id, "message": message}),
        )),
        [kind, id] if kind == "private" => Ok((
            "send_private_msg",
            json!({"user_id": id, "message": message}),
        )),
        _ => Err(OutboundError::Unsupported(format!(
            "不认识的房间写法：{}（QQ 要写成 group/群号 或 private/号码）",
            room.key()
        ))),
    }
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
    use acb_kernel::{Risk, Scope, TargetRole};

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
            .send(&room, &[Element::text("x")])
            .expect_err("没连上就不该假装发成功");
        assert!(matches!(error, OutboundError::Unavailable(_)));
    }

    #[test]
    fn a_room_on_another_connection_is_refused() {
        let mut outbound = OneBotOutbound::new(LinkSlot::new(), "qq");
        let room = RoomId::new("qq2", "group/123").expect("合法");

        let error = outbound
            .send(&room, &[Element::text("x")])
            .expect_err("一条连接只管它自己那个号");
        assert!(matches!(error, OutboundError::Unavailable(_)));
    }

    #[test]
    fn a_deep_path_is_not_a_qq_room() {
        let room = RoomId::new("qq", "guild/9/channel/7").expect("合法");
        let error = action_of(&room, &[Element::text("hi")]).expect_err("认不出房间");
        assert!(matches!(error, OutboundError::Unsupported(_)));
    }

    // ── 平台方法目录与调用 ────────────────────────────────────────────────

    /// 目录要按「人能做的事」铺开，不是只挑安全的。风险靠分级 + 配置开关管，不靠删。
    #[test]
    fn catalog_covers_read_write_and_destructive() {
        let outbound = OneBotOutbound::new(LinkSlot::new(), "qq");
        let methods = outbound.catalog("qq");
        assert!(methods.len() >= 40, "能力面太窄：只有 {} 个", methods.len());

        for risk in [Risk::Read, Risk::Write, Risk::Destructive] {
            assert!(
                methods.iter().any(|method| method.risk == risk),
                "没有 {risk:?} 级别的方法"
            );
        }

        for method in methods {
            // 强制 resource.method 写法，目录靠点号前那段分组。
            assert!(
                method.name.contains('.'),
                "{} 没按 resource.method 命名",
                method.name
            );
            // 说明里别出现平台方法名之外的内部词，模型看的是中文说明。
            assert!(!method.summary.is_empty(), "{} 没写说明", method.name);
            assert!(!method.tags.is_empty(), "{} 没打标签", method.name);
        }
    }

    /// 每个声明的方法都得有对应的平台动作，否则声明了也调不动；反过来路由也不许悬空。
    #[test]
    fn declared_methods_and_routes_match() {
        let outbound = OneBotOutbound::new(LinkSlot::new(), "qq");
        for method in outbound.catalog("qq") {
            assert!(
                ROUTES.iter().any(|route| route.method == method.name),
                "{} 声明了但没有路由",
                method.name
            );
        }
        for route in ROUTES {
            assert!(
                catalog().iter().any(|method| method.name == route.method),
                "{} 有路由但没声明，模型永远查不到它",
                route.method
            );
        }
    }

    /// 目录要把“房间目标”和“成员目标”声明清楚，core 才能兜底拒绝换房间。
    #[test]
    fn catalog_declares_room_and_member_targets() {
        let methods = catalog();
        let poke = methods
            .iter()
            .find(|method| method.name == "poke.send")
            .expect("有 poke");
        assert!(poke
            .targets
            .iter()
            .any(|target| target.param == "group_id" && target.role == TargetRole::Room));
        assert!(poke
            .targets
            .iter()
            .any(|target| target.param == "user_id" && target.role == TargetRole::Member));

        let forward = methods
            .iter()
            .find(|method| method.name == "message.send_forward")
            .expect("有合并转发");
        assert!(forward
            .targets
            .iter()
            .any(|target| target.param == "group_id" && target.role == TargetRole::Room));
        assert!(forward
            .targets
            .iter()
            .any(|target| target.param == "user_id" && target.role == TargetRole::Room));
    }

    /// 目录里不许出现重复方法名——否则 platform_call 会静默用第一条。
    #[test]
    fn method_names_are_unique() {
        let outbound = OneBotOutbound::new(LinkSlot::new(), "qq");
        let mut seen = std::collections::HashSet::new();
        for method in outbound.catalog("qq") {
            assert!(seen.insert(method.name), "{} 重复声明了", method.name);
        }
    }

    /// 「人在群里能做的」这一类必须都在：发言、看图、查成员、踢人、禁言、改名片。
    #[test]
    fn catalog_covers_what_a_person_can_do_in_a_group() {
        let outbound = OneBotOutbound::new(LinkSlot::new(), "qq");
        let names: Vec<&str> = outbound.catalog("qq").iter().map(|m| m.name).collect();
        for expected in [
            "message.send_group",
            "message.history_group",
            "message.recall",
            "message.forward_content",
            "group.get_member_list",
            "group.get_notice",
            "group.files",
            "group.kick",
            "group.ban",
            "group.whole_ban",
            "group.set_card",
            "group.set_name",
            "group.upload_file",
            "poke.send",
            "like.send",
        ] {
            assert!(names.contains(&expected), "缺 {expected}");
        }
    }

    /// 「号上的事」和「房间里的事」要分清：前者不该逼模型先编一个群号出来。
    #[test]
    fn account_level_methods_are_connection_scoped() {
        let outbound = OneBotOutbound::new(LinkSlot::new(), "qq");
        for method in outbound.catalog("qq") {
            let account_level = matches!(
                method.name,
                "account.login_info"
                    | "account.status"
                    | "account.version"
                    | "friend.list"
                    | "friend.get_info"
                    | "friend.approve"
                    | "friend.delete"
                    | "group.list"
                    | "like.send"
                    | "system.check_url"
            );
            let expected = if account_level {
                Scope::Connection
            } else {
                Scope::Room
            };
            assert_eq!(method.scope, expected, "{} 的作用域标错了", method.name);
        }
    }

    #[test]
    fn calling_before_the_connection_is_up_is_refused() {
        let mut outbound = OneBotOutbound::new(LinkSlot::new(), "qq");
        let room = RoomId::new("qq", "group/456").expect("合法");
        let error = outbound
            .call(&room, "message.forward_content", &json!({"id": "x"}))
            .expect_err("没连上就不该假装调成功");
        assert!(matches!(error, CallError::Unavailable(_)));
    }

    #[test]
    fn a_call_for_another_connection_is_refused() {
        let mut outbound = OneBotOutbound::new(LinkSlot::new(), "qq");
        let room = RoomId::new("qq2", "group/123").expect("合法");
        let error = outbound
            .call(&room, "message.get", &json!({"message_id": "1"}))
            .expect_err("一条连接只管它自己那个号");
        assert!(matches!(error, CallError::Forbidden(_)));
    }

    #[test]
    fn an_undeclared_method_is_unknown() {
        let mut outbound = OneBotOutbound::new(LinkSlot::new(), "qq");
        let room = RoomId::new("qq", "group/123").expect("合法");
        let error = outbound
            .call(&room, "group.transfer_owner", &json!({}))
            .expect_err("没声明就调不动");
        assert!(matches!(error, CallError::UnknownMethod { .. }));
    }

    #[test]
    fn success_returns_only_the_data_payload() {
        let result = parse_call_response(
            "message.forward_content",
            json!({"status": "ok", "retcode": 0, "data": {"messages": [1, 2]}}),
        )
        .expect("应当成功");
        assert_eq!(result["messages"][1], 2);
    }

    /// 区分「我们还没实现」和「参数错了」——模型靠这个自己纠正。
    #[test]
    fn platform_retcodes_map_to_distinct_errors() {
        let not_implemented = parse_call_response(
            "message.forward_content",
            json!({"retcode": 1404, "wording": "API 不存在"}),
        )
        .expect_err("1404 是实现层面缺");
        assert!(matches!(not_implemented, CallError::NotImplemented(_)));

        let bad = parse_call_response(
            "group.get_member_info",
            json!({"retcode": 1400, "wording": "参数错误"}),
        )
        .expect_err("1400 是参数错");
        assert!(matches!(bad, CallError::BadParams(_)));

        let denied = parse_call_response(
            "message.get",
            json!({"retcode": 1403, "wording": "权限不足"}),
        )
        .expect_err("1403 是没权限");
        assert!(matches!(denied, CallError::Forbidden(_)));
    }

    /// 合并转发不给 id：先说清楚要什么，别拿着空参数去平台上撞。
    #[test]
    fn forward_content_without_an_id_is_a_param_error() {
        let mut outbound = OneBotOutbound::new(LinkSlot::new(), "qq");
        let room = RoomId::new("qq", "group/1").expect("合法");
        let error = outbound
            .call(&room, "message.forward_content", &json!({}))
            .expect_err("要 id");
        assert!(matches!(error, CallError::BadParams(_)), "{error:?}");
    }

    /// 在群里调 `group.*` 不用重复填群号。
    #[test]
    fn group_id_is_filled_from_the_room() {
        let route = ROUTES
            .iter()
            .find(|route| route.method == "group.get_member_list")
            .expect("有这个路由");
        let room = RoomId::new("qq", "group/456").expect("合法");

        let payload = call_payload(route, &room, &json!({})).expect("能补");
        assert_eq!(payload["group_id"], "456");

        // 显式给了别的群：拒绝，不允许在 params 里换房间。
        let error = call_payload(route, &room, &json!({"group_id": "999"}))
            .expect_err("不能在 params 里换群");
        assert!(matches!(error, CallError::BadParams(_)), "{error:?}");
    }

    #[test]
    fn poke_in_group_keeps_the_member_target_and_fills_the_group() {
        let route = ROUTES
            .iter()
            .find(|route| route.method == "poke.send")
            .expect("有 poke 路由");
        let room = RoomId::new("qq", "group/456").expect("合法");

        let payload = call_payload(route, &room, &json!({"user_id": "10002"})).expect("能补");
        assert_eq!(payload["group_id"], "456");
        assert_eq!(payload["user_id"], "10002", "群里的 user_id 是拍谁");

        let error = call_payload(route, &room, &json!({})).expect_err("群里必须给 user_id");
        assert!(matches!(error, CallError::BadParams(_)), "{error:?}");
    }

    #[test]
    fn poke_in_private_fills_the_target() {
        let route = ROUTES
            .iter()
            .find(|route| route.method == "poke.send")
            .expect("有 poke 路由");
        let room = RoomId::new("qq", "private/10002").expect("合法");

        let payload = call_payload(route, &room, &json!({})).expect("能补");
        assert_eq!(payload["user_id"], "10002");
        assert!(payload.get("group_id").is_none());
    }

    #[test]
    fn send_forward_uses_the_room_target_and_refuses_the_other_kind() {
        let route = ROUTES
            .iter()
            .find(|route| route.method == "message.send_forward")
            .expect("有转发布路由");
        let group = RoomId::new("qq", "group/456").expect("合法");
        let private = RoomId::new("qq", "private/10002").expect("合法");
        let messages = json!([{"type": "node", "data": {}}]);

        let payload = call_payload(route, &group, &json!({"messages": messages})).expect("能补");
        assert_eq!(payload["group_id"], "456");
        assert!(payload.get("user_id").is_none());

        let error = call_payload(
            route,
            &group,
            &json!({"messages": messages, "user_id": "10002"}),
        )
        .expect_err("群里不能把目标换成私聊");
        assert!(matches!(error, CallError::BadParams(_)), "{error:?}");

        let payload = call_payload(route, &private, &json!({"messages": messages})).expect("能补");
        assert_eq!(payload["user_id"], "10002");
    }

    /// 私聊里调要群号的方法又不给群号：明确报参数不对。
    #[test]
    fn group_method_without_a_group_room_is_a_param_error() {
        let route = ROUTES
            .iter()
            .find(|route| route.method == "group.get_member_info")
            .expect("有这个路由");
        let room = RoomId::new("qq", "private/10001").expect("合法");
        let error = call_payload(route, &room, &json!({"user_id": "1"})).expect_err("补不出来");
        assert!(matches!(error, CallError::BadParams(_)));
    }
}
