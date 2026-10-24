# 内核与聊天协议之间要有 Transport 边界

内核用 `onebot.rs` 直接把 OneBot 11 的字段写进了自己的类型：`post_type`、`notice_type`、`sub_type`、`group_id`、消息段数组。这是**不对称**的——agent 那一侧早就抽象成了适配器，QQ 这一侧从来没抽象。

## 决定

在 `crates/kernel/src/chat.rs` 定义 `Transport` trait 与一套中立类型，内核只认它们。

```rust
trait Transport {
    fn name(&self) -> &'static str;
    fn decode(&self, line: &str) -> Result<ChatEvent, TransportError>;
    fn encode(&self, action: &ChatAction, id: &str) -> String;
    fn response(&self, line: &str) -> Option<(Option<String>, ChatOutcome)>;
}
```

中立事件只有四个分支：`Message`、`Poke`、`Status`、`Ignored`。动作只有一个：`SendGroupMessage`。

`onebot.rs` 降级成一个实现（`OneBot`），`satori.rs` 是第二个。配置里每个账号写 `protocol = "onebot11" | "satori"`。

## 为什么抽象在这里成立

写第二个实现之前先量了一下内核到底需要什么，结论是**非常少**：

| 内核要的 | 不要的 |
| --- | --- |
| 群号与会话 | 消息段数组（已拍平成文本） |
| 发送者身份与昵称 | `post_type` 之类的分类字段 |
| 已拍平的正文 | 私聊、频道、临时会话 |
| 是否 @ 到自己 | 撤回、禁言、传文件 |
| 消息 ID 与时间 | 平台特有的扩展字段 |
| 会话里的拍一拍 | 心跳里的链路自评 |

所以协议的差异全部落在实现里。Satori 与 OneBot 的差异——信令封装、`channel.type == DIRECT`、`guild.id` 对 `group_id`、`<at>` 元素对消息段、WS 单向对动作同连接——每一条都只出现在 `satori.rs` 里，内核零改动。

## 考虑过的方案

**一个「协议无关」的通用事件模型，带字段全集。** 每个平台填自己的部分，缺的留空。这会让内核开始按字段是否存在分岔——比按协议名分岔更糟，因为分支散落在每个消费点上，而不是集中在一个文件里。

**只做 OneBot，等真有第二个需求再抽象。** 这条对，但「真有第二个」已经发生了：NoneBot、Koishi、Satori 都在做同一件事，而它们的共同点是**都不长成 OneBot 的样子**。

**让 Transport 负责建连。** 没这么做。Satori 的动作走 HTTP、事件走 WS，连接管理与协议是两件事；现在 `transport.rs` 持有 `Box<dyn Transport>` 只用来编解码，建连仍由它自己管。混在一起会让「换个协议要不要重写重连逻辑」变得没法回答。

## 后果

- **`ChatOutcome` 不承诺 `data`。** OneBot 的动作响应里有 `data`（带 `message_id`），Satori 的 HTTP 响应形状不同。中立层只给「成功/失败+原因」，需要 `message_id` 的工具得各自想办法。这是对抽象的诚实让步，不是疏漏。
- **Satori 的 HTTP 侧没实现。** `encode` 产出的是一条待发 HTTP 请求的描述（`{"path":..., "body":..., "id":...}`），真正的 HTTP 客户端留给传输层。一个协议同时要 WS 与 HTTP 时，抽象只能容纳这个形状，不能假装它是一条 WS 帧。
- **拍一拍在 Satori 上不存在。** 没做硬映射：把一个不存在的事件映射过去，会让「拍一拍唤醒」在两个协议上语义不同。Satori 上它落到 `Ignored`。
- **新增一个协议要动的只有两处**：`KNOWN_PROTOCOLS` 加一个名字，`main.rs` 的 `transport_for` 加一个分支。后者是唯一没法抽象掉的地方——它得知道具体类型才能 `Box::new`。
- 未知协议名在**配置校验期**就失败，不留到运行期。`protocol = "onebot1l"` 这种笔误在运行期表现为「连上了但没有事件」，比启动失败难查得多。
