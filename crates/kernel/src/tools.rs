//! 工具面：agent 能调的东西。
//!
//! 执行端全在桥里（流水、笔记、订阅表都在桥手上），harness 只负责让 agent 看见这些
//! 工具、把调用原样转发过来。参数形状统一用 snake_case 的 JSON 对象。
//!
//! 铁律：没有任何工具能发到订阅之外的房间；没有自由改配置或改水位的工具。
//!
//! 房间参数的写法一律是 `连接:段/段`（`qq:group/123`）。一次接入能碰的房间，等于这把
//! 令牌背后那组设备订阅的并集（ADR-0002）——所以「发到别处」和「读别处」是一个边界。

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
    /// 目标房间，形如 `qq:group/123`。必须落在本组设备订阅范围之内。
    pub to: String,
    /// 用哪台设备的身份说。只在「同一个房间被本组多台设备订阅」时才必须。
    pub device: Option<String>,
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

/// 工具的真假判定：这个名字认识不认识。
pub fn is_known(tool: &str) -> bool {
    TOOL_NAMES.contains(&tool)
}
