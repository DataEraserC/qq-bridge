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

/// 发给当前轮次房间的参数。
#[derive(Debug, Deserialize)]
pub struct SendArgs {
    pub elements: Vec<acb_proto::Element>,
}

/// 发到别的房间的参数。形状和 [`SendArgs`] 一样，只多一个目标。
#[derive(Debug, Deserialize)]
pub struct SendToArgs {
    pub elements: Vec<acb_proto::Element>,
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
    "at_self",
    "reply_to_self",
];

/// 不指定 `columns` 时的精简默认：够 agent 读内容、知道是谁、什么时候说的。
pub const DEFAULT_COLUMNS: &[&str] = &["id", "at", "sender", "elements"];

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
}

/// 查平台能力目录（ADR-0020）。
///
/// 三样都空 = 给分组摘要；给 `group` = 这一组的方法详情；给 `query` = 按关键词找。
#[derive(Debug, Default, Deserialize)]
pub struct PlatformMethodsArgs {
    #[serde(default)]
    pub room: Option<String>,
    /// 只看某一组，如 `forward`、`group`。
    #[serde(default)]
    pub group: Option<String>,
    /// 按关键词找方法（名字或说明里含这个词）。
    #[serde(default)]
    pub query: Option<String>,
}

/// 调一个平台方法。`params` 是任意 map，core 不认识里面任何字段，原样转给适配器。
#[derive(Debug, Default, Deserialize)]
pub struct PlatformCallArgs {
    #[serde(default)]
    pub room: Option<String>,
    pub method: String,
    #[serde(default)]
    pub params: serde_json::Value,
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

/// 搜索：`path`（搜索起点）+ `needle`（关键字）。
#[derive(Debug, Deserialize)]
pub struct MemorySearchArgs {
    pub memory: String,
    #[serde(default)]
    pub path: String,
    pub needle: String,
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
