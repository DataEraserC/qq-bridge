//! 工具面：agent 能调的东西。
//!
//! 执行端全在桥里（流水、笔记、订阅表都在桥手上），harness 只负责让 agent 看见这些
//! 工具、把调用原样转发过来。参数形状统一用 snake_case 的 JSON 对象。
//!
//! 铁律：没有任何工具能发到订阅之外的房间；没有自由改配置或改水位的工具。
//!
//! 房间参数的写法一律是 `连接:段/段`（`qq:group/123`）。`send_message` 回当前轮次会话，
//! `send_message_to` 的目标只落在**当前会话订阅的房间**范围内——跨会话发送已砍（ADR-0017）。

use serde::Deserialize;

/// 工具清单。顺序即给 harness 注册的顺序。
pub const TOOL_NAMES: &[&str] = &[
    "send_message",
    "send_message_to",
    "ack_messages",
    "get_history",
    "fetch_records",
    "list_rooms",
    "get_notes",
    "set_note",
    "del_note",
    "get_platform_events",
    // 平台能力：一个目录 + 一个通用调用口（ADR-0020）。core 不认识任何方法名。
    "platform_methods",
    "platform_call",
    // 插件目录：本会话实际用了哪些 wake/access/delivery 策略，它们声称做什么。
    "describe_plugins",
    // 反馈：把「哪里卡住了」写回桥里（ADR-0021）。活干不下去时用它，不要直接摸平台端口。
    "report_feedback",
    // 记忆：文件层
    "memory_read",
    "memory_write",
    "memory_list",
    "memory_search",
    // 记忆：git 层
    "memory_git_status",
    "memory_git_log",
    "memory_git_diff",
    "memory_git_blame",
    "memory_git_add",
    "memory_git_commit",
    "memory_git_push",
    "memory_git_branch",
];

/// 元素数组的包装：解析走 proto 的人话通道（别名兼容 + 报错带原文），
/// 不用 serde 的干巴巴缺字段报错。
#[derive(Debug)]
pub struct Elements(pub Vec<acb_proto::Element>);

impl<'de> serde::Deserialize<'de> for Elements {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        acb_proto::parse_elements(value)
            .map(Elements)
            .map_err(serde::de::Error::custom)
    }
}

/// 发给当前轮次房间的参数。
#[derive(Debug, Deserialize)]
pub struct SendArgs {
    pub elements: Elements,
}

/// 发到别的房间的参数。形状和 [`SendArgs`] 一样，只多一个目标。
#[derive(Debug, Deserialize)]
pub struct SendToArgs {
    pub elements: Elements,
    /// 目标房间，形如 `qq:group/123`。必须落在当前会话订阅范围之内。
    pub to: String,
}

/// 确认这批消息处理完了。
#[derive(Debug, Deserialize)]
pub struct AckArgs {
    pub ids: Vec<u64>,
}

/// 查流水。
#[derive(Debug, Default, Deserialize)]
pub struct HistoryArgs {
    /// 房间；不填就是当前轮次的房间。
    #[serde(default)]
    pub room: Option<String>,
    /// 最多几条，默认 20，上限 200。
    #[serde(default)]
    pub limit: Option<usize>,
    /// 只看编号小于它的记录（往前翻）。
    #[serde(default)]
    pub before: Option<u64>,
    /// 只看编号大于它的记录（往后翻，按从旧到新返回）。给了它就走「补看更新消息」模式。
    #[serde(default)]
    pub after: Option<u64>,
    /// 只返回这些列。缺省给一个精简子集（id/at/sender/elements）。
    /// 非法列名会报错，并在错误里列出全部合法列名。
    #[serde(default)]
    pub columns: Option<Vec<String>>,
}

/// 按水位编号精确查一批记录。
#[derive(Debug, Default, Deserialize)]
pub struct FetchRecordsArgs {
    /// 房间；不填就是当前轮次的房间。水位编号是房间内递增的，跨房间会撞号，
    /// 所以必须限定在一个房间内查。
    #[serde(default)]
    pub room: Option<String>,
    /// 要查的水位编号（投递记录前面的 `#编号`）。
    pub ids: Vec<u64>,
    /// 只返回这些列。缺省给一个精简子集（id/at/sender/elements）。
    /// 非法列名会报错，并在错误里列出全部合法列名。
    #[serde(default)]
    pub columns: Option<Vec<String>>,
}

