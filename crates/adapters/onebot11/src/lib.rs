//! OneBot 11 适配器：把 QQ（群与私聊）接进桥。
//!
//! 分工（ADR-0004 / 0005）：
//! - **进**：平台事件解成 [`qqab_kernel::RecordDraft`]，交给桥记流水；
//! - **出**：桥给的内容元素编成 OneBot 消息段，按房间调用群/私聊发送动作；
//! - **不解释**：平台特有的段原样透传，核心不认识也不需要认识。
//!
//! 房间写法：`qq:group/<群号>`、`qq:private/<号码>`。

pub mod event;
pub mod link;
pub mod outbound;
pub mod segment;

pub use event::{EventError, Parsed};
pub use link::{Action, LinkError, LinkSlot, connect, pump, supervise};
pub use outbound::OneBotOutbound;
pub use segment::{PLATFORM, SegmentError};
