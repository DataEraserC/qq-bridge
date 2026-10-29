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
    "list_rooms",
    "get_notes",
    "set_note",
    "del_note",
    "get_platform_events",
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
}

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
