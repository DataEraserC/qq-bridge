# ADR-0019 一个 harness 连接内多会话并行 + 单套 MCP 工具面

来源：用户裁定（2026-10-30 对话敲定；本文标「用户裁定」的句子出自用户原话或明确选择，
其余为实现默认，可推翻）。

## 这条解决了什么问题

真机实测 dsh 时发现一个 bug：dsh 侧模型经常先调 `mcp__acb-group2__list_rooms` 报
`unauthorized`（「现在没有进行中的轮次，工具调不动」），换 `mcp__acb__list_rooms` 才成功。

查证源码后定位的真根因（2026-10-30 修正，比初稿的「单值票串行」更准确）：

1. **dsh 侧两套 MCP 没按 session 隔离**（真根因）。配置里挂了 `mcp-acb`（管 `dsh` 会话）和
   `mcp-acb-group2`（管 `dsh-group2` 会话）两个 MCP server，但两个都挂在了**同一个 dsh
   session / 全局 scope** 上，于是模型面前同时摆着 `mcp__acb__*` 和 `mcp__acb-group2__*`
   两套工具。模型不知道当前对话属于哪个 acb 会话，只能试错：先调 group2 那套 → 没有该
   会话的票 → unauthorized，再换 acb 那套 → 成功。
2. **桥侧 `Conn.ticket` 是连接级单值**（次生问题，已改）。虽然当前每个 session 一个独立
   token、一条连接只绑一个 session，单值票在现状下够用；但它限制了「一条连接同时跑多个
   会话」的能力——而这是 ADR-0017 已定「不同会话各自并发」目标里没落地的一块。改成
   per-session map 后，一条连接也能承载多个会话并行，是通用能力补完。

用户裁定要的是：**一个 harness 会话可以处理多个订阅房间的消息，会话内部串行、不同会话
之间并行**，并且**模型不该面对两套不知道该选哪套的工具**。

## 目标模型（用户裁定）

- 一个 acb 会话（session）= 一个串行单元，订阅多个房间；同一会话内部严格串行，一轮只投
  一个房间的未读（这跟 ADR-0017 一致，不变）。
- **不同 acb 会话各自并发、互不排队**（ADR-0017 第 47 行原话，本就是已定目标）。
- 一个 harness 连接（driver）可以同时跑**多个会话**的轮次，各会话并行。
- **一套 MCP 工具面**：一个 driver 只暴露一份 `mcp__<name>__*`，模型不用猜哪套有票。

> 2026-10-30 用户口头勘误：用户原话先说「一个 session 对应一个 harness 会话」，后改口
> 「我要的是 session 间并行/并发」。两者不矛盾——「一个 acb session = 一个 harness 侧
> session」是映射关系（一对一，本 ADR 术语澄清已定），而「并行」是**多个这样的 session
> 各自并发跑**。勘误只澄清了「并行」的诉求，不改映射。

## 术语澄清（用户裁定，务必记牢）

「一个 session 对应一个 **harness 会话**」里的「harness 会话」= **harness 侧的 session**，
而且它**不跟某个具体 harness 绑定**——取决于接的是哪个 harness：

- dsh harness → dsh 侧的一个 session（固定 id `session-acb-<id>`）。
- opencode harness → opencode 的一个 session。
- 其它未来 harness（Trae / Codex / OpenClaw 等）→ 各自 harness 侧的一个 session。

也就是说：

- acb 的 `session`（配置里那一个串行单元，比如 `dsh`、`dsh-group2`）↔ 一个 harness 侧的
  session，**一对一**。harness 侧 session 的具体形态由 harness 自己决定，桥不定义它长啥样，
  只知道「这是 harness 侧的一个串行单元」。
- 不是「一个 session 对应一个 driver 进程 / 一个 MCP server」。driver 进程是**承载**这些
  session 的运行单元，一个 driver 可以承载多个 acb session，每个 acb session 再各自映射到
  一个 harness 侧 session。

这跟 ADR-0017 的「harness 会话」概念一致：harness 会话（harness 侧的 session）是一个串行
单元，一个 harness 进程连上来声明「接管哪些会话 id」。区别只在于本 ADR 把它推进到「一条
连接能**同时**跑多个 session」，而不是一次一个。这个「harness 侧 session」的跨 harness 抽象
正是桥做「多 harness × 多聊天平台」桥接时不能丢的东西——core 只承载「acb session ↔ harness
侧 session」这个事实，harness 侧 session 具体是什么，交给各 harness。

## 现状 vs 目标（差距）

