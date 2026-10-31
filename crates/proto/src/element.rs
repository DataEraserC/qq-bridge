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

    /// 媒体段（带 url 的那些）的 URL 列表。出站失败时用来补提示。
    pub fn media_urls(&self) -> Vec<&str> {
        match self {
            Self::Image { url, .. } | Self::Audio { url } | Self::Video { url } | Self::File { url, .. } => {
                vec![url.as_str()]
            }
            _ => Vec::new(),
        }
    }
}

/// 把模型给的媒体段字段名归一到标准形状（就地修改）。
///
/// 平台原生 API 用 `file`，模型经常混着写（2026-10-31 真机：image 段给了
/// `file`，换来一句干巴巴的 `missing field url`，反复试错三轮才发出去）。
/// 既然两边都在上下文里，桥把 `file`/`path`/`src` 当 `url` 的别名收下。
fn normalize_element(value: &mut serde_json::Value) {
    let kind = value.get("type").and_then(serde_json::Value::as_str).unwrap_or("").to_owned();
    let aliases: &[&str] = match kind.as_str() {
        "image" | "audio" | "video" => &["file", "path", "src"],
        "file" => &["path", "src"],
        _ => return,
    };
    let Some(obj) = value.as_object_mut() else {
        return;
    };
    if obj.contains_key("url") {
        return;
    }
    for alias in aliases {
        if let Some(url) = obj.remove(*alias) {
            obj.insert("url".to_owned(), url);
            return;
        }
    }
}

/// 从工具参数解析元素数组，失败时报人话。
///
/// serde 的缺字段报错（`missing field url`）信息量是零——不说该给什么、不贴
/// 原文。这里补齐：哪一段、什么错、url 能给什么（http(s) 地址 / 本机路径，
/// 本机路径桥会自动中继）、认哪些别名。
pub fn parse_elements(value: serde_json::Value) -> Result<Vec<Element>, String> {
    let serde_json::Value::Array(items) = value else {
        return Err(format!("elements 要是数组，收到的是：{value}"));
    };
    let mut elements = Vec::with_capacity(items.len());
    for (index, mut item) in items.into_iter().enumerate() {
        normalize_element(&mut item);
        let element: Element = serde_json::from_value(item.clone()).map_err(|error| {
            let kind = item.get("type").and_then(serde_json::Value::as_str).unwrap_or("?");
            format!(
                "第 {index} 段（{kind}）解析失败：{error}。\
                 image/audio/video/file 段要 url 字段——可以是 http(s) 地址，\
                 也可以是本机文件路径（桥会自动中继成平台取得到的地址）；\
                 字段名 file/path/src 也认。原文：{item}"
            )
        })?;
        elements.push(element);
    }
    Ok(elements)
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

#[cfg(test)]
mod compat_tests {
    use super::*;

    #[test]
    fn file_field_alias_is_accepted_for_media() {
        // 平台原生写法（file 字段）：要能直接收下，别让人对着 missing field url 试错。
        let elements: Vec<Element> =
            parse_elements(serde_json::json!([
                {"type": "image", "file": "/tmp/a.png"},
                {"type": "image", "path": "https://e/b.png"},
                {"type": "audio", "file": "/tmp/a.mp3"},
                {"type": "file", "path": "/tmp/a.zip", "name": "a.zip"}
            ]))
            .expect("别名要认");
        assert_eq!(elements[0], Element::Image { url: "/tmp/a.png".into(), alt: None });
        assert_eq!(elements[1], Element::Image { url: "https://e/b.png".into(), alt: None });
        assert_eq!(elements[2], Element::Audio { url: "/tmp/a.mp3".into() });
        assert_eq!(
            elements[3],
            Element::File { url: "/tmp/a.zip".into(), name: "a.zip".into() }
        );
    }

    #[test]
    fn url_field_wins_over_aliases_and_missing_url_talks_human() {
        // 已有 url 时别的字段不抢座。
        let elements: Vec<Element> = parse_elements(serde_json::json!([
            {"type": "image", "url": "https://e/a.png", "file": "/tmp/x.png"}
        ]))
        .expect("能解析");
        assert_eq!(elements[0], Element::Image { url: "https://e/a.png".into(), alt: None });

        // 缺 url：报错要说清给什么、认哪些别名、原文是什么。
        let error = parse_elements(serde_json::json!([
            {"type": "image", "note": "没给地址"}
        ]))
        .expect_err("缺 url 要报错");
        assert!(error.contains("第 0 段（image）"), "{error}");
        assert!(error.contains("本机文件路径"), "{error}");
        assert!(error.contains("file/path/src"), "{error}");
        assert!(error.contains("没给地址"), "{error}");
    }

    #[test]
    fn media_urls_lists_every_bearing_element() {
        let elements = vec![
            Element::text("看"),
            Element::Image { url: "a.png".into(), alt: None },
            Element::Video { url: "b.mp4".into() },
        ];
        let urls: Vec<&str> = elements.iter().flat_map(|e| e.media_urls()).collect();
        assert_eq!(urls, vec!["a.png", "b.mp4"]);
    }
}
