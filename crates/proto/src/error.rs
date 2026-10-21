//! 协议错误：不是独立帧类型，而是响应上的 `error` 字段。

use serde::{Deserialize, Serialize};

/// 固定错误码。这是一个封闭集合——新增错误码是协议变更。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// 票已作废或过期。不重试，回合应结束。
    TicketExpired,
    /// 票与记忆名不匹配。不重试，视为协议违规。
    TicketMismatch,
    /// 工具不在该范围允许集合。不重试，回报模型。
    ToolNotAllowed,
    /// `send` 列表无可用账号。不重试，回报模型。
    AccountUnavailable,
    /// 帧结构非法。不重试。
    BadRequest,
    /// 内核内部错误。可重试一次。
    Internal,
}

impl ErrorCode {
    /// 发起方是否应重试。
    pub fn is_retryable(self) -> bool {
        matches!(self, Self::Internal)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorBody {
    pub code: ErrorCode,
    pub message: String,
}

impl ErrorBody {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_internal_is_retryable() {
        assert!(ErrorCode::Internal.is_retryable());
        for code in [
            ErrorCode::TicketExpired,
            ErrorCode::TicketMismatch,
            ErrorCode::ToolNotAllowed,
            ErrorCode::AccountUnavailable,
            ErrorCode::BadRequest,
        ] {
            assert!(!code.is_retryable(), "{code:?} 不应可重试");
        }
    }
}
