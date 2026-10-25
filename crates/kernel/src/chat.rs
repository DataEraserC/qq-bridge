//! 聊天传输：内核认识的**中立**事件与动作。
//!
//! 这一层回答一个问题：内核到底需要聊天平台的什么？
//!
//! 答案比想象中小得多。内核不做——也不需要做——协议差异相关的任何判断：
//!
//! - 它不知道「消息段」，只知道一条已经拍平成正文的文本。
//! - 它不知道 `post_type`、`notice_type`、`sub_type`，只知道「这是消息 / 这是拍一拍 /
//!   这条连接就绪了 / 这条连接说自己在线 / 这条我不管」。
//! - 它不知道私聊、频道、临时会话，因为它只处理群。
//!
//! 所以协议的差异全部落在 [`Transport`] 的实现里：OneBot、Satori、NoneBot 的 CQHTTP、
//! Koishi 的适配器输出，各写一份 `decode`/`encode`，内核一行不改。
//!
//! **这里没有 `impl Transport`。** 内核只声明它要什么，不实现任何一家协议——见
//! [ADR-0009](../../../docs/adr/0009-chat-transport-boundary.md)。

use crate::archive::Message;

/// 一个会话的身份：**命名空间 + 会话标识**。
///
/// 两段分开而不是拼成一个字符串，因为分隔符会带来歧义（群号里出现分隔符就撞车），
/// 而且相等判断必须逐段比较——这也是 Satori 文档对 `platform` 的定义：它「同时是一个
/// 命名空间，`user.id`/`guild.id` 在同一 `platform` 内才保证唯一」。
///
/// **不含实现名。** 同一 platform 上可以有两个 adapter 指向同一个会话（OneBot 的
/// SnowLuma 与 Satori 指向同一个 QQ 群），它们必须算作**同一个**会话；把 adapter 放进键
/// 会让同一个会话被拆成两份记忆。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct ConversationKey {
    /// 命名空间，如 `qq`、`qqguild`、`discord`、`kook`。
    ///
    /// 对一条已解码的消息它**永远非空**：拿不到平台的消息不会被当成消息，见
    /// [`ChatEvent::Ignored`]。
    pub platform: String,
    /// 该命名空间内的会话标识。对群聊平台就是群号。
    pub conversation: String,
}

impl ConversationKey {
    pub fn new(platform: impl Into<String>, conversation: impl Into<String>) -> Self {
        Self {
            platform: platform.into(),
            conversation: conversation.into(),
        }
    }
}

/// 一次回复要发到哪里。
///
/// 与 [`ConversationKey`] 同形却是独立类型：它们回答两个不同的问题，且在 Satori 上
/// **不一定取值相同**——收消息的会话是 `guild.id`（一个群可以有多个文本频道），发消息的
/// 目标是 `channel.id`。混用一个类型会让那处差异无处安放。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct ReplyAddress {
    pub platform: String,
    /// 真正收动作的那个对象。群聊平台是群号；Satori 上是频道号。
    pub address: String,
}

/// 一条已拍平的消息。
///
/// 字段就是内核真正读的那些。媒体只带**已落地**的路径——下载属于传输层，不属于内核。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatMessage {
    /// 收到它的连接所属账号的**平台侧**身份。内核用它判断「是不是自己发的」。
    pub self_id: String,
    /// 这条消息属于哪个会话。平台由传输层填：OneBot 用自己的平台名，Satori 用 READY
    /// 里的 `login.platform`。
    pub conversation: ConversationKey,
    /// 回复该发到哪儿。**可以与会话不同**：Satori 上收消息的会话是 `guild.id`，回消息的
    /// 目标是 `channel.id`。`None` 表示传输层没给出差异，回复等于会话本身。
    pub reply_to: Option<ReplyAddress>,
    /// 平台侧的消息标识。用于跨连接去重。
    pub message_id: String,
    /// 发言者的平台侧身份。
    pub sender_id: String,
    /// 群名片优先，没有则昵称。只是显示数据，不参与判定。
    pub sender_name: Option<String>,
    /// 已拍平的正文。媒体已换成占位符。
    pub text: String,
    /// 有没有 @ 到本账号。
    pub at_self: bool,
    /// Unix 毫秒。
    pub time: i64,
}

