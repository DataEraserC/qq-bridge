# 配置

唯一配置文件是 `config.toml`。五类顶层表：账号、人格、设备、适配器、笔记。

## 账号

```toml
[[accounts]]
id = "main"                      # 本文件内唯一的标识，不是号码
self_id = "10001"                # 这个账号在该平台上的号
adapter = "onebot11"             # 省略时默认 onebot11
platform = "qq"                  # 省略时 onebot11 取 qq
ws_url = "ws://127.0.0.1:6700"
token = "env:QQ_TOKEN"           # 支持 env: 前缀，避免明文入库
```

`id` 是你起的名字，工具调用和日志里都用它。

**账号身份是「平台 + 号码」，不是全球唯一号码。** `self_id` 只在该平台内唯一——`qq` 上的 `1` 与 `discord` 上的 `1` 是两个人。查重按 `(平台, 号码)` 进行。

`adapter` 决定用哪份 [`Transport`](../architecture/03-kernel.md#聊天传输) 实现解码与编码。它**不是平台名**：一个适配器可以服务多个平台，一个平台也可以有多个适配器。

| 值 | 说明 | 平台从哪来 |
| --- | --- | --- |
| `onebot11` | OneBot 11；动作与事件走同一条 WS | 配置声明，缺省 `qq` |
| `satori` | Satori；事件走 WS，动作走 HTTP `message.create` | 连上后由平台的 `login.platform` 给出，配置可省略 |

写错适配器名会在**启动期**失败并列出已知值——运行期才发现的表现是「连上了但没有事件」，那比启动报错难查得多。

`platform` 是会话的命名空间，决定两个 id 是否可比。OneBot 的帧里没有平台字段，所以它必须由配置声明（缺省 `qq`，旧配置不必改）；Satori 会在 `READY` 里告诉我们，写不写都行。给了的场合，内核拿它与平台自报的值核对。

## 人格

```toml
[[personas]]
id = "whale"
text = """你是一个……"""          # 或 file = "personas/whale.md"
```

`text` 与 `file` 二选一。多个人格可以引用同一个文件。

## 设备

设备是一份「自我」的配置。最小形态：

```toml
[[devices]]
id = "alice-phone"
persona = "whale"
memory = "alice"                 # 记忆名，两个设备填同一个即共享记忆
accounts = ["main", "backup"]    # 这个设备可用的账号集合

  [[devices.scopes]]
  conversation = "20001"         # 平台由 receive 的账号推出
  receive = ["main"]             # 从哪些账号收这个会话
  send = ["main", "backup"]      # 允许用哪些账号发

  [devices.wake]
  any_message = true
```

### 字段

| 字段 | 必填 | 说明 |
| --- | --- | --- |
| `id` | 是 | 设备标识，文件内唯一 |
| `persona` | 是 | 引用人格的 `id` |
| `memory` | 是 | 记忆名。相同即共享记忆 |
| `accounts` | 是 | 可用账号 `id` 集合 |
| `scopes` | 是 | 至少一个范围 |
| `wake` | 否 | 唤醒规则，缺省为只响应 @ 与名字 |
| `hooks` | 否 | Lua 脚本路径 |

### 范围

```toml
[[devices.scopes]]
conversation = "20001"
receive = ["main"]
send = ["main", "backup"]
```

`conversation` 是该平台内的会话标识，对群聊平台就是群号。**范围不写 `platform`**：它由 `receive` 里的账号推出——收信账号已经决定了是哪个平台，再写一个字段就有两个会互相矛盾的来源。

会话的身份是「平台 + 会话标识」两段。`qq` 群 `20001` 与 `discord` 频道 `20001` 是两个会话，各有一条未读水位；同一个设备可以两个都订阅，按 `(平台, 会话)` 去重。

`receive` 与 `send` 是两份独立列表：

- `receive`：从哪些账号的连接上收这个会话。**决定订阅，也决定平台。**
- `send`：回复时允许用哪些账号。**决定故障轮换，必须与 `receive` 同平台**——「收 QQ 的消息、用 Discord 账号发出去」会被启动期拒绝。

写成一份列表会让「主号掉线换备用号发」变成「连收也换到备用号」——而备用号可能不在这个群里。

`send` 里第一个可用账号是默认发送账号。工具调用可以显式指定，见 [唤醒规则](03-wake.md) 末尾。

### 冲突规则

**同一个 `(平台, 会话, 账号)` 只能被一个设备声明。** 启动时检查，冲突即失败：

```
错误：账号 main 在群 20001 上被两个设备声明
  - alice-phone
  - bob
请删掉其中一个设备的该范围，或改用不同账号。
```

这条不能关掉。冲突意味着同一个账号的同一份消息流进两个设备的记忆，两边各自回复——没有一种行为是你想要的。

不同账号可以订阅同一个群：

```toml
# 合法：alice-phone 用 main 收，alice-pc 用 backup 收同一个群
[[devices]]
id = "alice-phone"
memory = "alice"
  [[devices.scopes]]
  conversation = "20001"
  receive = ["main"]

[[devices]]
id = "alice-pc"
memory = "alice"
  [[devices.scopes]]
  conversation = "20001"
  receive = ["backup"]
```

## 共享记忆

两个设备填同一个 `memory` 即共享同一份记忆：

```toml
[[devices]]
id = "alice-phone"
memory = "alice"

[[devices]]
id = "alice-pc"
memory = "alice"
```

共享记忆的设备共享一条串行队列——它们的回合不会同时进入同一个会话。这是自动的，没有额外开关。

两个设备可以共享记忆但订阅完全不同的群；也可以订阅同一个群但用不同账号。模型里没有任何一条要求它们一致。

## 适配器

```toml
[adapter]
socket = "/run/user/1000/qqab.sock"   # 内核监听的接入点
turn_timeout_sec = 600               # 单回合上限，超时视为失败
token = "env:QQAB_ADAPTER_TOKEN"     # 跨机器时必填
```

`socket` 有三种写法，帧格式完全相同：

| 写法 | 形态 | 跨机器 |
| --- | --- | --- |
| `/run/user/1000/qqab.sock` | Unix 域套接字 | 否 |
| `\\.\pipe\qqab` | Windows 命名管道 | 否 |
| `local:qqab` | 平台命名空间（Linux 抽象命名空间 / Windows 管道） | 否 |
| `tcp://0.0.0.0:7000` | TCP | **是** |

Windows 部署必须显式写 `\\.\pipe\qqab` 或 `local:qqab`——缺省值是一个 Unix 路径，在 Windows 上不存在。

**跨机器必须配 `token`。** 监听非回环地址时，同网段任何人都能冒充适配器接管投递，所以内核在启动期就拒绝「TCP 监听非回环地址但没配 token」的组合。本机形态不强制：文件权限或命名空间隔离已经是边界。

令牌在 `hello` 帧里核对，配了就必须一致，否则连接被拒绝并断开。

## 笔记

```toml
[notes]
max_entries = 2000                   # 每记忆的笔记条数上限
digest_budget_tokens = 800           # 每回合注入的摘要预算，0 表示不注入
```

整节可省略，用上面的默认值。细节见 [笔记](05-notes.md)。

## 校验

内核启动时校验全部配置，任何一条不通过即退出，并指出具体位置与原因。不做「警告后继续」——配置错误在运行期表现为「某些群没反应」，比启动失败难查得多。
