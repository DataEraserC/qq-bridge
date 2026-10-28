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