impl ChatMessage {
    /// 转成归档消息。`account` 是配置里的账号标识，不是平台侧身份。
    ///
    /// `own` 不在这里判定：它是**设备**的属性，同一个事件进入两台设备时结论可能不同，
    /// 所以由 [`crate::conn::route`] 在路由时按设备填。
    pub fn into_archive(&self, account: &str) -> Message {
        Message {
            seq: 0,
            account: account.to_owned(),
            platform: self.conversation.platform.clone(),
            group: self.conversation.conversation.clone(),
            reply: self
                .reply_to
                .as_ref()
                .map(|to| to.address.clone())
                .filter(|address| *address != self.conversation.conversation),
            message_id: self.message_id.clone(),
            sender_id: self.sender_id.clone(),
            sender_name: self.sender_name.clone(),
            text: self.text.clone(),
            at_self: self.at_self,
            own: false,
            at: self.time,
            media: Vec::new(),
        }
    }
}

/// 一次拍一拍。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatPoke {
    /// 拍一拍只发生在会话里。没有会话的拍一拍（好友私拍）不进判定。
    pub conversation: Option<ConversationKey>,
    /// 谁拍的。
    pub sender_id: String,
    /// 拍了谁。
    pub target_id: String,
    /// 本账号的平台侧身份。
    pub self_id: String,
}

impl ChatPoke {
    /// 这次拍一拍是否冲着本账号来的。
    pub fn targets_self(&self) -> bool {
        self.target_id == self.self_id
    }
}

/// 连接状态。
///
/// 两种就够了：能收发，以及「我不确定」。后者不是「断线」——`Incoming::Closed` 才是断线，
/// 由传输层的读循环给出。心跳说「我不在线」属于这里。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatStatus {
    /// 连接已可收发。带上平台名：有的协议（Satori）在握手里才告诉我们是哪个平台，
    /// 而平台是会话命名空间的一部分，内核要能记下来做校验与日志。
    Ready {
        /// 平台名。协议没给就是 `None`——那不是错误，只是没学到。
        platform: Option<String>,
    },
    /// 平台说本账号不在线。交给连接池退避。
    Offline,
}

/// 一帧解码后的事件。
///
/// 故意做成封闭枚举而不是 `Box<dyn Event>`：内核在这些分支上的处理是固定的，多一种
/// 事件就要多写一段判定。封闭枚举让「平台加了新事件类型」在传输层就地吸收成
/// [`ChatEvent::Ignored`]，不会悄悄流过内核。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatEvent {
    Message(ChatMessage),
    Poke(ChatPoke),
    /// 连接状态变化。
    Status(ChatStatus),
    /// 结构合法但内核不处理。`kind` 只给日志用。
    ///
    /// 调用方应丢弃而不是断连——上游多发一种通知不是错误。
    Ignored {
        kind: String,
    },
}

/// 一条要发给聊天平台的动作。
///
/// 只有发群消息一条。别的动作（撤回、禁言、传文件、发表情）等有工具面时再加，一次加一条
/// 并各自配一个测试——先留一个开放形状会让「哪些动作被支持」变得没法回答。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatAction {
    /// 发一条文本到 [`ReplyAddress`]。目标自带平台，所以「发到哪个平台」不是一个
    /// 需要调用方另行决定的参数。
    Send { to: ReplyAddress, text: String },
}

/// 平台对一次动作的回应。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatOutcome {
    Ok,
    Rejected { reason: String },
}

/// 传输层的解码失败原因。
///
/// 与内核-适配器协议的 `qqab_proto::DecodeError` 不共用类型：一个是 QQ 侧协议，一个是
/// 内核-适配器协议，两者独立演化。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TransportError {
    #[error("不是合法 JSON")]
    NotJson { detail: String },
    #[error("不是 JSON 对象")]
    NotObject,
    #[error("缺少必要的字段：{0}")]
    MissingField(&'static str),
    #[error("这个协议不认这种帧")]
    Unknown,
}

impl From<serde_json::Error> for TransportError {
    fn from(source: serde_json::Error) -> Self {
        Self::NotJson {
            detail: source.to_string(),
        }
    }
}

