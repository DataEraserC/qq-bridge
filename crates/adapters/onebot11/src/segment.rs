//! 内容元素 ↔ OneBot 11 消息段。
//!
//! 两个方向都按「最大程度适配、降不了就明确报错、绝不悄悄丢」办（ADR-0004）：
//! - 出去：标准元素翻成 OneBot 段，翻不出来报 `Unsupported`；
//! - 进来：不认识的段装进 [`Element::Opaque`] 原样留着，编回 QQ 时照搬。

use qqab_proto::Element;
use serde_json::{Value, json};

/// 平台名。房间写成 `qq:group/123`。
pub const PLATFORM: &str = "qq";

/// 翻译失败。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SegmentError {
    /// 这个平台表达不了这段内容。
    #[error("{0}")]
    Unsupported(String),
}

/// 内容元素 → OneBot 消息段数组。
pub fn to_onebot(elements: &[Element]) -> Result<Vec<Value>, SegmentError> {
    elements.iter().map(one_to_onebot).collect()
}

fn one_to_onebot(element: &Element) -> Result<Value, SegmentError> {
    Ok(match element {
        Element::Text { text } => json!({"type": "text", "data": {"text": text}}),
        Element::At { target, all } => {
            let qq = if *all { "all" } else { target.as_str() };
            json!({"type": "at", "data": {"qq": qq}})
        }
        Element::Quote { message_id } => json!({"type": "reply", "data": {"id": message_id}}),
        Element::Image { url, .. } => json!({"type": "image", "data": {"file": url}}),
        Element::Audio { url } => json!({"type": "record", "data": {"file": url}}),
        Element::Video { url } => json!({"type": "video", "data": {"file": url}}),
        Element::Face { id, .. } => json!({"type": "face", "data": {"id": id}}),
        Element::File { .. } => {
            return Err(SegmentError::Unsupported(
                "OneBot 11 没有标准的文件段，QQ 这边发不了文件".into(),
            ));
        }
        // QQ 自己的段，原样送回去。
        Element::Opaque { platform, payload } if platform == PLATFORM => payload.clone(),
        Element::Opaque { platform, .. } => {
            return Err(SegmentError::Unsupported(format!(
                "这是 {platform} 特有的内容，QQ 发不了"
            )));
        }
    })
}

/// OneBot 消息段数组 → 内容元素，顺带判定「有没有 @ 到我」。
///
/// `message` 不是数组（老实现发的是 CQ 码字符串）时整段当文本，`at_self` 只能是假——
/// 所以连接要设成 `post-format=array`（见 `docs/05-adapters.md`）。
pub fn from_onebot(message: &Value, self_id: &str) -> (Vec<Element>, bool) {
    let Some(items) = message.as_array() else {
        let text = message.as_str().unwrap_or_default();
        if text.is_empty() {
            return (Vec::new(), false);
        }
        return (vec![Element::text(text)], false);
    };

    let mut elements = Vec::new();
    let mut at_self = false;

    for item in items {
        let kind = item.get("type").and_then(Value::as_str).unwrap_or_default();
        let data = item.get("data").cloned().unwrap_or(Value::Null);
        let field = |key: &str| {
            data.get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        };

        match kind {
            "text" => elements.push(Element::text(field("text"))),
            "at" => {
                let qq = field("qq");
                let all = qq == "all";
                if !all && !self_id.is_empty() && qq == self_id {
                    at_self = true;
                }
                elements.push(Element::At {
                    target: if all { String::new() } else { qq },
                    all,
                });
            }
            "reply" => elements.push(Element::Quote {
                message_id: field("id"),
            }),
            "image" => elements.push(Element::Image {
                url: url_of(&data),
                alt: data
                    .get("summary")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            }),
            "record" => elements.push(Element::Audio { url: url_of(&data) }),
            "video" => elements.push(Element::Video { url: url_of(&data) }),
            "face" => elements.push(Element::Face {
                platform: PLATFORM.to_owned(),
                id: field("id"),
            }),
            // 卡片、合并转发、markdown……核心不认识，原样留着。
            _ => elements.push(Element::Opaque {
                platform: PLATFORM.to_owned(),
                payload: item.clone(),
            }),
        }
    }

    (elements, at_self)
}

