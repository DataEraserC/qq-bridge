//! OneBot 11 适配器：把 QQ（群与私聊）接进桥。
//!
//! 分工（ADR-0004 / 0005）：
//! - **进**：平台事件解成 [`acb_kernel::RecordDraft`]，交给桥记流水；
//! - **出**：桥给的内容元素编成 OneBot 消息段，按房间调用群/私聊发送动作；
//! - **不解释**：平台特有的段原样透传，核心不认识也不需要认识。
//!
//! 一条连接一个适配器实例（一个号一条）。房间写法：`<连接>:group/<群号>`、
//! `<连接>:private/<号码>`——第一段是连接名，所以两个号看同一个群是两个房间。
//!
//! 连接方式只做了正向 WebSocket（`ws-client`：桥主动连平台开出来的 WS 服务端）。
//! 反向 WS 要桥当服务端，还没做。

pub mod catalog;
pub mod event;
pub mod link;
pub mod outbound;
pub mod segment;

pub use event::{EventError, Parsed};
pub use link::{Action, LinkError, LinkSlot, connect, pump, supervise};
pub use outbound::OneBotOutbound;
pub use segment::{PLATFORM, SegmentError};
