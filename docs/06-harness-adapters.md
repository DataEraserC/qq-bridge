# 06 · harness 适配器

> 状态：opencode 那一份按 **v2 API** 写完了，协议层与插件逻辑都有测试；**还没在真的
> opencode 里跑完一轮**——本机 opencode2 2.0.22（Bun 1.4.2）加载任何插件都会段错误，
> 连 4 行空插件都崩。详见下面「卡在哪」。
> 原则：桥不懂 harness 的内脏，harness 也不懂平台细节。适配器是那个翻译。

## 一个适配器要干两件事

这两件事互不依赖，可以分别做到、分别缺席：

1. **工具面**：让模型看见桥上的九个工具（发消息、确认水位、翻历史、记笔记……），
   模型一调，就原样变成一帧 `tool_call` 发给桥。
2. **轮次驱动**：桥推来一轮（`turn_start`，带着这个房间的未读记录），把它变成 harness 里
   的一次对话，跑完回 `turn_end`。

```mermaid
flowchart LR
    P[聊天平台] <--> B[桥]
    B -->|turn_start：票 + 房间 + 记录| A[harness 适配器]
    A -->|tool_call：发消息 / 确认 / 翻历史| B
    A <-->|harness 自己的接口| H[harness 进程]
```

只有工具面、没有轮次驱动的，叫**半接**：能发不能收，桥推过来的活没人干。这种事要在
文档里写清楚，不能让人以为接完了。

## 两半各有各的手段

| 能力 | 手段 | 需要 harness 有 | 代价 |
|------|------|-----------------|------|
| 工具面 | 插件里注册自定义工具 | 插件 / 扩展 API | 跟 harness 版本绑得紧 |
| 工具面 | 挂一个 MCP server | 支持 MCP（多数都有） | 多一层进程，桥要额外实现 MCP |
| 轮次驱动 | 进程内：插件里起后台循环，反调会话接口注入一条消息，等它回完 | 插件 API，且能反向驱动会话 | 可能和 harness 自己的事件派发打架 |
| 轮次驱动 | 进程外：桥旁边跑个小程序，用无头模式 / HTTP / CLI 驱动 | 无头模式或服务端 API | 慢；会话续接、结束判定都要自己猜 |

结论：**有插件 API 就走插件**，工具面和轮次驱动都在同一个进程里，最跟手；没有插件
API 才退到进程外驱动或 MCP。这条判断写进了 [ADR-0013](adr/0013-harness-adapter-shape.md)。

## 拿到一个新 harness，先问四句

1. 能不能注册自定义工具？（决定工具面走插件还是 MCP）
2. 能不能从扩展点里反过来驱动一次会话，并且知道它什么时候结束？（决定轮次驱动走进程内还是进程外）
3. 没有插件的话，有没有服务端 / 无头模式？能不能接着上一次的会话继续？
4. 上面都不行？那就只挂 MCP，算半接，文档里标明「只能发不能收」。

## 当前各家的情况

下面「判断」一栏里，只有 opencode 是我看过文档、写过代码的；其余是**待核实**，别当结论用。

| harness | 扩展点 | 轮次驱动 | 判断 |
|---------|--------|----------|------|
| opencode | 插件（v2：`export default { id, setup(ctx) }`；工具用 `ctx.tool.transform` 注册） | 进程内：`ctx.session.prompt` 排话 + `ctx.session.wait` 等它说完 | 插件型，已按 v2 实现，见 `harnesses/opencode/plugin/`；真机跑到一半被 Bun 崩溃挡住 |
| DSH | 插件 | 待核实 | 用户说可以走插件，具体接口未看 |
| WorkBuddy | 待核实 | 待核实 | 未看 |
| Trae | 待核实 | 待核实 | 未看 |
| Codex | 无插件；有 `codex exec` 这类无头入口 | 进程外：每轮起一次 | 大概率进程外 + MCP 补工具面 |
| OpenClaw | 待核实 | 待核实 | 未看 |

## opencode 这一份怎么用

opencode 有 v1 和 v2 两套插件 API，**写法不通用**。我们按 v2 写（本机能装到的是 v2）。

v2 的插件是一个目录：`index.js` 是入口，`qqab-wire.mjs` 是协议客户端（入口 import 它）。

```bash
# 整体拷进 opencode 的插件目录，目录名随便，这里叫 qqab
cp -r harnesses/opencode/plugin ~/.config/opencode/plugins/qqab
```

配置两种给法，选项优先于环境变量：

```jsonc
// ~/.config/opencode/opencode.jsonc
{
  "plugins": [
    { "package": "/home/me/.config/opencode/plugins/qqab", "options": { "token": "设备令牌" } }
  ]
}
```

