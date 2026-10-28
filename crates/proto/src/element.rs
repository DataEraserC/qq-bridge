//! 内容元素：消息的最小组成单位。
//!
//! 标准元素表只增不改。适配器对标准元素做最大程度适配：平台不支持就降级，降不了就
//! 明确报错，绝不悄悄丢弃。平台特有内容装进 [`Element::Opaque`] 原样透传，核心不解释。

use serde::{Deserialize, Serialize};

/// 标准元素表。JSON 里用 `type` 区分。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Element {
    /// 纯文本。
    Text { text: String },
    /// 提及。`all` 为真时是 @ 全体，此时 `target` 留空。
    At {
        #[serde(default)]
        target: String,
        #[serde(default)]
        all: bool,
    },
    /// 引用某条消息。
    Quote { message_id: String },
    /// 图片。
    Image {
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        alt: Option<String>,
    },
    /// 音频。
    Audio { url: String },
    /// 视频。
    Video { url: String },
    /// 文件。
    File { url: String, name: String },
    /// 平台表情，带平台侧 id。
    Face { platform: String, id: String },
    /// 平台特有内容原样透传：核心不解释，编回原平台时照搬。
    Opaque {
        platform: String,
        payload: serde_json::Value,
    },
}

impl Element {
    /// 造一段文本。
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }

    /// 是不是纯文本段。取值统一用这个，别在调用处自己匹配。
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text { text } => Some(text),
            _ => None,
        }
    }
}

/// 把一串元素拼成给人看的纯文本（丢掉了 at/quote 等结构，只用于日志与降级）。
pub fn flatten_text(elements: &[Element]) -> String {
    let mut out = String::new();
    for element in elements {
        match element {
            Element::Text { text } => out.push_str(text),
            Element::At { target, all } => {
                if *all {
                    out.push_str("@全体成员");
                } else {
                    out.push('@');
                    out.push_str(target);
                }
            }
            Element::Quote { .. } => {}
            Element::Image { .. } => out.push_str("[图片]"),
            Element::Audio { .. } => out.push_str("[语音]"),
            Element::Video { .. } => out.push_str("[视频]"),
            Element::File { name, .. } => {
                out.push_str("[文件 ");
                out.push_str(name);
                out.push(']');
            }
            Element::Face { id, .. } => {
                out.push_str("[表情 ");
                out.push_str(id);
                out.push(']');
            }
            Element::Opaque { .. } => out.push_str("[平台内容]"),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_element_has_flat_shape() {
        let json = serde_json::to_string(&Element::text("你好")).expect("可序列化");
        assert_eq!(json, r#"{"type":"text","text":"你好"}"#);
    }

    #[test]
    fn every_standard_element_round_trips() {
        let elements = vec![
            Element::text("hi"),
            Element::At {
                target: "10001".into(),
                all: false,
            },
            Element::At {
                target: String::new(),
                all: true,
            },
            Element::Quote {
                message_id: "m1".into(),
            },
            Element::Image {
                url: "https://example.com/a.png".into(),
                alt: None,
            },
            Element::Audio {
                url: "https://example.com/a.mp3".into(),
            },
            Element::Video {
                url: "https://example.com/a.mp4".into(),
            },
            Element::File {
                url: "https://example.com/a.zip".into(),
                name: "a.zip".into(),
            },
            Element::Face {
                platform: "qq".into(),
                id: "178".into(),
            },
        ];
        for element in elements {
            let json = serde_json::to_string(&element).expect("可序列化");
            let back: Element = serde_json::from_str(&json).expect("可反序列化");
            assert_eq!(back, element, "往返后应当相等：{json}");
        }
    }

    #[test]
    fn opaque_payload_survives_untouched() {
        let element = Element::Opaque {
            platform: "kook".into(),
            payload: serde_json::json!({"card": [{"type": "card", "theme": "primary"}]}),
        };
        let json = serde_json::to_string(&element).expect("可序列化");
        let back: Element = serde_json::from_str(&json).expect("可反序列化");
        assert_eq!(back, element);
    }

    #[test]
    fn flatten_keeps_text_and_marks_the_rest() {
        let elements = vec![
            Element::text("看这个 "),
            Element::Image {
                url: "u".into(),
                alt: None,
            },
            Element::At {
                target: "10001".into(),
                all: false,
            },
        ];
        assert_eq!(flatten_text(&elements), "看这个 [图片]@10001");
    }
}
