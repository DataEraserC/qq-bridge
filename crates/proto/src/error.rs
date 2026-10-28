//! 协议层自己的错误：构造参数不合法、编码失败、帧解不开。
//!
//! 工具的失败不在这里——那是 [`crate::frame::ToolError`]，走正常的帧返回，不断连接。

use thiserror::Error;

/// 构造协议类型时的错误。
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ProtoError {
    #[error("平台名不能为空")]
    EmptyPlatform,
    #[error("房间号不能为空")]
    EmptyRoom,
    #[error("标识的这一段里不能有冒号：{0}")]
    ColonInPart(String),
    #[error("回合票不能为空")]
    EmptyTicket,
    #[error("帧编码失败：{0}")]
    Encode(String),
}

/// 解帧失败。除了「太大」，其余都不是断连理由之外的错误。
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum DecodeError {
    #[error("帧太大：{size} 字节，上限 {max}")]
    TooLarge { size: usize, max: usize },
    #[error("不是合法 JSON：{detail}")]
    NotJson { detail: String },
    #[error("不认识的帧：{frame}")]
    UnknownFrame { frame: String },
    #[error("帧缺少必需字段：{detail}")]
    MissingField { detail: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_render_readable_text() {
        let error = ProtoError::EmptyRoom;
        assert_eq!(error.to_string(), "房间号不能为空");

        let decode = DecodeError::TooLarge { size: 9, max: 4 };
        assert!(decode.to_string().contains("9"));
    }
}