/// 一条记录能按需取的全部列。顺序即文档/错误提示里的列出顺序。
pub const RECORD_COLUMNS: &[&str] = &[
    "id",
    "room",
    "kind",
    "sender",
    "at",
    "elements",
    "platform_id",
    "quote_ref",
    "at_self",
    "reply_to_self",
    "labels",
];

/// 不指定 `columns` 时的精简默认：够 agent 读内容、知道是谁、什么时候说的。
pub const DEFAULT_COLUMNS: &[&str] = &["id", "at", "sender", "elements", "labels"];

/// 查笔记。
#[derive(Debug, Default, Deserialize)]
pub struct NotesArgs {
    #[serde(default)]
    pub room: Option<String>,
    /// 只看某种笔记：`term` / `member` / `topic`。
    #[serde(default)]
    pub kind: Option<String>,
}

/// 写一条笔记。
#[derive(Debug, Deserialize)]
pub struct SetNoteArgs {
    #[serde(default)]
    pub room: Option<String>,
    /// 默认 `term`。
    #[serde(default)]
    pub kind: Option<String>,
    pub key: String,
    pub value: String,
    #[serde(default)]
    pub label: Option<String>,
    /// 只有 `topic` 用得上：`open` / `closed`。
    #[serde(default)]
    pub status: Option<String>,
}

/// 删一条笔记。
#[derive(Debug, Deserialize)]
pub struct DelNoteArgs {
    #[serde(default)]
    pub room: Option<String>,
    /// 默认 `term`。
    #[serde(default)]
    pub kind: Option<String>,
    pub key: String,
}

/// 查平台事件。
#[derive(Debug, Default, Deserialize)]
pub struct EventsArgs {
    #[serde(default)]
    pub room: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
    /// 从第几条开始给（按时间正序数，翻页用上一次返回的 `next_offset`）。
    #[serde(default)]
    pub offset: Option<usize>,
}

/// 分页默认值：一次给多少条。来源：实现默认。
///
/// 100 条对绝大多数平台列表（群成员、好友、群列表）够用；大群才需要翻页。
pub const DEFAULT_PAGE_LIMIT: usize = 100;

/// 分页上限：一次最多给多少条。再大就该翻页，而不是硬塞进上下文。
pub const MAX_PAGE_LIMIT: usize = 500;

/// 查平台能力目录（ADR-0020）。
///
/// 三样都不给 = 逐条清单（名字 + 一句话 + 标签）；给了任一过滤条件 = 命中方法连参数表一起给。
/// `group` / `tag` / `query` 可以叠加，取交集。
#[derive(Debug, Default, Deserialize)]
pub struct PlatformMethodsArgs {
    #[serde(default)]
    pub room: Option<String>,
    /// 只看这条连接（如 `qq`），不用给房间。会话必须能访问这条连接。
    #[serde(default)]
    pub connection: Option<String>,
    /// 只给每条可访问连接的摘要（平台、kind、方法数、事件/标签能力），
    /// 想展开某条再用 `connection` 查。不能和 room / connection / 过滤条件同时给。
    #[serde(default)]
    pub all: Option<bool>,
    /// 只看某一归属，如 `forward`、`group`。
    #[serde(default)]
    pub group: Option<String>,
    /// 只看带这个用途标签的方法，如 `image`、`send`、`member`。跨归属。
    #[serde(default)]
    pub tag: Option<String>,
    /// 按关键词找方法（名字、说明、标签都会搜，中英都认）。
    #[serde(default)]
    pub query: Option<String>,
}

/// 查插件/策略目录。默认只看当前会话实际用到的 provider。
#[derive(Debug, Default, Deserialize)]
pub struct DescribePluginsArgs {
    /// `session`（默认）或 `global`。
    #[serde(default)]
    pub scope: Option<String>,
    /// 只看某个扩展点（`wake` / `access` / `delivery` / `turn_error` / `clock` /
    /// `receive` / `platform_adapter`）。只对 `scope=global` 生效。
    #[serde(default)]
    pub extension: Option<String>,
}

