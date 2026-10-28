//! 出站：桥把编好的内容元素交给平台适配器。
//!
//! 桥不碰平台细节，只知道「把这个设备、这个房间、用这个账号，发这些元素」。怎么编成
//! OneBot 消息链或 Discord 消息，是适配器的事。

use qqab_proto::{Element, RoomId};
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

/// 出站通道。每个平台一个实现，由适配器提供。
pub trait Outbound: Send {
    /// 发一条。`account` 是订阅里配的发送账号。
    fn send(
        &mut self,
        device: &str,
        room: &RoomId,
        account: &str,
        elements: &[Element],
    ) -> Result<Sent, OutboundError>;
}

/// 按平台分发的一组出站通道。
///
/// 一个设备可以同时订 QQ 群和 Discord 频道，所以桥不能只挂一个出站实现——发往哪个房间，
/// 就交给那个平台的适配器。没接适配器的平台明确报错，不静默丢。
#[derive(Default)]
pub struct Outbounds {
    by_platform: std::collections::HashMap<String, Box<dyn Outbound>>,
}

impl Outbounds {
    pub fn new() -> Self {
        Self::default()
    }

    /// 给某个平台接上一个出站通道。同平台重复接会覆盖前一个。
    pub fn with(mut self, platform: impl Into<String>, outbound: Box<dyn Outbound>) -> Self {
        self.by_platform.insert(platform.into(), outbound);
        self
    }

    /// 已经接了哪些平台。
    pub fn platforms(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.by_platform.keys().map(String::as_str).collect();
        names.sort_unstable();
        names
    }
}

impl Outbound for Outbounds {
    fn send(
        &mut self,
        device: &str,
        room: &RoomId,
        account: &str,
        elements: &[Element],
    ) -> Result<Sent, OutboundError> {
        match self.by_platform.get_mut(&room.platform) {
            Some(outbound) => outbound.send(device, room, account, elements),
            None => Err(OutboundError::Unavailable(format!(
                "没有为平台 {} 接上适配器",
                room.platform
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
        sent: Vec<(String, String)>,
    }

    impl Outbound for Recorder {
        fn send(
            &mut self,
            _device: &str,
            room: &RoomId,
            _account: &str,
            _elements: &[Element],
        ) -> Result<Sent, OutboundError> {
            self.sent.push((room.platform.clone(), room.room.clone()));
            Ok(Sent {
                platform_id: "p1".into(),
                at: 0,
            })
        }
    }

    #[test]
    fn routes_by_platform() {
        let mut outbounds = Outbounds::new()
            .with("qq", Box::new(Recorder::default()))
            .with("discord", Box::new(Recorder::default()));

        let qq = RoomId::new("qq", "group/1").expect("合法");
        let discord = RoomId::new("discord", "1").expect("合法");
        outbounds
            .send("main", &qq, "a", &[Element::text("x")])
            .expect("能发");
        outbounds
            .send("main", &discord, "a", &[Element::text("y")])
            .expect("能发");
        assert_eq!(outbounds.platforms(), vec!["discord", "qq"]);
    }

    #[test]
    fn unconnected_platform_is_an_error() {
        let mut outbounds = Outbounds::new();
        let room = RoomId::new("kook", "1").expect("合法");
        let error = outbounds
            .send("main", &room, "a", &[Element::text("x")])
            .expect_err("没接适配器就该报错");
        assert!(matches!(error, OutboundError::Unavailable(_)));
    }
}
