# 钩子脚本

钩子是写在配置里的脚本，可以改变唤醒判定与消息处理。默认用 Lua，也可以用任何语言写成常驻进程。设计说明见 [架构：钩子](../architecture/06-hooks.md)，本文只讲怎么写。

## 启用

```toml
[[devices]]
id = "alice-phone"
# ...
hooks = "hooks/alice.lua"
```

脚本在启动时加载。语法错误会导致启动失败，不会运行期才暴露。修改后需要重启内核。

## 用别的语言写：外进程钩子

`HookRuntime` 是接口，Lua 只是默认实现（[ADR-0010](../adr/0010-hook-runtime-is-an-interface.md)）。要换语言就把脚本写成一个**常驻进程**，与内核按行交换 JSON。

协议：

```text
→ {"hook":"preReceive","event":{...}}
← {"drop":false,"text":null,"tags":[]}
```

三个方法对应三个 `hook` 值：

| hook | 响应 |
| --- | --- |
| `preReceive` | `{"drop":bool,"text":string|null,"tags":[]}`；`text` 为 `null` 表示不改正文 |
| `postReceive` | 任何 JSON，忽略 |
| `shouldWake` | `{"wake":bool,"reason":"..."}`；只有 `wake:true` 才唤醒 |

**必须每条响应都 flush。** 不 flush 的表现是「钩子永远不返回」，超时后进程被杀。

**必须常驻。** 实测每次重起进程约 12.4 ms，常驻逐行 IPC 约 0.031 ms——差 400 倍。钩子是每条消息都要过一遍的路径。

**超时的后果与 Lua 不同。** 外进程超时会被 `kill`，下一次调用要重起（付一次启动成本）；Lua 只是这一次调用失败，环境还在。

## 可用的东西（仅 Lua 实现）

三个钩子函数、三个宿主函数、一份只读配置快照。下面这些是 Lua 环境独有的；外进程实现里语言有什么能力由你自己决定。

### 宿主函数

| 函数 | 说明 |
| --- | --- |
| `log(level, msg)` | 写日志，`level` 取 `"debug"`/`"info"`/`"warn"`/`"error"` |
| `now()` | 当前 Unix 毫秒时间戳 |
| `random()` | `[0, 1)` 随机数，种子来自设备 id，故跨启动可重放 |

没有 `io`、`os.execute`、`require`、`load`、`loadfile`、`dofile`、`debug`、`collectgarbage`。钩子只做判定与过滤，不做动作——需要发消息的场合是工具，不是钩子。

### 配置快照

`config` 是这台设备的只读副本：

| 字段 | 说明 |
| --- | --- |
| `config.id` | 设备 id |
| `config.memory` | 记忆名 |
| `config.persona` | 人格 id |
| `config.accounts` | 可用账号 id 数组 |
| `config.scopes` | 数组，每项含 `group`、`receive`、`send` |

改它当场抛错。`rawset` 仍能绕过——目标是防误改，不是防恶意脚本。

### 资源上限

每次调用最多 100ms 墙钟、两万条 VM 指令、4 MiB 内存。超限按抛错处理，方向见 [架构：钩子](../architecture/06-hooks.md#超时与资源上限)。

## preReceive：消息进归档之前

```lua
function preReceive(ev)
  -- ev.sender_id  string   发言者 QQ 号
  -- ev.group_id   string   群号
  -- ev.text       string   纯文本
  -- ev.time       number   Unix 毫秒
  -- ev.is_self    boolean  是否为设备自己账号发出
  -- ev.tags       table    可变，附加标签
  return ev        -- 返回 nil 表示丢弃
end
```

### 丢弃空消息与纯表情

```lua
function preReceive(ev)
  if ev.text == "" then return nil end
  return ev
end
```

### 给消息打标签，供后续判定使用

```lua
function preReceive(ev)
  if ev.text:find("^/") then
    ev.tags = { "command" }
  end
  return ev
end
```

`is_self` 在钩子执行前就已确定，**不能改写**。这是身份识别，不是规则。

## postReceive：归档之后

```lua
function postReceive(ev, batch)
  -- batch.count     number  本批消息条数
  -- batch.oldest_at number
  -- batch.newest_at number
end
```

返回值被忽略。用于维护跨事件状态或记录统计：

```lua
local seen = {}

function postReceive(ev, batch)
  local key = ev.sender_id
  seen[key] = (seen[key] or 0) + 1
  if seen[key] == 20 then
    log("info", "这个人在本进程内已发 20 条")
  end
end
```

## shouldWake：唤醒判定

```lua
function shouldWake(ev, reason)
  -- reason  string|nil  配置规则算出的结果
  return reason           -- 返回 nil 否决；返回字符串则作为原因
end
```

### 除某人外都唤醒

配置里的 `ignore_speakers` 覆盖常见情况。需要更复杂的条件时：

```lua
function shouldWake(ev, reason)
  -- 深夜只响应 @
  local hour = tonumber(os.date("%H", ev.time / 1000))
  if hour >= 2 and hour < 8 and reason ~= "at_mention" then
    return nil
  end
  return reason
end
```

### 按内容否决

```lua
function shouldWake(ev, reason)
  -- 刷屏的单字回复不唤醒。按**字符数**而不是字节数：
  -- Lua 的 # 返回字节数，一个汉字占 3 字节，用 # 永远判不到中文短句。
  local chars = utf8.len(ev.text) or #ev.text
  if chars <= 2 and reason == "any_message" then
    return nil
  end
  return reason
end
```

### 改写原因

```lua
function shouldWake(ev, reason)
  if ev.text:find("上线") then
    return "keyword:上线"
  end
  return reason
end
```

原因字符串会传给适配器，可用于在提示里区分「被叫到」和「随机冒泡」。

## 失败处理

| 情况 | 行为 |
| --- | --- |
| `preReceive` 抛错 | 事件**保留**，记错误日志 |
| `shouldWake` 抛错 | **不否决**，按配置规则的结果继续 |
| 单次调用超时（100ms） | 按抛错处理 |

方向是「宁可多处理一次，不要静默丢弃」。脚本 bug 的代价应该是噪音，不是失联。

## 完整示例

```lua
-- 白天宽松，深夜只认 @；刷屏短句不唤醒
function preReceive(ev)
  if ev.text == "" then return nil end
  return ev
end

function postReceive(ev, batch)
  log("debug", string.format("批 %d 条，来自 %s", batch.count, ev.sender_id))
end

function shouldWake(ev, reason)
  if reason == nil then return nil end

  local hour = tonumber(os.date("%H", ev.time / 1000))
  if hour >= 2 and hour < 8 then
    if reason == "at_mention" then return reason end
    return nil
  end

  -- 同样是字符数：`#` 数的是字节，中文按字节算永远过不了这个阈值。
  local chars = utf8.len(ev.text) or #ev.text
  if chars <= 2 and reason ~= "at_mention" then
    return nil
  end

  return reason
end
```
