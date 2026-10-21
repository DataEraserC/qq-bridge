//! 协议帧定义。
//!
//! 内核不知道会话、回合、工具调用的运行时形态——所以这些帧里不出现这些词；
//! 适配器不知道群号、账号、消息段——所以消息内容已经是渲染好的块。

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::ErrorBody;
use crate::{Block, Scope};

/// 一帧协议消息。
///
/// `Unknown` 是向前兼容的唯一手段：结构合法但类型未知的帧必须被忽略而不是断连。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Frame {
    // ---- 内核发往适配器 ----
    Deliver(Deliver),
    Cancel(Cancel),
    Ping(Ping),
    ToolResult(ToolResult),
    HelloOk(HelloOk),
    HelloReject(HelloReject),

    // ---- 适配器发往内核 ----
    Hello(Hello),
    DeliverOk(DeliverOk),
    DeliverFailed(DeliverFailed),
    ToolCall(ToolCall),
    TurnEnd(TurnEnd),
    Pong(Pong),

    /// 结构合法但类型未知。调用方丢弃它，不断连。
    #[serde(skip)]
    Unknown {
        type_name: String,
        raw: Value,
    },
}

/// 请求适配器为一个记忆名开始一个回合。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Deliver {
    pub id: String,
    pub memory: String,
    /// 已渲染的人格文本。
    pub persona: String,
    pub blocks: Vec<Block>,
    pub scope: Scope,
    /// 唤醒原因标签，供运行时展示。
    pub wake_reason: String,
    /// 已渲染的笔记摘要，可为空字符串。适配器原样拼进提示词，不解析、不裁剪、不排序。
    #[serde(default)]
    pub notes: String,
}

/// 请求终止某记忆名进行中的回合。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cancel {
    pub id: String,
    pub memory: String,
}

/// 保活。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ping {
    pub id: String,
}

/// 对 `tool_call` 的响应。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorBody>,
}

/// 版本协商通过。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelloOk {
    pub id: String,
    pub version: u32,
}

/// 版本协商拒绝。主版本不同即拒绝，不尝试兼容。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelloReject {
    pub id: String,
    pub version: u32,
    pub reason: String,
}

/// 连接建立后适配器发 `hello`，带协议版本。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub id: String,
    pub version: u32,
    pub adapter: String,
}

/// 对 `deliver` 的成功响应。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliverOk {
    pub id: String,
}

/// 对 `deliver` 的失败响应。内核据此决定是否重试。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliverFailed {
    pub id: String,
    pub reason: String,
}

/// 模型请求执行 QQ 动作。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    /// 回合票。内核用它确定「这一轮是哪个设备、哪个范围、哪些账号」。
    pub ticket: String,
    pub memory: String,
    pub tool: String,
    /// 工具参数，语义由内核定义。
    #[serde(default)]
    pub args: Value,
    /// 可选，指定发送账号标识。协议字段名是 `as`（Rust 关键字，故加 rename）。
    #[serde(rename = "as", default, skip_serializing_if = "Option::is_none")]
    pub as_account: Option<String>,
}

/// 回合结束。内核据此出队并检查积压。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnEnd {
    pub memory: String,
    /// 对应回合的票，内核据此作废。
    pub ticket: String,
    pub status: TurnStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnStatus {
    Ok,
    Error,
    Cancelled,
}

/// 对 `ping` 的响应。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pong {
    pub id: String,
}