| 层 | 现状 | 要改成 |
|---|---|---|
| engine（桥内核） | `active: HashMap<session, ActiveTurn>`、`by_ticket: HashMap<Ticket, String>`，**已经按会话串行**，多会话并行天然支持 | 不动 |
| endpoint（接入点） | `Conn.ticket: Option<Ticket>` 单值；`take_delivery` 手上有一票就不再领新轮，把一条连接上的多会话强行串行 | 改成 `HashMap<session, Ticket>`，按会话领轮/收工，允许一条连接同时跑多会话 |
| driver（dsh 驱动） | `BridgeLink.ticket` 单值，多轮会互相覆盖 | **每个 acb 会话一个独立 driver 进程**，各自单值票；不做合并、不做内部路由 |
| dsh 侧 MCP | 两个 driver = 两套 `mcp__acb__*` + `mcp__acb-group2__*` | 每个 session 一个 agent preset，各挂自己的 MCP（每 preset 一个 scope，工具面隔离） |
| 会话 → dsh session | 已经一对一（`#session` 用固定 id 建独立 dsh session） | 保持（这块已经对了） |

## 为什么引擎层早就对了、只差连接层

ADR-0017 已经把串行键从「记忆」改成「会话 id」，`engine.rs` 的 `active` / `by_ticket`
两处都按会话串行了。所以「不同会话并行」在**引擎层**已经成立。

卡住的是**连接层**的单值 `ticket`：

- 桥侧 `endpoint.rs` 的 `Conn.ticket`：一条 TCP 连接（一个 driver）手上只能有一张票，
  `take_delivery` 见 `self.ticket.is_some()` 就返回 `None` 不领新轮。于是这个 driver 声明的
  多个会话，即使引擎层 `active` 空着，也被这个单值挡着串行。

driver 侧不是靠改票解，而是靠「一个会话一个进程」解——见下节「driver 侧」。因为 MCP
`tools/call` 不带 session 标识，单进程多会话的票路由无法实现，多进程天然把票隔离开。

## 改法（分两层，都动）

### 桥侧：`Conn` 的票改成 per-session

- `Conn.ticket: Option<Ticket>` → `HashMap<String, Ticket>`（session → 该 session 正在跑的那张票）。
- `take_delivery`：不再「手上有一票就不领」，而是「这个 session 已经有票在跑就不领它的轮」。
  这样一条连接能同时领多个不同 session 的轮，各自跑、互不阻塞。
- `handle` 的 `ToolCall` / `TurnEnd`：按 ticket 反查它属于哪个 session，校验该 session 的票
  是否匹配（票仍然是一轮一签、作废即删，只是从「连接级单票」变成「会话级单票」）。
- 语义不变的部分：票还是开轮签发、收工作废；工具越界、跨会话发送的拦截逻辑不动。

### driver 侧：每个 acb 会话一个独立 driver 进程（不合并，不内部路由）

**这条路被否了**：原设想「一个 driver 合并接管两个会话、内部按 dsh session 反查路由工具」，
经查证**走不通**——dsh 的 MCP 工具调用 `tools/call` 只带 `{name, arguments}`，**不把「来自
哪个 dsh session / agent」传给 MCP server**（`dsh-mcp-client/lib/index.js` 的 `call` 闭包只
转 `name` + `arguments` + `signal`）。driver 拿到 `tools/call` 无法反推它属于哪个 acb 会话，
也就不知道该用哪张票。硬要内部路由，只能靠「同一时刻只有一个 session 在跑」的隐式假设，
那就回到串行了。

所以 driver 侧**不做** per-session 票、不做内部路由，改为：

- **每个 acb 会话 = 一个独立 driver 进程**（同一份 `driver.mjs`，`ACB_TOKEN` 指定它服务的
  会话——token 是 per-session 的，握手后一条连接只绑一个 session）。每个进程的
  `BridgeLink.ticket` 保持单值——它本来就只服务一个会话，单值够用、语义最清楚。
- dsh 侧用 **agent preset** 给每个 session 挂**它自己的那一个** MCP client 条目（每个 preset
  一个 scope，MCP 按 scope 隔离）。模型在 session A 里调工具，只会连到 session A 的 driver
  进程 → 那张票就是 session A 的，**路由天然正确，零反查**。

### driver 的 transport：streamable-http（打破「dsh 拉起」死结）

关键事实（查证 dsh 0.2.1-rc.2 源码，`@deepseek-ai/dsh-mcp-client` / `dsh-scope` /
`dsh-agent-preset` / `@modelcontextprotocol/sdk@1.31.0`）：

- 每个 **Agent（= session）是一个独立 registration scope**，MCP 挂载按 scope 隔离，各自
  独立 transport（README 原话「Independent Agent scopes may reuse the same namespace because
  their tools and transports are isolated」）。
- `dsh-scope.createScope(ctx, agent)` 按 agent 造 scope；通过 agent scope 注册的 MCP 只对
  该 agent 可见。