```bash
export QQAB_TOKEN=<设备令牌>         # 和 config.toml 里设备的 token 一致
export QQAB_ENDPOINT=127.0.0.1:7700  # 可选，默认就是这个
```

没给令牌时插件直接不启用，只写一条警告——不阻塞 opencode 启动。

几个刻意的做法：

- **会话按设备一份**，标题 `qqab:<设备>`。会话 id 记在插件自己的 storage 里，重开接上，
  找不到才新建；每次用前确认它还活着（用户在 TUI 里删了会话也能自愈）。
  会话就是设备的记忆（[ADR-0002](adr/0002-subscription-model.md)），重开 harness 不能失忆。
- **`prompt` 排话、`wait` 等它说完**，两个连起来才是一轮。v2 的 `prompt` 只保证把话排进
  会话，模型随后才跑；只看 `prompt` 返回就收工，会把票提前作废，工具调用全成
  `unauthorized`（假宿主测试里踩过这个）。
- **工具只在轮次内可用**。没有票的调用一律 `unauthorized`。这样人在 TUI 里手动跟同一个实例
  聊天，也不会把话发到别人的房间里。
- **失败也回 `turn_end`**。`prompt` 抛错、会话被删、连接断了，都带着错误收工——桥靠
  `turn_end` 或断连释放设备，不回就一直占着。
- **断线自己重连**，退避 1 秒起、最长 10 秒。桥比 opencode 晚启动也能接上。

### 写 v2 插件绕不过去的四个坑（都是实测）

| 现象 | 原因 / 做法 |
|------|-------------|
| 插件放进去没反应，日志里连「loading plugin」都没有 | `.opencode/plugins/` 只自动发现**直接的 `.js` / `.ts` 文件**和**插件目录**；`.mjs` 文件不认。入口用 `index.js` |
| `Cannot find package '@opencode/plugin'` | 本地插件不自动带这个包。不用它就行：直接 `export default { id, setup }`，功能一样（`Plugin.define` 只是语法糖） |
| 插件一加载，opencode 报 `Transport: The socket connection was closed unexpectedly` | 插件里 `console.log` 写到了 **stdout**，而 `serve --stdio` 用 stdout 当 RPC 通道，协议被污染。日志只能走 `ctx.app.log` 或 `console.error` |
| 配置写 `"plugin": ["./qqab.js"]` 报 `configured plugin path must be a directory` | v2 的键叫 `plugins`（复数），且本地路径必须是**目录**，指向单个文件会被跳过 |

## 怎么验

四层，越往下越真：

```bash
# 1. 协议层：假桥，验握手 / 开轮 / 工具配对 / 错误码 / 渲染（15 个）
node --test harnesses/opencode/test/wire.test.mjs

# 2. 插件逻辑：假桥 + 模拟的 v2 宿主，验工具注册 / 一轮的完整动作 / 会话复用 / 卸载（8 个）
node --test harnesses/opencode/test/plugin.test.mjs

# 3. 对着真桥（不用 opencode）：握手成不成
node harnesses/opencode/probe.mjs --token <设备令牌>

# 4. 真链路：真 opencode + 真桥 + 真 QQ（目前被上游 bug 挡住，见下）
```

第 3 步里 `probe.mjs` 会把投递的内容打出来、回一句话、确认水位。它和插件用的是同一个
协议客户端，所以这里看得到的，插件也看得到——区别只在「交给谁去思考」。

## 卡在哪：opencode2 加载插件就段错误

本机 `opencode2 2.0.22`（内嵌 Bun 1.4.2）只要加载插件，`serve --stdio` 进程就崩：

```
panic: Segmentation fault at address 0x...
oh no: Bun has crashed. This indicates a bug in Bun, not your code.
```

最小复现是一个 4 行插件，什么都不做：

```js
// .opencode/plugins/x.js
export default { id: "x", async setup() {} }
```

同一个目录里没有插件时 `opencode2 run` 一切正常，放进去就崩。所以这不是我们的代码问题，
是上游。等它修好，第 4 层才算验过；在那之前，插件逻辑靠第 2 层（假宿主）兜着。

## 已知没验的地方

- **真 opencode 里的整轮**：被上面的 Bun 崩溃挡住。具体没验到的是「`prompt` 之后工具调用
  是否真能拿到票」「工具 schema 它认不认」。假宿主能证明我们的顺序对，不能证明它的行为。
- 内容段参数用宽松的 `items: { type: "object" }` 接，靠描述文字教模型写元素。宁松勿严：
  写错了桥那边会给出比 JSON Schema 更准的话。
- `ctx.session.create` 的返回形状按文档写（`id`），另外兼容 `sessionID` / `session.id` /
  `data.id` 三种老形状；opencode 换版本改形状时，这里要跟着看。
