//! 模型把「调用标记」当成正文写进参数时的兜底。
//!
//! 为什么要有这个：有的 harness 用文本流解析工具调用，模型一边写正文一边写标签，
//! 解析器没把边界切开，于是一串标签连正文一起落进了 `text` 参数。实测样子：
//!
//! ```text
//! 正文正文正文。</text>
//! </mcp__acb__send_message>
//! <｜｜DSML｜｜ calls>
//! <｜｜DSML｜｜ invoke name="mcp__acb__platform_methods">
//! ```
//!
//! 这类残渣会一路走到群里——它已经发生过：群里出现过带 `</mcp__acb__send_message>`
//! 的回复。桥在另一个进程里拦不住解析器，但可以把「落进参数之后」的这一段切掉。
//!
//! 判据取的是**正常正文里几乎不可能出现**的三种串（闭合标签、我们自己的方法名前缀、
//! 模型侧的特殊 token）。不用 `<invoke name=` 当判据：那句话本身可能正是要报告的内容
//! （比如「它把 `<invoke>` 写进参数了」），拿它截断会吃掉真实信息。
//!
//! 代价说清楚：真要发一段含 `</text>` 的正文（比如教人写 XML），会被截断。
//! 所以截断之后一定要在回执里说明，让模型知道「你以为发出去的不是全的」。

use acb_proto::Element;

/// 出现即视为「从这里往后不是正文」。
const LEAK_MARKERS: [&str; 3] = [
    "</text>",
    "</mcp__acb__",
    // 模型侧特殊 token，全角竖线。它不是给用户看的文本，漏出来就是解析没切开。
    "<｜｜DSML｜｜",
];

/// 从第一个标记起截断。返回清理后的正文，以及有没有截过。
///
/// 截断点之前的尾随空白一并去掉——标记前面通常跟着一个换行，留着会让正文多一行空。
pub fn trim_call_markup(text: &str) -> (String, bool) {
    let mut cut: Option<usize> = None;
    for marker in LEAK_MARKERS {
        if let Some(at) = text.find(marker) {
            cut = Some(cut.map_or(at, |best: usize| best.min(at)));
        }
    }
    match cut {
        Some(at) => (text[..at].trim_end().to_owned(), true),
        None => (text.to_owned(), false),
    }
}

/// 就地清理一串元素的文本段。返回有没有截过。
///
/// 只动 `Text`：别的元素没有「正文」这个概念，图片 url、平台载荷都不该被改。
pub fn trim_elements(elements: &mut [Element]) -> bool {
    let mut trimmed = false;
    for element in elements.iter_mut() {
        if let Element::Text { text } = element {
            let (clean, cut) = trim_call_markup(text);
            if cut {
                *text = clean;
                trimmed = true;
            }
        }
    }
    trimmed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_of(element: &Element) -> &str {
        element.as_text().expect("这个是文本段")
    }

    #[test]
    fn plain_text_untouched() {
        let (text, cut) = trim_call_markup("就这样，没问题。");
        assert_eq!(text, "就这样，没问题。");
        assert!(!cut, "没有标记就不该动它");
    }

    #[test]
    fn cut_at_closing_text_tag() {
        let raw = "正文到这儿就完了。</text>\n<refs>[\"#700\"]</refs>\n</mcp__acb__report_feedback>";
        let (text, cut) = trim_call_markup(raw);
        assert_eq!(text, "正文到这儿就完了。");
        assert!(cut);
    }

    #[test]
    fn cut_at_dsml_token() {
        // 实测漏出来的样子：正文写完了，后面跟着一串特殊 token。
        let raw = "停停停，人在。</text>\n</mcp__acb__send_message>\n<｜｜DSML｜｜ calls>\n<｜｜DSML｜｜ invoke name=\"mcp__acb__platform_methods\">";
        let (text, cut) = trim_call_markup(raw);
        assert_eq!(text, "停停停，人在。");
        assert!(cut);
    }

    #[test]
    fn takes_the_earliest_marker() {
        let raw = "前半句<｜｜DSML｜｜这里才是更早的</text>";
        let (text, _) = trim_call_markup(raw);
        assert_eq!(text, "前半句");
    }

    #[test]
    fn invoke_word_alone_is_not_markup() {
        // 报「它把 <invoke> 写进参数了」是正当内容，不能截。
        let raw = "它把 <invoke name=\"x\"> 写进了 text 参数里。";
        let (text, cut) = trim_call_markup(raw);
        assert_eq!(text, raw);
        assert!(!cut);
    }

    #[test]
    fn elements_get_cleaned() {
        let mut elements = vec![
            Element::text("第一段，干净。"),
            Element::text("第二段，被污染了。</text></mcp__acb__send_message>"),
            Element::At {
                target: "123".into(),
                all: false,
            },
        ];
        assert!(trim_elements(&mut elements));
        assert_eq!(text_of(&elements[0]), "第一段，干净。");
        assert_eq!(text_of(&elements[1]), "第二段，被污染了。");
    }

    #[test]
    fn clean_elements_report_no_change() {
        let mut elements = vec![Element::text("没问题")];
        assert!(!trim_elements(&mut elements));
    }
}
