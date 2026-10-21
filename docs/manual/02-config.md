# 配置

唯一配置文件是 `config.toml`。五类顶层表：账号、人格、设备、适配器、笔记。

## 账号

```toml
[[accounts]]
id = "main"                      # 本文件内唯一的标识，不是 QQ 号
self_id = "10001"                # 这个账号的 QQ 号
ws_url = "ws://127.0.0.1:6700"
token = "env:QQ_TOKEN"           # 支持 env: 前缀，避免明文入库
```

`id` 是你起的名字，工具调用和日志里都用它。`self_id` 必须与 OneBot 实现实际登录的号一致——不一致时内核无法正确判断「自己的消息」。

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
  group = "20001"
  receive = ["main"]             # 从哪些账号收这个群
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
group = "20001"
receive = ["main"]
send = ["main", "backup"]
```

`receive` 与 `send` 是两份独立列表：

- `receive`：从哪些账号的连接上收这个群。**决定订阅。**
- `send`：回复时允许用哪些账号。**决定故障轮换。**

写成一份列表会让「主号掉线换备用号发」变成「连收也换到备用号」——而备用号可能不在这个群里。

`send` 里第一个可用账号是默认发送账号。工具调用可以显式指定，见 [唤醒规则](03-wake.md) 末尾。

### 冲突规则

**同一个 `(群, 账号)` 只能被一个设备声明。** 启动时检查，冲突即失败：

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
  group = "20001"
  receive = ["main"]

[[devices]]
id = "alice-pc"
memory = "alice"
  [[devices.scopes]]
  group = "20001"
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
socket = "/run/user/1000/qqab.sock"   # 内核监听，适配器连接
turn_timeout_sec = 600               # 单回合上限，超时视为失败
```

## 笔记

```toml
[notes]
max_entries = 2000                   # 每记忆的笔记条数上限
digest_budget_tokens = 800           # 每回合注入的摘要预算，0 表示不注入
```

整节可省略，用上面的默认值。细节见 [笔记](05-notes.md)。

## 校验

内核启动时校验全部配置，任何一条不通过即退出，并指出具体位置与原因。不做「警告后继续」——配置错误在运行期表现为「某些群没反应」，比启动失败难查得多。
