# ADR-0013 harness 适配器：拆成「工具面」和「轮次驱动」两半，按 harness 的扩展点选形状

来源：用户裁定（harness 适配器可以用插件实现，也可以不用，按具体情况判断）+ 实现默认
（两半的拆法、opencode 走插件、会话按设备一份，都是我自己定的，可以推翻）。

## 这条决定了什么行为

- 一个 harness 适配器要干两件独立的事，分别衡量：
  1. **工具面**：让模型看见桥上的九个工具，把调用原样转发成 `tool_call`。
  2. **轮次驱动**：把 `turn_start` 里的记录变成 harness 里的一次「对话」，跑完回 `turn_end`。
- 这两件事各有各的实现手段，所以「接一个新 harness」不是一种写法，而是从下面这张表里挑：

  | 能力 | 手段 | 需要 harness 有 |
  |------|------|-----------------|
  | 工具面 | 插件自定义工具 | 插件 / 扩展 API |
  | 工具面 | MCP server | 支持 MCP（多数都有） |
  | 轮次驱动 | 进程内：插件里起后台循环，反调 harness 的会话接口 | 插件 API + 能反向驱动会话 |
  | 轮次驱动 | 进程外：桥旁边跑个小程序，用无头/HTTP/CLI 驱动 | 无头模式或服务端 API |

- 只做到工具面的，算**半接**：能发不能收，桥推过来的轮次没人跑。这种情况要在文档里
  写明，不能让人以为接完了。
- 一个 harness 有插件 API 就优先走插件：工具面和轮次驱动都在同一个进程里，最跟手，
  也不用额外起进程。没有插件 API 才退到进程外驱动或 MCP。

## 不这样会怎样

如果按「一个 harness 一份完整实现」来想，每加一个 harness 都要重想一遍架构，而且看不出
哪一半缺了——接完发现只能发不能收，或者只能收不能发，都是事后才发现。拆成两半之后，
缺口在接入第一天就显形。

反过来，如果一律走进程外驱动（比如全塞 MCP 加无头模式），插件类 harness 就白丢了自己
最好用的能力：进程内能拿到会话、能拿到事件、能等一轮真正跑完。opencode 这种有插件的，
用外部驱动去猜「它跑完了没有」纯属自找麻烦。

## 技术细节

- opencode 选**插件型**，代码在 `harnesses/opencode/`：
  - `plugin/qqab-wire.mjs`：协议客户端 + 投递渲染。不依赖 opencode，可以单独测
    （15 个测试，对着一个假桥跑——假的是桥，不是平台）。
  - `plugin/index.js`：插件本体（**v2 API**，`export default { id, setup(ctx) }`）。
    用 `ctx.tool.transform` 注册九个工具（参数是 JSON Schema，不是 zod）；起一个后台连接，
    收到轮次就 `ctx.session.prompt` 排话、`ctx.session.wait` 等这一轮说完，再回 `turn_end`。
- **会话按设备一份**：标题 `qqab:<设备>`，id 存在 `ctx.storage` 里，重开能接上；每次用前
  确认它还活着。会话就是设备的记忆（ADR-0002），重开 harness 不能失忆。options 里的
  `session` 可以钉死。
- **工具只在轮次内可用**：没有票的调用直接以 `unauthorized` 拒掉。这样人在 TUI 里手动
  跟同一个实例聊天时，也不会把话发到别人的房间里去。
- **失败也回 `turn_end`**：`prompt` 抛错、会话被删、连接断了，都带着错误收工，不能让设备
  一直占着——桥那边是靠 `turn_end` 或断连释放设备的。
- **验证进度**：2026-10-29 真机上整轮跑通了——真 QQ 发消息 → 开轮 → 模型调
  `await tools.send_message(...)`（经 opencode 的 `execute`）→ 消息真的到 QQ → 再 `ack_messages`
  推水位；私聊和群各一轮。插件逻辑另有 8 个假宿主测试兜底。详见
  [ADR-0016](0016-opencode-v2-plugin.md) 与 `docs/06`。

## 参考

- 分家依据、各家 harness 的判断、怎么验：[`docs/06-harness-adapters.md`](../06-harness-adapters.md)
- opencode 用哪套插件 API：[`ADR-0016`](0016-opencode-v2-plugin.md)
- 帧的形状：[`docs/04-protocol.md`](../04-protocol.md)