/// 调一个平台方法。`params` 是任意 map，core 不认识里面任何字段，原样转给适配器。
///
/// `limit` / `offset` 是**桥这一层**的分页：平台照旧一次给全，桥切片后再交出去。
/// 这样不用为了分页去改五十个方法的签名，大群的大列表也不会因为「一次给不完」被写死。
///
/// `where` / `fields` / `order_by` / `group_by` 也是桥这一层的事（ADR-0023）：
/// 平台不认识它们，桥在结果上做过滤、投影、排序、分组计数。字段名靠看数据得出，
/// 传错会报 `bad_field` 并列出真实可用的字段。
#[derive(Debug, Default, Deserialize)]
pub struct PlatformCallArgs {
    #[serde(default)]
    pub room: Option<String>,
    pub method: String,
    #[serde(default)]
    pub params: serde_json::Value,
    /// 结果里的主数组最多给几条。默认 [`DEFAULT_PAGE_LIMIT`]，上限 [`MAX_PAGE_LIMIT`]。
    #[serde(default)]
    pub limit: Option<usize>,
    /// 从第几条开始给（翻页用，值取上一次返回的 `_page.next_offset`）。
    #[serde(default)]
    pub offset: Option<usize>,
    /// 过滤 / 投影 / 排序 / 分组。四个都不给就是平台原样的结果。
    #[serde(flatten)]
    pub query: crate::query::Query,
}

/// 一条反馈（ADR-0021）。
///
/// 刻意做成「写建议」而不是「填表」：正文一个字段，想写多长写多长；
/// 出处编号、发生次数这些用来让看的人省事，但都有缺省——只写正文也能入库。
#[derive(Debug, Default, Deserialize)]
pub struct FeedbackArgs {
    /// 正文。第一行会被当成标题；多写几行没关系（上限 4000 字，超出会截断并告知）。
    pub text: String,
    /// 这个坑撞了几次。默认 1；说清楚次数，排序才有意义。
    #[serde(default)]
    pub count: Option<u32>,
    /// 出处：水位编号（`#620`）、平台消息 id、工具名、错误码、接口名，随便贴。
    #[serde(default)]
    pub refs: Vec<String>,
    /// 类型：`bug`（坏了）/ `gap`（缺能力）/ `friction`（能用但别扭）/ `idea`（想要什么）。
    /// 缺省 `friction`。
    #[serde(default)]
    pub kind: Option<String>,
    /// 影响：`blocker`（活干不下去）/ `major` / `minor`。缺省按类型推。
    #[serde(default)]
    pub severity: Option<String>,
    /// 建议怎么改（可空）。有就给，没有也行。
    #[serde(default)]
    pub suggestion: Option<String>,
    /// 出事时在调哪个桥工具（如 `platform_call`）。缺省取当前正在执行的工具。
    #[serde(default)]
    pub tool: Option<String>,
}

/// 记忆工具公共参数：操作哪个记忆 + worktree 内的相对路径。
///
/// `memory` 是记忆 id（必填——一个会话可能引用多份记忆）；`path` 是相对 worktree 的
/// 路径，空串或 `.` 表示仓库根。
#[derive(Debug, Default, Deserialize)]
pub struct MemoryArgs {
    pub memory: String,
    #[serde(default)]
    pub path: String,
}

/// 写文件：`path` + `content`。
#[derive(Debug, Deserialize)]
pub struct MemoryWriteArgs {
    pub memory: String,
    pub path: String,
    pub content: String,
}

/// 列目录：`path` + 分页。
#[derive(Debug, Default, Deserialize)]
pub struct MemoryListArgs {
    pub memory: String,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub offset: Option<usize>,
}

/// 搜索：`path`（搜索起点）+ `needle`（关键字）+ 分页。
#[derive(Debug, Default, Deserialize)]
pub struct MemorySearchArgs {
    pub memory: String,
    #[serde(default)]
    pub path: String,
    pub needle: String,
    /// 最多给多少条命中。默认 100，上限 500——搜出一万行对谁都没用。
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub offset: Option<usize>,
}

/// git log：`limit` 条。
#[derive(Debug, Default, Deserialize)]
pub struct MemoryLogArgs {
    pub memory: String,
    #[serde(default)]
    pub limit: Option<usize>,
}

/// git add：`path` 空串表示暂存全部。
#[derive(Debug, Default, Deserialize)]
pub struct MemoryAddArgs {
    pub memory: String,
    #[serde(default)]
    pub path: String,
}

/// git commit：`message` 提交信息。
#[derive(Debug, Deserialize)]
pub struct MemoryCommitArgs {
    pub memory: String,
    pub message: String,
}

/// git blame：`path` 必填。
#[derive(Debug, Deserialize)]
pub struct MemoryBlameArgs {
    pub memory: String,
    pub path: String,
}

/// 工具的真假判定：这个名字认识不认识。
pub fn is_known(tool: &str) -> bool {
    TOOL_NAMES.contains(&tool)
}