/// 媒体段优先取直链，取不到退回平台内部文件名。
fn url_of(data: &Value) -> String {
    for key in ["url", "file"] {
        if let Some(value) = data.get(key).and_then(Value::as_str) {
            if !value.is_empty() {
                return value.to_owned();
            }
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_and_mention_go_out_as_onebot_segments() {
        let segments = to_onebot(&[
            Element::text("在吗 "),
            Element::At {
                target: "10001".into(),
                all: false,
            },
        ])
        .expect("能翻");
        assert_eq!(
            segments[0],
            json!({"type": "text", "data": {"text": "在吗 "}})
        );
        assert_eq!(segments[1], json!({"type": "at", "data": {"qq": "10001"}}));
    }

    #[test]
    fn mention_all_uses_the_magic_value() {
        let segments = to_onebot(&[Element::At {
            target: String::new(),
            all: true,
        }])
        .expect("能翻");
        assert_eq!(segments[0]["data"]["qq"], "all");
    }

    #[test]
    fn quote_becomes_reply_segment() {
        let segments = to_onebot(&[Element::Quote {
            message_id: "77".into(),
        }])
        .expect("能翻");
        assert_eq!(segments[0]["type"], "reply");
        assert_eq!(segments[0]["data"]["id"], "77");
    }

    #[test]
    fn file_is_refused_not_silently_dropped() {
        let error = to_onebot(&[Element::File {
            url: "u".into(),
            name: "n".into(),
        }])
        .expect_err("发不了就该报错");
        assert!(matches!(error, SegmentError::Unsupported(_)));
    }

    #[test]
    fn foreign_platform_payload_is_refused() {
        let error = to_onebot(&[Element::Opaque {
            platform: "kook".into(),
            payload: json!({"card": 1}),
        }])
        .expect_err("别的平台的内容发不到 QQ");
        assert!(matches!(error, SegmentError::Unsupported(_)));
    }

    #[test]
    fn qq_payload_passes_through_unchanged() {
        let card = json!({"type": "json", "data": {"data": "{\"app\":\"x\"}"}});
        let segments = to_onebot(&[Element::Opaque {
            platform: PLATFORM.into(),
            payload: card.clone(),
        }])
        .expect("原样送回去");
        assert_eq!(segments[0], card);
    }

    #[test]
    fn incoming_message_is_decoded_and_mention_detected() {
        let message = json!([
            {"type": "at", "data": {"qq": "999"}},
            {"type": "text", "data": {"text": " 看这个"}},
            {"type": "image", "data": {"url": "https://e/a.png", "summary": "图"}},
        ]);
        let (elements, at_self) = from_onebot(&message, "999");
        assert!(at_self, "@ 到我就是冲我来的");
        assert_eq!(
            elements,
            vec![
                Element::At {
                    target: "999".into(),
                    all: false
                },
                Element::text(" 看这个"),
                Element::Image {
                    url: "https://e/a.png".into(),
                    alt: Some("图".into())
                },
            ]
        );
    }

    #[test]
    fn at_all_is_not_at_self() {
        let message = json!([{"type": "at", "data": {"qq": "all"}}]);
        let (elements, at_self) = from_onebot(&message, "999");
        assert!(!at_self);
        assert_eq!(
            elements,
            vec![Element::At {
                target: String::new(),
                all: true
            }]
        );
    }

    #[test]
    fn unknown_segment_is_kept_as_opaque() {
        let message = json!([{"type": "mface", "data": {"emoji_id": "5"}}]);
        let (elements, _) = from_onebot(&message, "999");
        assert_eq!(
            elements,
            vec![Element::Opaque {
                platform: PLATFORM.into(),
                payload: json!({"type": "mface", "data": {"emoji_id": "5"}}),
            }]
        );
    }

    #[test]
    fn cq_string_is_taken_as_plain_text() {
        let (elements, at_self) = from_onebot(&json!("[CQ:at,qq=999] 你好"), "999");
        assert_eq!(elements, vec![Element::text("[CQ:at,qq=999] 你好")]);
        assert!(
            !at_self,
            "CQ 码字符串里认不出 @，所以要设 post-format=array"
        );
    }
}
