# 钩子

钩子是部署者写在配置里的 Lua 脚本，在固定位置执行，可以读取或改变该位置的结果。

## 定位

钩子是**配置的一部分**，不是插件系统。

- 脚本来自 `config.toml` 或它引用的文件，由部署者手写。
- 不接受模型输出、群友消息、网络内容作为脚本来源。
- 没有安装、发现、签名、权限声明这些机制。

运行时是 Rust 内嵌的 Lua 5.4（`mlua`），不是 WebAssembly。理由见 [ADR-0003](../adr/0003-embed-lua-natively.md)。

## 沙箱边界

每个设备一个独立的 Lua 环境。环境里只有：

- 纯 Lua 标准库中无副作用的部分：`string`、`table`、`math`、`utf8`、部分 `os.time`/`os.date`。
- 注入的宿主函数（见下）。
- 该设备的只读配置快照。

**不加载**：`io`、`os.execute`、`os.remove`、`package`、`require`、`load`、`dofile`、`debug`。

这不是安全沙箱的完整保证——Lua 环境的隔离依赖 `mlua` 的实现正确性。但威胁模型里脚本作者就是部署者本人，所以这里的目标是**防止误用**，不是防止恶意脚本。

## 三个钩子位置

```mermaid
flowchart LR
    E["OneBot 事件"] --> F["身份过滤"]
    F --> P1["preReceive"]
    P1 --> AR["归档"]
    AR --> P2["postReceive"]
    P2 --> W["唤醒判定"]
    W --> P3["shouldWake"]
    P3 --> Q["入队投递"]
```

| 钩子 | 位置 | 输入 | 返回 | 能改变什么 |
| --- | --- | --- | --- | --- |
| `preReceive` | 身份过滤之后、归档之前 | 事件 | 事件 / `nil` | 丢弃事件、改写文本、加标签 |
| `postReceive` | 归档之后、判定之前 | 事件 + 归档增量 | 无 | 只读观察，或写入设备级状态 |
| `shouldWake` | 唤醒判定 | 事件 + 规则判定结果 | 原因字符串 / `nil` | 否决唤醒，或改写原因 |

三个钩子共享同一个 Lua 环境，因此可以跨调用保存状态（例如「这个人在最近 10 分钟内说过几次」）。

## preReceive

```lua
function preReceive(ev)
  -- ev.sender_id, ev.group_id, ev.text, ev.is_self, ev.tags
  if ev.text == "" then return nil end        -- 丢弃
  ev.tags = { "noisy" }
  return ev
end
```

返回 `nil` 表示丢弃。被丢弃的事件不归档、不判定、不唤醒。

**`is_self` 在钩子之前就已确定，钩子不能改写它。** 一个设备声明了多个账号时，这些账号发的话在 `preReceive` 里已经是 `is_self = true`。这是身份识别，不是可配置的规则——见 [不变量](07-invariants.md)。

## postReceive

```lua
function postReceive(ev, batch)
  -- batch.count, batch.oldest_at, batch.newest_at
  state.last_seen = ev.time
end
```

返回值被忽略。存在的意义是让脚本维护跨事件的状态，或记录统计。

## shouldWake

```lua
function shouldWake(ev, reason)
  -- reason 是设备规则算出的结果，可能是 nil
  if ev.sender_id == "30002" and reason == "any_message" then
    return nil                                -- 否决
  end
  if ev.text:find("上线") then
    return "keyword:上线"                     -- 改写原因
  end
  return reason
end
```

**这是「除某人外都唤醒」的实现位置。** 配置里的 `ignore_speakers` 覆盖常见情况；需要更复杂的判断（按时间段、按消息内容、按发言频率）时写在这里。

`shouldWake` 返回 `nil` 表示不唤醒；返回字符串表示唤醒，字符串作为原因标签传给适配器。

## 注入的宿主函数

| 函数 | 作用 |
| --- | --- |
| `log(level, msg)` | 写入内核日志，带设备标识 |
| `now()` | 当前 Unix 毫秒时间戳 |
| `random()` | `[0, 1)` 随机数 |

只有这三个。不提供发送消息、查询历史、访问网络的能力——钩子是判定与过滤，不是动作执行。需要动作的场合是工具，不是钩子。

## 失败处理

脚本抛错时：

- `preReceive` 抛错时事件按**保留**处理，记录错误日志。不因为脚本 bug 丢消息。
- `shouldWake` 抛错时视为**不否决**，按配置规则的结果继续。不因为脚本 bug 导致永眠。

两条都偏向「宁可多处理一次，不要静默丢弃」。这个方向是有意的：脚本 bug 的代价应该是噪音，不是失联。

## 加载时机

启动时一次性加载全部脚本。脚本语法错误导致启动失败，而不是运行期才暴露。

修改脚本需要重启内核——不做热重载。热重载会让「当前这一批消息用哪版规则」变得不确定，而判定逻辑的不确定性比重启代价更贵。

## 超时

每个钩子调用有硬超时（默认 100ms）。超时视为抛错，按上面的失败处理方向走。

脚本里没有阻塞原语，正常写法不会接近这个上限；超时主要防的是意外的死循环。
