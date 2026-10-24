//! qq-agent-bridge 内核：处理全部 QQ 侧语义。
//!
//! 内核不知道运行时的存在。它只认识「记忆名」和「一个可以投递内容的通道」。

pub mod archive;
pub mod config;
pub mod deliver;
pub mod notes;
pub mod pipeline;
pub mod turn;
pub mod wake;