/// 一种聊天平台协议。
///
/// 实现方只做两件事：把平台帧解成 [`ChatEvent`]、把 [`ChatAction`] 编成平台帧。此外还要
/// 回答「一帧回来的文本是动作响应还是新事件」——这一步必须在传输层完成，因为内核不认识
/// `echo`。
///
/// **要求同步**：整条传输是与 [ADR-0006](../../../docs/adr/0006-blocking-websocket-per-thread.md)
/// 一致的阻塞模型，协议实现不该引入自己的执行器。
pub trait Transport: Send + 'static {
    /// 协议名（实现名），用于配置校验与日志。**不是平台名**：同一个协议可以服务多个平台。
    fn name(&self) -> &'static str;

    /// 这条连接服务的平台名，即会话命名空间。
    ///
    /// `None` 表示还不知道。Satori 在 `READY` 之前不知道（要读 `login.platform`），
    /// 所以那之前它解出消息也只能丢弃——猜一个平台名会把消息归进错误的命名空间，
    /// 比丢一条更糟。OneBot 的平台上不了线，构造时由配置给出。
    fn platform(&self) -> Option<String>;

    /// 把一行入站帧解成事件。
    ///
    /// 解不出来返回 `Err`，调用方记一条日志继续读——单帧失败不该断连。
    fn decode(&self, line: &str) -> Result<ChatEvent, TransportError>;

    /// 把动作编成一行出站帧。`id` 是请求标识，回来时用于配对。
    fn encode(&self, action: &ChatAction, id: &str) -> String;

    /// 把一行入站帧当作动作响应解析。`None` 表示这不是响应，是别的东西。
    ///
    /// 不返回 `Result`：解不出响应形态的帧要能退回「当成事件试试」，所以「不是响应」
    /// 与「是响应但失败」是两件事，前者是 `None`。
    fn response(&self, line: &str) -> Option<(Option<String>, ChatOutcome)>;

    /// 平台侧身份。配置里的账号要知道自己是谁。
    fn self_id(&self, account_self_id: &str) -> String {
        account_self_id.to_owned()
    }
}

/// 一帧里的请求标识是否与我们要的相等。
///
/// 协议实现之间唯一真正共享的逻辑：有的实现把 `echo` 序列化成字符串、有的序列化成数字，
/// 两种都要认。放在这里是为了不必每个实现各写一遍。
pub fn same_id(left: &str, right: &str) -> bool {
    left == right
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_becomes_an_archive_entry_without_deciding_ownership() {
        let message = ChatMessage {
            self_id: "10001".to_owned(),
            conversation: ConversationKey::new("qq", "20001"),
            reply_to: None,
            message_id: "7451".to_owned(),
            sender_id: "30002".to_owned(),
            sender_name: Some("小明".to_owned()),
            text: "在吗".to_owned(),
            at_self: true,
            time: 1_792_561_540_000,
        };

        let archived = message.into_archive("main");
        assert_eq!(archived.account, "main");
        assert_eq!(archived.platform, "qq", "平台要跟着消息进归档");
        assert_eq!(archived.group, "20001");
        assert_eq!(archived.sender_id, "30002");
        assert!(archived.at_self);
        assert!(
            !archived.own,
            "own 是设备属性，必须由路由决定，不能在这里猜"
        );
        assert_eq!(archived.seq, 0, "序号由归档分配");
    }

    #[test]
    fn same_conversation_id_on_two_platforms_is_two_conversations() {
        // 整个会话键存在的理由：QQ 群 123 与 Discord 频道 123 不是同一个会话。
        let qq = ConversationKey::new("qq", "123");
        let discord = ConversationKey::new("discord", "123");
        assert_ne!(qq, discord);
        assert_eq!(qq, ConversationKey::new("qq", "123"));
        // 顺序是确定的，才能做 BTreeMap 的键。
        assert!(discord < qq, "BTreeMap 需要全序");
    }

    #[test]
    fn reply_address_is_not_the_same_type_as_the_conversation() {
        // 两个类型分开的理由：Satori 上收消息的会话是 guild.id，发消息的目标是
        // channel.id，两者不同。混用一个类型就没有地方表达这件事。
        let conversation = ConversationKey::new("satori", "guild-1");
        let address = ReplyAddress {
            platform: "satori".to_owned(),
            address: "channel-9".to_owned(),
        };
        assert_ne!(conversation.conversation, address.address);
        assert_eq!(conversation.platform, address.platform);
    }

    #[test]
    fn poke_targets_self_only_when_the_target_matches() {
        let mut poke = ChatPoke {
            conversation: Some(ConversationKey::new("qq", "20001")),
            sender_id: "30002".to_owned(),
            target_id: "10001".to_owned(),
            self_id: "10001".to_owned(),
        };
        assert!(poke.targets_self());

        poke.target_id = "30003".to_owned();
        assert!(!poke.targets_self(), "拍别人不是给这台设备的信号");
    }

    #[test]
    fn errors_carry_enough_to_log() {
        let error = TransportError::MissingField("group");
        assert!(error.to_string().contains("group"));

        let json: TransportError = serde_json::from_str::<serde_json::Value>("{")
            .unwrap_err()
            .into();
        assert!(matches!(json, TransportError::NotJson { .. }));
    }
}
