//! 唤醒判定。
//!
//! 判定是**正向条件的析取**，从上到下第一个命中的胜出。忽略名单在整条链的最前面，
//! 所以它对所有条件生效——包括 `any_message`。这就是「除某人外都唤醒」。
//!
//! 判定在内核，不消耗模型 token。

use time::OffsetDateTime;

use crate::config::WakeConfig;

/// 一条待判定的消息，只含判定需要的字段。
#[derive(Debug, Clone)]
pub struct Incoming<'a> {
    pub group: &'a str,
    pub sender_id: &'a str,
    pub text: &'a str,
    /// 被 @ 或被引用。
    pub at_mention: bool,
    /// 有人拍一拍。
    pub poke: bool,
}

/// 判定结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Wake(WakeReason),
    Stay,
}

impl Verdict {
    pub fn reason(&self) -> Option<&WakeReason> {
        match self {
            Self::Wake(reason) => Some(reason),
            Self::Stay => None,
        }
    }
}

/// 唤醒原因标签。判定链里每个命中的条件对应一个。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WakeReason {
    AtMention,
    NameMention,
    Question,
    Keyword(String),
    Poke,
    AnyMessage,
    Probability,
    IdlePrompt,
}

impl WakeReason {
    /// 传给适配器、供运行时展示的标签。
    pub fn label(&self) -> String {
        match self {
            Self::AtMention => "at_mention".to_owned(),
            Self::NameMention => "name_mention".to_owned(),
            Self::Question => "question".to_owned(),
            Self::Keyword(keyword) => format!("keyword:{keyword}"),
            Self::Poke => "poke".to_owned(),
            Self::AnyMessage => "any_message".to_owned(),
            Self::Probability => "probability".to_owned(),
            Self::IdlePrompt => "idle_prompt".to_owned(),
        }
    }

    /// 判定链里的位置：数字小的先胜出。
    ///
    /// 逐条判定时每条只给出自己最强的那个原因；一批消息要合成**一个**原因，靠这个顺序
    /// 取最小的。批量语义因此与 [`judge`] 的链条顺序一致，不必写第二份链条。
    pub fn rank(&self) -> u8 {
        match self {
            Self::AtMention => 0,
            Self::NameMention => 1,
            Self::Question => 2,
            Self::Keyword(_) => 3,
            Self::Poke => 4,
            Self::AnyMessage => 5,
            Self::Probability => 6,
            Self::IdlePrompt => 7,
        }
    }
}

/// 判定一条消息是否唤醒。
///
/// `now` 与 `roll` 显式传入而不是内部读取，让判定可测且可重放。
/// `roll` 是 `[0, 1)` 的随机数，只在走到概率判定时使用。
pub fn judge(
    config: &WakeConfig,
    incoming: &Incoming<'_>,
    now: OffsetDateTime,
    roll: f64,
) -> Verdict {
    // 休眠期：任何条件都不唤醒。
    if let Some(until) = config.sleep_until
        && now < until
    {
        return Verdict::Stay;
    }

    // 忽略名单在整条链的最前面，对所有条件生效。
    if config.ignore_speakers.contains(incoming.sender_id) {
        return Verdict::Stay;
    }

    if config.at_mention && incoming.at_mention {
        return Verdict::Wake(WakeReason::AtMention);
    }

    if config.name_mention && mentions_any_name(incoming.text, &config.names) {
        return Verdict::Wake(WakeReason::NameMention);
    }

    if config.question && looks_like_question(incoming.text) {
        return Verdict::Wake(WakeReason::Question);
    }

    if let Some(keyword) = config
        .keywords
        .iter()
        .find(|keyword| !keyword.is_empty() && incoming.text.contains(keyword.as_str()))
    {
        return Verdict::Wake(WakeReason::Keyword(keyword.clone()));
    }

    if config.poke && incoming.poke {
        return Verdict::Wake(WakeReason::Poke);
    }

    if config.any_message {
        return Verdict::Wake(WakeReason::AnyMessage);
    }

    if config.probability > 0.0 && roll < config.probability {
        return Verdict::Wake(WakeReason::Probability);
    }

    Verdict::Stay
}

fn mentions_any_name(text: &str, names: &[String]) -> bool {
    names
        .iter()
        .any(|name| !name.is_empty() && text.contains(name.as_str()))
}

