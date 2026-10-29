//! 房间标识。
//!
//! 一个房间由「连接 + 路径」唯一确定。连接决定是哪个平台、用哪个账号在看；路径是平台侧
//! 的位置，由适配器决定怎么切段。
//!
//! 路径是**一串段**，不是任意结构。原因：它要当流水、水位、笔记的表键，必须能比较、
//! 能建索引、能前缀匹配「这个群下面所有楼」。段的内容平台自己定（`group/123`、
//! `guild/9/channel/7/thread/8`），核心不认识这些词，只当一段段字符串。

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::ProtoError;

/// 段与段之间的分隔符。
pub const PATH_SEPARATOR: char = '/';

/// 连接与路径之间的分隔符。
pub const CONNECTION_SEPARATOR: char = ':';

/// 房间标识：连接名 + 路径。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct RoomId {
    /// 连接名，如 `qq`、`dc`。同名连接只能有一个。
    pub connection: String,
    /// 路径段，如 `["group", "158296748"]`。
    pub path: Vec<String>,
}

impl RoomId {
    /// 构造并校验。`path` 用 `/` 分段的文本写法。
    ///
    /// 连接名与每一段都不许空、不许带分隔符——否则「解析回来」和「拼出去」不是同一件事。
    pub fn new(connection: impl Into<String>, path: impl Into<String>) -> Result<Self, ProtoError> {
        let connection = connection.into();
        if connection.trim().is_empty() {
            return Err(ProtoError::EmptyConnection);
        }
        if connection.contains(CONNECTION_SEPARATOR) || connection.contains(PATH_SEPARATOR) {
            return Err(ProtoError::SeparatorInPart(connection));
        }

        let path = path.into();
        let segments: Vec<String> = path.split(PATH_SEPARATOR).map(str::to_owned).collect();
        for segment in &segments {
            if segment.trim().is_empty() {
                return Err(ProtoError::EmptyPath);
            }
            if segment.contains(CONNECTION_SEPARATOR) {
                return Err(ProtoError::SeparatorInPart(segment.clone()));
            }
        }

        Ok(Self {
            connection,
            path: segments,
        })
    }

    /// 从 `连接:段/段` 解析。
    pub fn parse(text: &str) -> Result<Self, ProtoError> {
        let Some((connection, path)) = text.split_once(CONNECTION_SEPARATOR) else {
            return Err(ProtoError::EmptyConnection);
        };
        Self::new(connection, path)
    }

    /// 路径的文本写法（不含连接）。当库里的键用。
    pub fn key(&self) -> String {
        self.path.join(&PATH_SEPARATOR.to_string())
    }

    /// 上一级房间。已经在最外层就返回 `None`。
    ///
    /// 「这个群下面的楼」靠这个和 [`RoomId::key`] 的前缀匹配来查。
    pub fn parent(&self) -> Option<Self> {
        if self.path.len() <= 1 {
            return None;
        }
        Some(Self {
            connection: self.connection.clone(),
            path: self.path[..self.path.len() - 1].to_vec(),
        })
    }
}

impl fmt::Display for RoomId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{}{}{}",
            self.connection,
            CONNECTION_SEPARATOR,
            self.path.join(&PATH_SEPARATOR.to_string())
        )
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
    fn same_number_on_two_connections_are_different_rooms() {
        let qq = RoomId::new("qq", "group/123").expect("合法");
        let other = RoomId::new("qq2", "group/123").expect("合法");
        assert_ne!(qq, other);
        assert_eq!(qq.key(), other.key(), "路径一样，但房间不是同一个");
    }

    #[test]
    fn round_trips_through_text() {
        let room = RoomId::new("qq", "group/987654321").expect("合法");
        assert_eq!(room.to_string(), "qq:group/987654321");
        assert_eq!(RoomId::parse("qq:group/987654321"), Ok(room));
    }

    #[test]
    fn deep_paths_keep_every_segment() {
        let room = RoomId::new("dc", "guild/9/channel/7/thread/8").expect("合法");
        assert_eq!(room.path.len(), 6);
        assert_eq!(room.key(), "guild/9/channel/7/thread/8");
        assert_eq!(
            room.parent().expect("有上一级").to_string(),
            "dc:guild/9/channel/7/thread"
        );
    }

    #[test]
    fn parent_of_a_top_room_is_none() {
        let room = RoomId::new("qq", "group").expect("合法");
        assert_eq!(room.parent(), None);
    }

    #[test]
    fn rejects_empty_and_separators() {
        assert_eq!(RoomId::new("", "1"), Err(ProtoError::EmptyConnection));
        assert_eq!(RoomId::new("qq", "  "), Err(ProtoError::EmptyPath));
        assert_eq!(RoomId::new("qq", "a//b"), Err(ProtoError::EmptyPath));
        assert!(matches!(
            RoomId::new("qq", "a:b"),
            Err(ProtoError::SeparatorInPart(_))
        ));
        assert!(matches!(
            RoomId::new("a:b", "1"),
            Err(ProtoError::SeparatorInPart(_))
        ));
    }

    #[test]
    fn serde_shape_is_stable() {
        let room = RoomId::new("qq", "group/1").expect("合法");
        let json = serde_json::to_string(&room).expect("可序列化");
        assert_eq!(json, r#"{"connection":"qq","path":["group","1"]}"#);
        let back: RoomId = serde_json::from_str(&json).expect("可反序列化");
        assert_eq!(back, room);
    }
}
