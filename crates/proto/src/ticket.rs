//! 回合票：工具身份的唯一来源。
//!
//! 票在开轮次时签发、收工时作废。工具调用不认票就不执行——迟到的、伪造的、上一轮的
//! 调用都过不了这一关。

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::error::ProtoError;

/// 一张回合票。内容由桥生成，协议层只负责原样携带。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Ticket {
    id: String,
}

impl Ticket {
    /// 构造一张票。空票没有意义，直接拒绝。
    pub fn new(id: impl Into<String>) -> Result<Self, ProtoError> {
        let id = id.into();
        if id.trim().is_empty() {
            return Err(ProtoError::EmptyTicket);
        }
        Ok(Self { id })
    }

    /// 票面文本。
    pub fn as_str(&self) -> &str {
        &self.id
    }
}

impl fmt::Display for Ticket {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ticket_serializes_as_bare_string() {
        let ticket = Ticket::new("t-1").expect("合法");
        assert_eq!(
            serde_json::to_string(&ticket).expect("可序列化"),
            r#""t-1""#
        );
    }

    #[test]
    fn empty_ticket_is_rejected() {
        assert_eq!(Ticket::new("  "), Err(ProtoError::EmptyTicket));
    }
}