/// 「像提问」：以问号结尾，或含常见疑问词。
///
/// 这是启发式，不是语言判定——误判的代价只是多唤醒一次，模型看到内容后自己会决定不说。
fn looks_like_question(text: &str) -> bool {
    let trimmed = text.trim_end();
    if trimmed.ends_with('?') || trimmed.ends_with('？') {
        return true;
    }
    const MARKERS: [&str; 12] = [
        "吗",
        "呢",
        "什么",
        "怎么",
        "为什么",
        "哪",
        "谁",
        "多少",
        "是否",
        "能不能",
        "要不要",
        "有没有",
    ];
    MARKERS.iter().any(|marker| text.contains(marker))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn now() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_792_561_540).expect("合法时间戳")
    }

    fn base() -> WakeConfig {
        WakeConfig::default()
    }

    fn incoming<'a>(sender: &'a str, text: &'a str) -> Incoming<'a> {
        Incoming {
            group: "20001",
            sender_id: sender,
            text,
            at_mention: false,
            poke: false,
        }
    }

    #[test]
    fn default_only_wakes_on_mention_and_name() {
        let config = base();
        assert_eq!(
            judge(&config, &incoming("1", "随便说说"), now(), 0.0),
            Verdict::Stay
        );

        let mut mentioned = incoming("1", "");
        mentioned.at_mention = true;
        assert_eq!(
            judge(&config, &mentioned, now(), 0.0),
            Verdict::Wake(WakeReason::AtMention)
        );
    }

    #[test]
    fn name_mention_uses_configured_names() {
        let mut config = base();
        config.names = vec!["小鲸鱼".to_owned()];

        assert_eq!(
            judge(&config, &incoming("1", "小鲸鱼在吗"), now(), 0.0),
            Verdict::Wake(WakeReason::NameMention)
        );
        // 没配名字时，name_mention 不会凭空命中。
        assert_eq!(
            judge(&base(), &incoming("1", "小鲸鱼在吗"), now(), 0.0),
            Verdict::Stay
        );
    }

    #[test]
    fn ignore_list_beats_any_message() {
        // 这就是「除某人外都唤醒」。
        let mut config = base();
        config.any_message = true;
        config.ignore_speakers = BTreeSet::from(["30002".to_owned()]);

        assert_eq!(
            judge(&config, &incoming("30002", "随便说说"), now(), 0.0),
            Verdict::Stay
        );
        assert_eq!(
            judge(&config, &incoming("1", "随便说说"), now(), 0.0),
            Verdict::Wake(WakeReason::AnyMessage)
        );
    }

    #[test]
    fn ignore_list_beats_mention_too() {
        // 忽略名单在整条链最前面，对 @ 也生效。
        let mut config = base();
        config.ignore_speakers = BTreeSet::from(["30002".to_owned()]);

        let mut mentioned = incoming("30002", "在吗");
        mentioned.at_mention = true;
        assert_eq!(judge(&config, &mentioned, now(), 0.0), Verdict::Stay);
    }

    #[test]
    fn order_is_mention_then_name_then_question_then_keyword() {
        let mut config = base();
        config.names = vec!["小鲸鱼".to_owned()];
        config.question = true;
        config.keywords = vec!["上线".to_owned()];

        // 同时满足多个条件时，第一个命中的胜出。
        let mut all = incoming("1", "小鲸鱼上线了吗");
        all.at_mention = true;
        assert_eq!(
            judge(&config, &all, now(), 0.0),
            Verdict::Wake(WakeReason::AtMention)
        );

        assert_eq!(
            judge(&config, &incoming("1", "小鲸鱼上线了吗"), now(), 0.0),
            Verdict::Wake(WakeReason::NameMention)
        );

        assert_eq!(
            judge(&config, &incoming("1", "上线了吗"), now(), 0.0),
            Verdict::Wake(WakeReason::Question)
        );

        assert_eq!(
            judge(&config, &incoming("1", "上线"), now(), 0.0),
            Verdict::Wake(WakeReason::Keyword("上线".to_owned()))
        );
    }

    #[test]
    fn question_detection_covers_both_marks_and_markers() {
        let mut config = base();
        config.question = true;

        for text in ["在吗", "为什么", "你好？", "hello?"] {
            assert_eq!(
                judge(&config, &incoming("1", text), now(), 0.0),
                Verdict::Wake(WakeReason::Question),
                "{text} 应判为提问"
            );
        }
        assert_eq!(
            judge(&config, &incoming("1", "今天天气不错"), now(), 0.0),
            Verdict::Stay
        );
    }

    #[test]
    fn sleep_window_suppresses_everything() {
        let mut config = base();
        config.any_message = true;
        config.sleep_until = Some(now() + time::Duration::hours(1));

        let mut mentioned = incoming("1", "在吗");
        mentioned.at_mention = true;
        assert_eq!(judge(&config, &mentioned, now(), 0.0), Verdict::Stay);

        // 到期后自动恢复。
        let later = now() + time::Duration::hours(2);
        assert_eq!(
            judge(&config, &mentioned, later, 0.0),
            Verdict::Wake(WakeReason::AtMention)
        );
    }

    #[test]
    fn probability_zero_never_wakes() {
        let mut config = base();
        config.probability = 0.0;
        for roll in [0.0, 0.5, 0.999] {
            assert_eq!(
                judge(&config, &incoming("1", "随便"), now(), roll),
                Verdict::Stay
            );
        }
    }

    #[test]
    fn probability_compares_against_roll() {
        let mut config = base();
        config.probability = 0.05;

        assert_eq!(
            judge(&config, &incoming("1", "随便"), now(), 0.04),
            Verdict::Wake(WakeReason::Probability)
        );
        assert_eq!(
            judge(&config, &incoming("1", "随便"), now(), 0.05),
            Verdict::Stay
        );
    }

    #[test]
    fn empty_keyword_never_matches() {
        // 空字符串是任何文本的子串——不排除就会变成「永远唤醒」。
        let mut config = base();
        config.keywords = vec![String::new()];
        assert_eq!(
            judge(&config, &incoming("1", "随便"), now(), 0.0),
            Verdict::Stay
        );
    }

    #[test]
    fn poke_only_when_enabled() {
        let mut poke = incoming("1", "");
        poke.poke = true;

        assert_eq!(judge(&base(), &poke, now(), 0.0), Verdict::Stay);

        let mut config = base();
        config.poke = true;
        assert_eq!(
            judge(&config, &poke, now(), 0.0),
            Verdict::Wake(WakeReason::Poke)
        );
    }

    #[test]
    fn reason_labels_are_stable() {
        assert_eq!(WakeReason::AtMention.label(), "at_mention");
        assert_eq!(
            WakeReason::Keyword("上线".to_owned()).label(),
            "keyword:上线"
        );
    }
}