- agent preset 的 `config.plugins` 声明子插件（含 MCP client），每个 preset 一个 scope，
  会话选择 preset（`session/create` 请求带 `agentPreset?: string` 字段）。
- **鸡生蛋问题**：dsh 的 MCP client 用 `stdio` 是「spawn 子进程」——driver 是 MCP server +
  桥客户端 + 轮次驱动三合一，必须被 MCP client spawn；但 MCP client 若放进 preset 的
  `plugins`，只有「有 agent 选了该 preset」才 mount、才 spawn driver，而「有 agent」又靠
  driver 建 session 才发生。死循环。
- **解法**：driver 的 MCP transport 从 `stdio` 换成 **`streamable-http`**。dsh 的 MCP client
  用 `StreamableHTTPClientTransport`（官方 `@modelcontextprotocol/sdk`，非 dsh 私有）**主动
  连 driver 的 HTTP URL**。于是 driver 是独立常驻进程：自己起、自己连桥、自己建 session，
  不再靠 dsh spawn → 没有死结。ticket 仍在同一进程内共享（MCP server 和轮次驱动合一不变）。
- 无状态 streamable-http server 实现要点（对照 sdk 客户端）：GET 回 405（客户端当「无 SSE
  流」退回纯 POST，规范明说的 expected case）；POST 直接回 `application/json`，不带
  `Mcp-Session-Id`（= 无状态会话）。

这套「每会话一个 driver 进程 + agent preset + streamable-http」**完全封在 dsh harness 内部**，
acb 本体（桥 + 协议）零依赖——桥只做了「一条连接允许同时跑多会话」这个通用改动，任何
harness 都能用；dsh 特有的东西不外泄到 core。

## 影响范围与风险

- **动了「一轮一票」这条强一致保证的载体**（从连接级变成会话级）。票的签发/校验/作废逻辑
  要逐条核对，尤其：
  - 一条连接并发多轮后，`pending`/`active`/`by_ticket` 的并发访问有没有竞态（当前引擎靠
    单连接单轮隐式避免了并发，改成多轮后要确认 `Mutex<Bridge>` 的保护是否覆盖全）。
  - `take_delivery` 的领轮顺序、`end_turn` 的收工、断连 `abandon` 都要按会话处理。
- **协议不用改**（`tool_call` 帧已经带 `ticket`，`turn_start` 已经带 `session`，票本身仍是
  session 级签发的——引擎层 `ticket = "{session}-{seq}"` 已经带 session 前缀）。所以这次
  **不升协议版本**，纯是桥侧 endpoint + driver 侧的连接层改造。
- 需要补的测试：一条连接并发两个 session 各开一轮、工具调用各带各的票、一个 session 结束
  不影响另一个、断连时所有在跑会话都 abandon。

## 与旧 ADR 的关系

- 不推翻 ADR-0017，反而是**补完它第 47 行「不同会话各自并发」这个已定但未完全落地的目标**。
- 不推翻 ADR-0018（控制台/映射），会话映射、reset 都照旧。
- 协议版本不升（v4 不变），桥和 harness 不必同批次换协议，但 endpoint 和 driver 的代码要
  同批次换（票语义变了，单换一边会错乱）。

## 落地顺序

1. ✅ 桥侧 endpoint：`Conn.ticket` 单值 → per-session map，`take_delivery`/`handle`/`abandon` 按会话处理。（已完成，nix build 全绿，含新增两个并发测试）
2. ✅ driver 侧：加 `ACB_PRESET`（建 session 传 agentPreset）+ `ACB_MCP_PORT`（起 streamable-http
   MCP），把 MCP transport 从 stdio 换成 streamable-http（打破「dsh 拉起」死结）。`BridgeLink.ticket`
   保持单值（token 已 per-session，一条连接只绑一个 session）。已加 mcp-http-server 单测（7 个全绿），
   真机验证 driver 能起 streamable-http 端点 + 连桥绑会话组 dsh。
3. ✅ dsh 侧配置（真根因）：`web-acb-prod-v2.patch.yml` + `start-acb-drivers.sh` 已写好——两个 MCP
   client 从顶层移到两个 agent preset 的 `plugins` 里（streamable-http 连 3101/3102），每个 acb 会话
   一个 preset、每个 preset 只挂自己的 MCP。**尚未切换真机**（当前 dsh 仍跑旧 stdio 全局 patch）。
4. ⏳ 真机切换 + 实测：停旧 dsh/两个 stdio driver → 起两个 streamable-http driver（start-acb-drivers.sh）
   → 用 v2 patch 重启 dsh → 验证两群并行回话 + 工具各带各的票、不再 unauthorized + dsh 的
   streamable-http MCP client 握手（initialize/tools/list/tools/call）。
