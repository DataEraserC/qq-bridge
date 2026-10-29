# 协议：桥 ↔ harness

> 状态：草稿，与 `crates/proto` 同步维护。
> 原则：桥不关心 harness 内部；harness 也不关心平台细节。这一层只说「投递什么、调用什么、返回什么」。

## 传输

- 一条长连接（本机套接字或 TCP），**每帧一行 JSON**，UTF-8。
- 单帧上限 1 MiB，超了直接断连。
- 首帧必须是 `hello`，带协议版本与令牌（TCP 时必填）。版本不符由桥拒绝并说明原因。

## 接入点

- 监听地址来自配置的 `[endpoint] listen`，默认只听 `127.0.0.1`，外网连不上。
- **令牌绑一组设备**：配置里几台设备写同一个 `token`，它们就是一组，由这条连接代表。令牌对不上任何设备就直接断开，不回 `ready`。
- 一条连接只服务它那一组设备：`poll_turn` 只领这组里的活，不会把别人的轮次领走。
- **跨设备说话只能走 `send_message_to`**：参数里没有「别的设备 id」这种入口，能发的范围就是这组设备订阅的并集（[ADR-0002](../adr/0002-subscription-model.md)）。
- 握手 10 秒内不说话就断；空闲时桥每 100ms 回头看一眼有没有活，所以有活时最迟约 0.1 秒推过去。
- **断线不丢消息**：连接断在轮次中间，这一轮直接放掉，不确认任何消息——它们还是未读，下一轮重投（[ADR-0010](../adr/0010-ack-watermark.md)）。
- 同一设备开两条连接不会破坏串行：桥保证一个设备同时只有一轮，第二条连接暂时领不到活。

## 帧一览

| 帧 | 方向 | 用途 |
|----|------|------|
| `hello` | harness → 桥 | 握手：协议版本 + 接入令牌 |
| `hello` | harness → 桥 | 握手：协议版本 + 接入令牌 |
| `ready` | 桥 → harness | 握手成功：`devices` = 这条连接代表哪几台设备（配置顺序） |
| `turn_start` | 桥 → harness | 开一轮：回合票 + 设备 + 记忆 + 房间 + 本次投递的记录 + 开轮原因 |
| `turn_end` | harness → 桥 | 本轮收工（可带错误） |
| `tool_call` | harness → 桥 | 调用工具：票 + 工具名 + 参数 |
| `tool_result` | 桥 → harness | 工具结果或错误 |
| `log` | 桥 → harness | 给 harness 侧的日志，不影响流程 |
| `ping` / `pong` | 双向 | 保活 |

## 一轮的形状

```mermaid
sequenceDiagram
    participant H as harness
    participant B as 桥
    B->>H: turn_start（票 + 房间 + 未读记录）
    H->>B: tool_call(send_message)
    B->>H: tool_result
    H->>B: tool_call(ack_messages)
    B->>H: tool_result
    H->>B: turn_end（票）
    Note over H,B: 票作废，水位已按确认推进
```

要点：

- **票是唯一身份**。`tool_call` 必须带本轮的票，票在 `turn_start` 签发、`turn_end` 作废。票不对，调用一律拒绝。
- 一轮只投一个房间的记录（[ADR-0003](../adr/0003-serial-turns.md)）。
- 同一份记忆的轮次串行：上一轮 `turn_end` 之前不会有下一轮 `turn_start`（哪怕两台设备共用一份记忆）。

## 一次投递（delivery）

`turn_start` 上带的 `delivery`：

| 字段 | 含义 |
|------|------|
| `ticket` | 本轮的票。之后每个 `tool_call` 都要带上它 |
| `device` | 这一轮替哪台设备说话 |
| `memory` | 这台设备的记忆 id。 **harness 按记忆找会话**——两台设备共用一份记忆时，它们本该落在同一个会话里 |
| `room` | 本轮只涉及这一个房间 |
| `records` | 投递的记录，可能为空 |
| `reason` | 为什么开这一轮：`wake` / `clock` |

## 记录（record）

投递给 harness 的最小单位是「记录」，消息与平台事件统一用这个形状：
```json
{
  "id": 42,
  "room": {"connection": "qq", "path": ["group", "123456"]},
  "kind": {"kind": "message"},
  "sender": {"id": "10001", "name": "老王"},
  "at": 1793000000000,
  "elements": [{"type": "text", "text": "在吗"}],
  "at_self": false,
  "reply_to_self": false,
  "platform_id": "592852179",
  "redelivery": null
}
```

- `room`：连接名 + 路径段。路径由适配器决定怎么切（QQ 是 `group/123`、`private/456`；
  Discord 这类会更深，如 `guild/9/channel/7/post/8`）。核心不认识段里的词，只把它当一串段
  ——所以要前缀查「这个群下面所有楼」也是免费的（[ADR-0012](../adr/0012-platform-connections.md)）。
- `id`：房间内递增编号，水位按它推进。
- `kind`：`message` 或 `{"kind":"event","event":"poke"}`；拍一拍就是 `event: poke`（[ADR-0007](../adr/0007-poke-as-message.md)）。
- `at_self`：这条是不是冲本账号来的（@ 到我、拍我）。
- `reply_to_self`：这条是不是**引用了我发的某条消息**。
  `at_self` 和 `reply_to_self` 是两个事实，不是一个：@ 是明确点名，引用是接着某句话往下说。
  默认两者都会叫醒（`wake.at_mention` / `wake.reply`），但可以分别关掉。
  判定靠反查：被引用那条如果是本房间的发送账号发的，就算数；查不到（被清理了、对面没给编号）
  就当不是——宁可少叫醒一次，也不凭编号长相猜。
- `platform_id`：平台自己的消息编号（QQ 的 `message_id`）。留着只有一个用途：别人引用某条时，
  靠它反查「被引用的是谁发的」。我方发出去的消息也带，所以「引用了我」才判得出来。
- `redelivery`：非空表示「第 N 次投递」。
- `note`：桥附在**这次投递**上的旁白，不是发送者说的话。目前只在重投时出现，内容是
  「第 2 次投递；调 ack_messages 确认 #5 之后就不再重复投递」。

  为什么要由桥来写这句：它是**行为要求**（agent 得知道怎么让重复投递停下），不是某家
  harness 的排版偏好。放在桥里，接新 harness 不用重新发明一遍（[ADR-0010](../adr/0010-ack-watermark.md)）。

  为什么单独一个字段、不塞进 `elements`：`elements` 是对方说的话。旁白混进内容，人和模型
  都会以为「对方打了这行字」——真机上已经出现过一次。harness 的呈现义务是：**内容照原样，
  旁白另起一行**，两者不能拼在一起。

  旁白只出现在投给 harness 的副本上，流水里存的是原话。

## 内容元素

| 元素 | 字段 |
|------|------|
| `text` | `text` |
| `at` | `target`，`all`（是否 @ 全体） |
| `quote` | `message_id` |
| `image` | `url`，可选 `alt` |
| `audio` / `video` | `url` |
| `file` | `url`，`name` |
| `face` | `platform`，`id` |
| `opaque` | `platform`，`payload`（平台特有内容原样透传，核心不解释） |

元素表只增不改；加元素是兼容演进（[ADR-0004](../adr/0004-content-elements.md)）。

## 错误码

`bad_request`、`unauthorized`、`tool_not_allowed`、`room_not_subscribed`、`timeout`、`internal`。

工具失败一律用 `tool_result.error` 返回，不拆连接——只有协议本身坏了才断。
