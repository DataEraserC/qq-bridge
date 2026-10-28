//! 房间标识。
//!
//! 一个房间由「平台名 + 平台侧房间号」唯一确定。平台进键，所以 QQ 群 123 与
//! Discord 频道 123 是两个房间——这是旧实现串过台的地方，从类型上堵死。

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::ProtoError;

/// 房间标识：平台名 + 平台侧房间号。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct RoomId {
    /// 平台名，如 `qq`、`kook`、`discord`。
    pub platform: String,
    /// 平台侧的房间号。
    pub room: String,
}

impl RoomId {
    /// 构造并校验。空值、带冒号的值一律拒绝——冒号是 [`RoomId::parse`] 的分隔符。
    pub fn new(platform: impl Into<String>, room: impl Into<String>) -> Result<Self, ProtoError> {
        let platform = platform.into();
        let room = room.into();
        validate_part(&platform).map_err(|_| ProtoError::EmptyPlatform)?;
        validate_part(&room).map_err(|_| ProtoError::EmptyRoom)?;
        if platform.contains(':') {
            return Err(ProtoError::ColonInPart(platform));
        }
        if room.contains(':') {
            return Err(ProtoError::ColonInPart(room));
        }
        Ok(Self { platform, room })
    }

    /// 从 `平台:房间号` 解析。
    pub fn parse(text: &str) -> Result<Self, ProtoError> {
        let Some((platform, room)) = text.split_once(':') else {
            return Err(ProtoError::EmptyPlatform);
        };
        Self::new(platform, room)
    }
}

fn validate_part(part: &str) -> Result<(), ()> {
    if part.trim().is_empty() {
        return Err(());
    }
    Ok(())
}

impl fmt::Display for RoomId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}:{}", self.platform, self.room)
    }
}

impl FromStr for RoomId {
    type Err = ProtoError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::parse(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_number_on_two_platforms_are_different_rooms() {
        let qq = RoomId::new("qq", "123").expect("合法");
        let discord = RoomId::new("discord", "123").expect("合法");
        assert_ne!(qq, discord);
    }

    #[test]
    fn round_trips_through_text() {
        let room = RoomId::new("qq", "987654321").expect("合法");
        assert_eq!(room.to_string(), "qq:987654321");
        assert_eq!(RoomId::parse("qq:987654321"), Ok(room));
    }

    #[test]
    fn rejects_empty_and_colon() {
        assert_eq!(RoomId::new("", "1"), Err(ProtoError::EmptyPlatform));
        assert_eq!(RoomId::new("qq", "  "), Err(ProtoError::EmptyRoom));
        assert!(matches!(
            RoomId::new("qq", "12:34"),
            Err(ProtoError::ColonInPart(_))
        ));
    }

    #[test]
    fn serde_shape_is_stable() {
        let room = RoomId::new("qq", "1").expect("合法");
        let json = serde_json::to_string(&room).expect("可序列化");
        assert_eq!(json, r#"{"platform":"qq","room":"1"}"#);
        let back: RoomId = serde_json::from_str(&json).expect("可反序列化");
        assert_eq!(back, room);
    }
}
