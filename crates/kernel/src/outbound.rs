//! 出站：桥把编好的内容元素交给平台连接。
//!
//! 桥不碰平台细节，只知道「往这个房间，发这些元素」。怎么编成 OneBot 消息链或 Discord
//! 消息，是适配器的事。
//!
//! 分发按**连接**，不按平台：同一个平台可以挂多条连接（两个 QQ 号），房间的第一段就是
//! 连接名，查表不会撞。

use acb_proto::{Element, RoomId};
use thiserror::Error;

/// 一条已经发出去的消息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sent {
    /// 平台侧消息 id。写进流水，方便引用。
    pub platform_id: String,
    /// 平台侧时间（Unix 毫秒）。平台不给就用本地时间。
    pub at: i64,
}

/// 出站失败。
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum OutboundError {
    /// 平台明确拒收（被踢了、没权限、内容违规）。
    #[error("平台拒收：{0}")]
    Rejected(String),
    /// 适配器编不出来（比如平台不支持这种元素，也降级不了）。
    #[error("平台不支持这种内容：{0}")]
    Unsupported(String),
    /// 通道本身没通（连接断了、账号掉线）。
    #[error("发送通道不可用：{0}")]
    Unavailable(String),
}

/// 出站通道。一条连接一个实现，由适配器提供。
pub trait Outbound: Send {
    /// 发一条到 `room` 指向的连接与位置。
    fn send(&mut self, room: &RoomId, elements: &[Element]) -> Result<Sent, OutboundError>;
}

/// 按连接分发的一组出站通道。
///
/// 一个设备可以同时订 QQ 群和 Discord 频道，所以桥不能只挂一个出站实现——发往哪个房间，
/// 就交给那个房间第一段指向的连接。没接连接的名字明确报错，不静默丢。
#[derive(Default)]
pub struct Outbounds {
    by_connection: std::collections::HashMap<String, Box<dyn Outbound>>,
}

impl Outbounds {
    pub fn new() -> Self {
        Self::default()
    }

    /// 给某条连接接上一个出站通道。同名重复接会覆盖前一个（配置层已经拦了重名）。
    pub fn with(mut self, connection: impl Into<String>, outbound: Box<dyn Outbound>) -> Self {
        self.by_connection.insert(connection.into(), outbound);
        self
    }

    /// 已经接了哪些连接。
    pub fn connections(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.by_connection.keys().map(String::as_str).collect();
        names.sort_unstable();
        names
    }
}

impl Outbound for Outbounds {
    fn send(&mut self, room: &RoomId, elements: &[Element]) -> Result<Sent, OutboundError> {
        match self.by_connection.get_mut(&room.connection) {
            Some(outbound) => outbound.send(room, elements),
            None => Err(OutboundError::Unavailable(format!(
                "没有名为 {} 的连接，发不到 {}",
                room.connection, room
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 记下发过什么，不真发。
    #[derive(Default)]
    struct Recorder {
        sent: Vec<String>,
    }

    impl Outbound for Recorder {
        fn send(&mut self, room: &RoomId, _elements: &[Element]) -> Result<Sent, OutboundError> {
            self.sent.push(room.to_string());
            Ok(Sent {
                platform_id: "p1".into(),
                at: 0,
            })
        }
    }

    #[test]
    fn routes_by_connection() {
        let mut outbounds = Outbounds::new()
            .with("qq", Box::new(Recorder::default()))
            .with("qq2", Box::new(Recorder::default()));

        outbounds
            .send(
                &RoomId::new("qq", "group/1").expect("合法"),
                &[Element::text("x")],
            )
            .expect("能发");
        outbounds
            .send(
                &RoomId::new("qq2", "group/1").expect("合法"),
                &[Element::text("y")],
            )
            .expect("能发");

        // 第二个号在同一个群里：房间不同（连接不同），各发各的，不互相覆盖。
        assert_eq!(outbounds.connections(), vec!["qq", "qq2"]);
    }

    #[test]
    fn unconnected_room_is_an_error() {
        let mut outbounds = Outbounds::new();
        let room = RoomId::new("kook", "1").expect("合法");
        let error = outbounds
            .send(&room, &[Element::text("x")])
            .expect_err("没接连接就该报错");
        assert!(matches!(error, OutboundError::Unavailable(_)));
    }
}
