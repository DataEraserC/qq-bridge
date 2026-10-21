# 协议

内核与适配器之间的唯一接触面。本机 Unix socket，换行分隔的 JSON 对象，双向。

## 设计约束

协议必须让两侧都保持无知：

- 内核不知道会话、回合、工具调用的运行时形态——所以协议里不出现这些词。
- 适配器不知道群号、账号、消息段——所以协议里的消息内容已经是渲染好的块。

协议是**有状态的**：一次连接承载多个记忆名的会话，靠 `memory` 字段区分。

## 帧格式

每行一个 JSON 对象，必须有 `type` 字段。未知 `type` 必须被忽略而不是断连——这是向前兼容的唯一手段。

```json
{"type": "deliver", "id": "d-1", "memory": "alice", "persona": "...", "blocks": [...], "scope": {...}, "notes": "..."}
```

所有请求带 `id`，响应带相同的 `id`。`id` 由发起方生成，只需在单条连接内唯一。

## 内核发往适配器

### `deliver`

请求适配器为一个记忆名开始一个回合。

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `id` | string | 请求标识 |
| `memory` | string | 记忆名 |
| `persona` | string | 已渲染的人格文本 |
| `blocks` | array | 内容块，见下 |
| `scope` | object | 本次投递涉及的群与可用发送账号 |
| `wake_reason` | string | 唤醒原因标签，供运行时展示 |
| `notes` | string | 已渲染的笔记摘要，可为空字符串 |

`blocks` 是渲染好的内容块，不是 QQ 消息段：

```json
{"kind": "text", "text": "..."}
{"kind": "image", "path": "/abs/path.jpg", "mime": "image/jpeg"}
{"kind": "file",  "path": "/abs/path.zip", "mime": "application/zip"}
```

路径必须是内核可读的绝对路径。适配器不负责下载媒体——媒体已由内核落地。

`scope` 只含适配器需要转述给模型的部分：

```json
{"group": "20001", "group_name": "...", "send_accounts": ["main", "backup"]}
```

`send_accounts` 是账号的**标识**，不是 QQ 号。适配器把它们当作不透明字符串，工具调用时原样回传。

`notes` 是内核渲染好的纯文本，适配器原样拼进提示词，不解析、不裁剪、不排序。预算与选取规则在内核——见[笔记](08-notes.md)。

### `cancel`

请求终止某记忆名进行中的回合。适配器应尽快中止并回 `turn_end`。

### `ping`

保活。适配器回 `pong`。

## 适配器发往内核

### `deliver_ok` / `deliver_failed`

对 `deliver` 的响应。`deliver_failed` 带 `reason`，内核据此决定是否重试。

### `tool_call`

模型请求执行 QQ 动作。

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `id` | string | 请求标识 |
| `ticket` | string | 回合票 |
| `memory` | string | 记忆名 |
| `tool` | string | 工具名 |
| `args` | object | 工具参数，语义由内核定义 |
| `as` | string? | 可选，指定发送账号标识 |

内核校验票与记忆名一致、票未过期、工具属于该范围允许的集合，然后执行。

### `turn_end`

回合结束。内核据此出队并检查积压。

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `memory` | string | 记忆名 |
| `ticket` | string | 对应回合的票，内核据此作废 |
| `status` | string | `ok` / `error` / `cancelled` |

### `pong`

对 `ping` 的响应。

## 错误

错误不是独立帧类型，而是响应上的 `error` 字段：

```json
{"type": "tool_result", "id": "t-7", "error": {"code": "ticket_expired", "message": "..."}}
```

固定错误码：

| 码 | 含义 | 发起方处理 |
| --- | --- | --- |
| `ticket_expired` | 票已作废或过期 | 不重试，回合应结束 |
| `ticket_mismatch` | 票与记忆名不匹配 | 不重试，视为协议违规 |
| `tool_not_allowed` | 工具不在该范围允许集合 | 不重试，回报模型 |
| `account_unavailable` | `send` 列表无可用账号 | 不重试，回报模型 |
| `bad_request` | 帧结构非法 | 不重试 |
| `internal` | 内核内部错误 | 可重试一次 |

## 连接与重连

适配器主动连内核。断开后按指数退避重连，退避上限固定。

重连后不重放历史帧：内核把未完成的投递视为失败，重新计算增量后再投递。协议本身不提供「断点续传」——归档才是消息的真相，协议只承载当前这一刻的请求。

## 版本协商

连接建立后适配器发 `hello`，带协议版本：

```json
{"type": "hello", "version": 1, "adapter": "dsh"}
```

内核回 `hello_ok` 或 `hello_reject`。主版本不同即拒绝，不尝试兼容。次版本差异靠「未知字段忽略」处理。

## 为什么不用 HTTP

三个理由：需要内核主动推送（投递）、需要低延迟双向、需要单机部署零端口占用。Unix socket 同时满足三条，且天然不暴露到网络。

若将来需要跨机部署，替换传输层而不改帧格式——协议本身不假设传输方式。
