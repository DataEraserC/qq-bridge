# 会话身份是「平台 + 会话」，不是群号

内核从 OneBot 时代带来的假设是「会话就是一个 QQ 群号」。这个假设在四个地方变成了比较一个**没有命名空间**的值，每一处都能静默出错：

| 位置 | 原来 | 后果 |
| --- | --- | --- |
| `conn.rs` `device_is_sender` | 只比 QQ 号 | Discord 上有个用户恰好等于我们某个 QQ 账号的号，他被当成「自己发的」——消息不进未读、不唤醒 |
| `config.rs` `seen_self` | 全局唯一 | 两个平台用同一个数字号码的账号**配置期就被拒**，部署起不来 |
| `archive.rs` `watermarks` | 按裸群号 | QQ 群 `123` 与 Discord 频道 `123` 在同一个记忆名下共用一条水位，一边投递把另一边划掉 |
| `wake.rs` `ignore_speakers` | 裸号码集合 | 忽略某个平台的某人会顺带忽略另一个平台同号的人 |

另外，配置字段 `protocol` 把**命名空间**和**实现**混成了一个词。

## 决定

1. **会话键是两段**：`ConversationKey { platform, conversation }`，逐段比较，不拼成字符串。
2. **回复目标独立于会话**：`ReplyAddress { platform, address }`。同形但不同类型。
3. **平台名是传输层的事实，不是配置的偏好**（除 OneBot 外——它的帧里没有平台字段，只能由配置声明）。
4. **配置字段改名**：`accounts[].protocol` → `accounts[].adapter`（实现名）；`devices[].scopes[].group` → `conversation`（会话标识）。范围不单独写 `platform`，由它的 `receive` 账号推出并校验一致。
5. **`adapter` 不进会话键。**

## 为什么平台与实现是两个词

Satori 自己的文档就是分开的：`login.platform` 是命名空间，`login.adapter` 是实现。两者是多对多的：

- 一个 adapter 可以服务多个 platform（自托管 Satori 服务器同时挂 Discord、KOOK、Telegram）。
- 一个 platform 可以有多个 adapter（官方与非官方的 OneBot 实现）。

所以：

| 概念 | 我们的字段 | 用途 |
| --- | --- | --- |
| 命名空间 | `platform` | 进会话键，决定两个 id 是否可比 |
| 实现 | `adapter` | 选哪个 `Transport`，只用于配置校验与日志 |

**`adapter` 绝不能进会话键。** SnowLuma 与 Satori 同时指向同一个 QQ 群时，它们是**同一个**会话；把实现放进键会让同一场对话被拆成两份记忆。

## 为什么 `ReplyAddress` 与会话分开

Satori 里一个 guild 可以有多个文本频道：消息的**会话**是 `guild.id`（内核的范围模型是「一个群一个订阅」），而回复要打到 `channel.id`。一个类型表达不了两个值。多数平台上两者相等，那时 `address` 等于 `conversation`，`reply` 字段省略。

## 为什么范围不写 `platform`

收信的账号已经决定了是哪个平台，再写一个字段就有两个能互相矛盾的来源。所以：

- `platform_of_scope` 从 `receive` 里的账号推出平台；
- 混用平台报 `ScopeMixedPlatform`；
- `send` 里的账号必须是同一个平台，否则报 `ScopeSenderWrongPlatform`——「收 QQ 的消息、用 Discord 账号发出去」是最坏的「能跑但发错地方」。

`[[devices.scopes]]` 按 `(平台, 会话)` 去重而不是按群号：`qq` 群 `123` + `discord` 频道 `123` 是合法配置，只按群号去重会让它根本写不出来。

## 平台从哪来：两种协议两种答案

**OneBot：配置声明。** 帧里只有 `self_id`、`group_id`，没有一个字说这是哪个平台。缺省 `qq`，旧配置不必改。

**Satori：`READY` 的 `login.platform`。** 这是「平台是传输层的事实」的完整形态。

代价要明说：`READY` 之前不知道平台，那段窗口里解出的消息**被丢弃**（`message:platform_unknown`）。猜一个平台名会把消息归进错误的命名空间，比丢一条更难查。协议没给 `login.platform` 时也不猜，一直丢。

这个代价的另一半是：启动期无法校验 `scopes[].conversation` 是否指向存在的会话，配置写错要等「这个群没反应」。可接受，因为**静默丢弃会留日志**，且真正的兜底是范围来自配置、不是来自平台。

## 考虑过的方案

**把会话键拼成 `"qq/20001"` 字符串。** 最省事，但把「分隔符不出现在 id 里」变成一条没人检查的隐式约定，而且相等判断退化成字符串相等。两段结构化没有这个问题。

**让 platform 由配置完全声明（包括 Satori），运行期只校验。** 能换回启动期校验，但限制了自托管 Satori 的合法用法——一台服务器上的两个 bot 可以是不同 platform。

**给工具加一个「指定哪个会话」的参数。** 明确拒绝。模型在同一次对话里只看到一个会话的内容，给它一个选择目标的口径，它唯一能做的事是猜——猜错就是把 A 群的话发进 B 群。会话身份是**投递信息**（模型只能读），不是工具参数。

**`ScopeConfig` 存 `ConversationKey` 而不是两个字段。** 会让配置结构体依赖一个「传输层的类型」，且序列化时多一层嵌套。两个平铺字段加一个 `key()` 方法足够。

## 后果

- **协议主版本升到 2。** `scope.group` → `scope.platform` + `scope.conversation`，`Deliver` 新增可选 `reply`。主版本不同即拒绝，不尝试兼容；当前没有落盘的数据要迁移（`Archive`/`NoteStore` 的 JSON 往返只被测试调用，生产路径不写盘）。
- **归档水位在线上是数组**：`watermarks: [{platform, conversation, through}]`。JSON 的对象键只能是字符串，两段键做不了 map 键——拼成字符串就退回分隔符撞车的老问题。
- **钩子看到两段**：`ev.platform` + `ev.conversation`（事件视图），`config.scopes[i].platform` + `.conversation`（配置视图）。原来只有 `ev.group_id`。
- **`ChatStatus::Ready` 不再是单元变体**，带 `platform: Option<String>`，因此丢掉 `Copy`。
- **`AdapterEvent::Frame` 的 `frame` 装箱**：`Deliver` 变大之后枚举按最大变体分配内存，每条拍一拍都跟着付这份开销。
- 新增一个平台**不需要新协议**：OneBot 换个 `platform` 配置值即可；Satori 不用动。
