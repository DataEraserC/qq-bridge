// acb 的 dsh 驱动器：一个进程同时干三件事。
//
//   1. 作为 acb 桥的客户端（BridgeLink，复用 opencode 的 acb-wire.mjs）：
//      连桥、被推 turn_start、发 tool_call、回 turn_end。
//   2. 作为 MCP server：暴露桥上的工具，工具名在 dsh 那边变成 mcp__<serverName>__<tool>。
//      有两种 transport（见 ADR-0019）：
//        - streamable-http（生产）：driver 是独立常驻进程，自己监听一个本地 HTTP 端口，
//          dsh 的 MCP client 用 streamable-http 主动连它。没有「driver 靠 dsh 拉起」的死结。
//        - stdio（兼容/测试）：被 dsh 的 StdioClientTransport spawn。
//   3. 作为 dsh 的客户端（DshClient）：调 dsh webui 的 HTTP/WS API 驱动会话，
//      session/prompt 投话、session/follow 等 turn/end。
//
// 为什么要合成一个进程：tool_call 必须带当前轮次的 ticket，而 ticket 是从
// turn_start 拿到的。MCP server 和轮次驱动分开两个进程的话，ticket 没法共享，
// 模型调工具时发不出带正确票的 tool_call。换成 streamable-http 后仍是同一进程，
// ticket 照旧共享。
//
// 拓扑（streamable-http 模式）：
//   [机器A：桥 + 平台]  ←acb TCP(跨机可)→  [机器B：dsh + 本驱动器(常驻)]
//                                            ├─ MCP streamable-http（dsh 连本进程的 URL）
//                                            └─ 本进程连 A 的桥 + 连 dsh 的 HTTP/WS
//
// 配置走环境变量（dsh 的 preset 配置里写 env，或启动时注入）：
//   ACB_ENDPOINT  桥的接入点，默认 127.0.0.1:7700
//   ACB_TOKEN     设备令牌，必填；和 config.toml 里设备的 token 一致
//   ACB_PRESET    dsh 的 agent preset id（config.id）：本进程建会话时指名用哪个 preset
//   ACB_MCP_PORT  设了就起 streamable-http MCP（常驻模式）；不设走 stdio（兼容/测试）
//   DSH_BASE_URL  dsh webui 地址，默认 http://127.0.0.1:3080
//   DSH_TOKEN     dsh 启动 token（可选，没有就走离线铸造 Cookie）
//   ACB_CWD       会话工作目录：dsh 按它把会话归进某个 workspace（Workspaces 分组）
//   ACB_WORKSPACE_TITLE  新建 workspace 时给它起的显示标题（默认「ACB」）

import { BridgeLink, BridgeError, normalizeToolArgs, renderTurn } from "../../opencode/plugin/acb-wire.mjs"
import { TOOLS } from "../../shared/tools.mjs"
import { McpServer } from "./mcp-server.mjs"
import { McpHttpServer } from "./mcp-http-server.mjs"
import { DshClient } from "./dsh-client.mjs"

const log = (level, message) => {
  // stdout 是 MCP 通道，一个字都不能碰；诊断全走 stderr。
  process.stderr.write(`[acb-dsh] ${level} ${message}\n`)
}

function readConfig() {
  return {
    endpoint: process.env.ACB_ENDPOINT?.trim() || "127.0.0.1:7700",
    token: process.env.ACB_TOKEN?.trim() || "",
    harness: process.env.ACB_HARNESS?.trim() || "",
    dshBaseUrl: process.env.DSH_BASE_URL?.trim() || "http://127.0.0.1:3080",
    dshToken: process.env.DSH_TOKEN?.trim() || "",
    // 会话的工作目录：dsh 按它把会话归到某个 workspace（分组）。留空就不分组。
    cwd: process.env.ACB_CWD?.trim() || "",
    // dsh workspace 标题（新建 workspace 时命名，如「QQ 聊天」）。留空不命名。
    workspaceTitle: process.env.ACB_WORKSPACE_TITLE?.trim() || "",
    // dsh 的 agent preset id（config.id）：本进程建会话时指名用哪个 preset。
    // 每个 acb 会话一个 preset，每个 preset 只挂它自己的 MCP，这样模型在某个
    // dsh 会话里只会看到那一套工具，不会拿到「另一套没票的工具」去试错（ADR-0019）。
    preset: process.env.ACB_PRESET?.trim() || "",
    // 设了就起 streamable-http MCP（常驻进程模式，生产用）；不设走 stdio（兼容/测试）。
    mcpPort: process.env.ACB_MCP_PORT ? Number(process.env.ACB_MCP_PORT) : 0,
    // 本进程要钉住的模型。dsh 把「上次用的模型」存在会话记录里，复用老会话时会盖住
    // agent-default-model——只改补丁里的默认模型，老会话照样跑老模型。所以这里给了就
    // 每个会话显式 selectModel 一次。留空 = 不干预，用会话当前选的那个。
    model: {
      provider: process.env.ACB_DSH_PROVIDER?.trim() || "",
      model: process.env.ACB_DSH_MODEL?.trim() || "",
      reasoningEffort: process.env.ACB_DSH_EFFORT?.trim() || "",
    },
  }
}

async function main() {
  const config = readConfig()
  if (!config.token) {
    log("error", "没有 ACB_TOKEN：dsh 驱动不起来（工具面和轮次驱动都得有设备令牌）")
    process.exit(2)
  }

  const dsh = new DshClient({ baseUrl: config.dshBaseUrl, token: config.dshToken, log })

  // 轮次驱动：桥推 turn_start → dsh 投话 → 等 turn/end → turn_end。
  const driver = new TurnDriver({
    dsh,
    log,
    cwd: config.cwd,
    preset: config.preset,
    model: config.model,
    workspaceTitle: config.workspaceTitle,
  })

  const link = new BridgeLink({
    endpoint: config.endpoint,
    token: config.token,
    harness: config.harness,
    log,
    onTurn: (delivery) => driver.run(delivery, link),
    // 桥推来的控制指令（如 reset）：driver 处理，处理完回报结果。
    onControl: (frame) => driver.handleControl(frame, link),
  })

  // MCP server：暴露桥上的工具，工具调用转成 tool_call（带当前 ticket）。
  // 工具面两份（stdio / streamable-http）共用同一套定义和同一个 onCall。
  const toolDefs = TOOLS.map((t) => ({
    name: t.name,
    description: t.description,
    inputSchema: t.schema,
  }))
  const onCall = (name, args) => callBridge(link, name, args)

  // 桥是独立的 TCP 连接，可以现在就连。连上后把桥下发的会话映射 seed 进驱动，
  // 这样跨进程重启也能复用旧 dsh 会话，而不是每次都新建一个。
  void link
    .connect()
    .then(() => {
      driver.seedMaps(link.sessionMaps)
      log("info", `已接入桥，绑定会话组 ${link.sessions.join("、")}`)
    })
    .catch((error) => log("warn", `暂时连不上桥：${error.message}，会一直重试`))

  if (config.mcpPort > 0) {
    // 生产：streamable-http 常驻模式。driver 自己监听端口，dsh 的 MCP client 主动连。
    const mcpHttp = new McpHttpServer({
      name: "acb",
      version: "0.1.0",
      tools: toolDefs,
      onCall,
      log,
    })
    const actualPort = await mcpHttp.start({ port: config.mcpPort })
    log("info", `dsh 驱动器就位（streamable-http）：桥 ${config.endpoint}，dsh ${config.dshBaseUrl}，MCP http://127.0.0.1:${actualPort}/mcp`)

    process.on("SIGINT", () => {
      link.close()
      mcpHttp.stop()
      process.exit(0)
    })
  } else {
    // 兼容/测试：stdio 模式，被 dsh 的 StdioClientTransport 拉起。
    const mcp = new McpServer({
      name: "acb",
      version: "0.1.0",
      tools: toolDefs,
      onCall,
      log,
    })
    // stdio 挂上（dsh 拉起本进程时，stdin/stdout 已经就位）。
    mcp.start()

    log("info", `dsh 驱动器就位（stdio）：桥 ${config.endpoint}，dsh ${config.dshBaseUrl}`)

    process.on("SIGINT", () => {
      link.close()
      mcp.stop()
      process.exit(0)
    })
  }
}

// ---------------------------------------------------------------------------
// 轮次驱动
// ---------------------------------------------------------------------------

class TurnDriver {
  constructor({ dsh, log, cwd = "", preset = "", model = null, workspaceTitle = "" }) {
    this.dsh = dsh
    this.log = log
    this.cwd = cwd
    this.preset = preset
    // 配了就把 cwd 对应的 dsh workspace 命名成这个标题（只在新建 workspace 时命名，
    // 不覆盖用户在 dsh 里手改过的名字）。dsh UI 的 Workspaces 列表就按它显示。
    this.workspaceTitle = workspaceTitle
    // 要钉住的模型 {provider, model, reasoningEffort}；provider/model 任一为空就不干预。
    this.model = model && model.provider && model.model ? model : null
    // 已经钉过模型的 dsh 会话。一个会话只调一次 selectModel，别每轮都发一遍。
    this.modelPinned = new Set()
    // acb session id -> 固定的 dsh sessionId（`acb:<id>`）。用固定 id 而不是 dsh 造的
    // uuid，是为了持久化：进程重启后 createSession 传同一个 sessionId + cwd，
    // dsh 的 claim 逻辑会直接复用旧会话，不丢上下文。
    this.sessions = new Map()
    // 重置代次：reset 过的会话换新 id（-r<代次>），避开同 id + 不同 cwd 的冲突。
    this.generations = new Map()
    // 已经探过 workspace 归属的 cwd（每个只发一次 workspace/create）。
    this.workspaceEnsured = new Set()
    this.chain = Promise.resolve()
  }

  /** 这个 acb 会话对应的固定 dsh sessionId（重置过后带代次后缀）。 */
  #fixedSessionId(acbSession) {
    // dsh 的 sessionId 都是 `session-<uuid>` 风格；这里用 `session-acb-<id>` 保持
    // 一致的命名空间，且不含冒号（避免任何潜在的 id 校验歧义）。
    const generation = this.generations.get(acbSession)
    return `session-acb-${acbSession}${generation ? `-r${generation}` : ""}`
  }

  /**
   * 重置某个 acb 会话：丢掉当前映射 + 记一代次号，下次开轮用带代次的新 id
   * 新建一个干净会话（不记代次的话还是同一个 id，dsh 会因为 cwd 冲突拒绝：
   * `session "…" belongs to …, not …`）。旧会话留在 dsh 里不删（符合「新会话即可」的重置语义）。
   */
  resetSession(acbSession) {
    this.sessions.delete(acbSession)
    this.generations.set(acbSession, Date.now().toString(36))
    this.log("info", `已重置会话 ${acbSession}，下次开轮用新 id 新建`)
  }

  /**
   * 处理桥推来的控制指令（ADR-0018）。目前只认 `reset`：
   * 丢掉当前会话映射，回报成功；下次开轮用带时间戳的新 id 新建干净会话。
   * 处理完用 link.controlResult 回报，好让桥把映射落盘。
   */
  async handleControl(frame, link) {
    const { id, session, command } = frame
    if (command === "reset") {
      try {
        this.resetSession(session)
        link.controlResult({ id, session, command, result: { ok: true } })
      } catch (error) {
        this.log("error", `重置会话 ${session} 失败：${error?.message ?? error}`)
        link.controlResult({ id, session, command, error: String(error?.message ?? error) })
      }
      return
    }
    // 不认识的指令：报错，别默默吞掉。
    this.log("warn", `不认识的 control 指令：${command}`)
    link.controlResult({ id, session, command, error: `不认识的指令 ${command}` })
  }

  run(delivery, link) {
    const step = () => this.#one(delivery, link)
    this.chain = this.chain.then(step, step)
    return this.chain
  }

  async #one(delivery, link) {
    let failure = null
    try {
      const sessionId = await this.#session(delivery.session, link)
      await this.#pinModel(sessionId)
      const text = renderTurn(delivery)
      const info = delivery.session_info ?? {}
      const model = info.model ?? "(dsh 默认)"
      this.log(
        "debug",
        `开轮 ${delivery.ticket}：${delivery.room.connection}:${delivery.room.path.join("/")}，${delivery.records.length} 条，模型 ${model}`,
      )
      // prompt 投话，follow 等这一轮真正结束（turn/end）。
      await this.dsh.prompt(sessionId, [{ type: "text", text }])
      const turnFailure = await this.#waitTurnEnd(sessionId)
      if (turnFailure) {
        failure = turnFailure
        this.log("error", `这一轮失败了：${failure}`)
        // 失败/中止不丢会话映射：下次开轮继续用同一个 dsh 会话，不新建。
        // 只有显式 reset（handleControl）才解除 acb↔harness 会话关联。
      }
    } catch (error) {
      failure = error?.message ?? String(error)
      this.log("error", `这一轮失败了：${failure}`)
      // 同上：异常也不丢映射，避免「一轮失败就换一个新会话」。
    } finally {
      try {
        link.endTurn(delivery.ticket, failure)
      } catch (error) {
        this.log("warn", `回 turn_end 失败：${error.message}`)
      }
    }
  }

  /**
   * 把桥下发的会话映射（acb 会话 ↔ dsh 会话）填进缓存，跨进程重启复用旧会话。
   * 桥在握手 `Ready` 帧里带回它落盘的 session_maps（ADR-0018）。这里直接拿来当缓存，
   * 这样 driver 重启后第一次开轮就不会重新 createSession，而是复用上次那个 dsh 会话。
   *
   * 注意：只 seed 当前还不在缓存里的会话，不覆盖运行中新建的。
   */
  seedMaps(maps) {
    if (!Array.isArray(maps)) return
    for (const map of maps) {
      if (!map?.session || !map?.harness_session) continue
      if (this.sessions.has(map.session)) continue
      this.sessions.set(map.session, map.harness_session)
      this.log("info", `复用已存会话：${map.session} → ${map.harness_session}`)
    }
  }

  /**
   * 把这个会话的模型钉成本进程配置的那个（每个会话一次）。
   *
   * 为什么必须做：dsh 把「上次用的模型」记在会话里，复用老会话时它优先于
   * agent-default-model。只改补丁里的默认模型，老会话会继续跑老模型——而且一点报错都没有，
   * 只能靠看上游（网关）的日志才发现请求还在打旧渠道。
   *
   * 失败不阻断这一轮：钉不上就用会话当前的模型跑，只留一条 warn。
   */
  async #pinModel(sessionId) {
    if (!this.model || this.modelPinned.has(sessionId)) return
    const { provider, model, reasoningEffort } = this.model
    try {
      await this.dsh.selectModel(sessionId, { provider, model, reasoningEffort })
      this.modelPinned.add(sessionId)
      this.log("info", `会话 ${sessionId} 模型钉到 ${provider}/${model}${reasoningEffort ? `（${reasoningEffort}）` : ""}`)
    } catch (error) {
      this.log("warn", `给会话 ${sessionId} 选模型失败（这轮仍用会话当前模型）：${error?.message ?? error}`)
    }
  }

  /**
   * 这个会话的 dsh 会话：内存缓存 > 按固定 id 复用（dsh 侧 persist，重启不丢）。
   * 固定 id 是 `acb:<session>`，配合固定 cwd，dsh 的 claim 会直接复用它；
   * 换新（重置）后 id 带时间戳，就新建一个。
   */
  async #session(session, link) {
    const cached = this.sessions.get(session)
    if (cached) return cached
    const fixed = this.#fixedSessionId(session)
    let sessionId
    try {
      sessionId = await this.dsh.createSession({
        sessionId: fixed,
        ...(this.cwd ? { cwd: this.cwd } : {}),
        ...(this.preset ? { agentPreset: this.preset } : {}),
      })
    } catch (error) {
      const message = String(error?.message ?? error)
      // 同 id 已存在但归属别的 cwd（比如 cwd 环境变量改过、或旧版本没传 cwd）：
      // 不硬顶，换一个带代次的 id 新建，保住「能开轮」这个主要矛盾。
      if (/conflict|belongs to/i.test(message)) {
        const fallback = `${fixed}-r${Date.now().toString(36)}`
        this.log("warn", `dsh 会话 ${fixed} 有 id/cwd 冲突，改用 ${fallback} 新建：${message}`)
        sessionId = await this.dsh.createSession({
          sessionId: fallback,
          ...(this.cwd ? { cwd: this.cwd } : {}),
          ...(this.preset ? { agentPreset: this.preset } : {}),
        })
      } else {
        throw error
      }
    }
    this.sessions.set(session, sessionId)
    this.log("info", `dsh 会话 ${sessionId}（${fixed}${this.cwd ? `，归组 ${this.cwd}` : ""}${this.preset ? `，preset ${this.preset}` : ""}）`)
    // workspace 标题：best-effort，失败不影响开轮。每个 cwd 只探一次（workspace/create
    // 对已存在的路径虽然无害，但没必要每次建会话都发）；且只在 workspace 是这次新建
    // 的时候命名，不覆盖已有的（可能是用户手改过的）标题。
    if (this.cwd && this.workspaceTitle && !this.workspaceEnsured.has(this.cwd)) {
      this.workspaceEnsured.add(this.cwd)
      try {
        const named = await this.dsh.ensureWorkspaceTitle(this.cwd, this.workspaceTitle)
        if (named?.created) this.log("info", `dsh workspace ${this.cwd} 命名为「${this.workspaceTitle}」`)
      } catch (error) {
        // 探过但失败：从集合里放出来，下轮再试一次。
        this.workspaceEnsured.delete(this.cwd)
        this.log("warn", `给 dsh workspace 命名失败（不影响会话）：${error?.message ?? error}`)
      }
    }
    // 建好会话就把映射回报给桥，让控制台能查到「acb 会话 ↔ dsh 会话」的对应关系。
    // 用 report_map 指令，result 里带 session_id + meta（这里放固定 id 和分组）。
    if (link) {
      link.controlResult({
        id: `map-${Date.now()}`,
        session,
        command: "report_map",
        result: {
          session_id: sessionId,
          meta: {
            fixed_id: fixed,
            cwd: this.cwd || null,
          },
        },
      })
    }
    return sessionId
  }

  /**
   * 订阅会话事件流，等到 turn/end 就返回。加个超时兜底，避免 follow 流断了
   * （dsh 重启、网络抖动）导致这一轮永远卡住。
   *
   * 返回这一轮的失败信息（`null` 表示正常结束）。turn/end 事件的 reason 会带出
   * 为什么结束：`completed` 是正常，`error`/`blocked`/`aborted`/`max-tokens` 都是
   * 失败。尤其「模型回了 completed 但没有内容」（no content）在 dsh 里也被映射成
   * `error`，不读 reason 就会把它当成成功、失败信息被吞掉。
   */
  async #waitTurnEnd(sessionId) {
    const timeout = 5 * 60 * 1000 // 5 分钟兜底
    const stream = await this.dsh.follow(sessionId)
    const deadline = Date.now() + timeout
    for await (const event of stream) {
      if (event.type === "turn/end") {
        return renderTurnFailure(event.data?.reason)
      }
      if (Date.now() > deadline) {
        this.log("warn", `等 turn/end 超时（${sessionId}），按本轮结束处理`)
        return "本轮超时：模型 5 分钟内没有结束这一轮"
      }
    }
    // 流断了但没等到 turn/end：也当结束，别让轮次挂死。
    return "事件流断了，没等到 turn/end"
  }
}

// ---------------------------------------------------------------------------
// MCP 工具调用 → 桥的 tool_call
// ---------------------------------------------------------------------------

/**
 * 把 dsh 的 turn/end reason 翻成给用户看的失败信息；正常结束返回 null。
 *
 * dsh 的 reason 是 `{ kind, ... }`：`completed` 是正常；`error` 带 `error.message`
 * 和 `error.code`；`blocked` / `aborted` / `max-tokens` / `interrupted` 等是其它
 * 失败形态。这里把每个形态都翻成一句人能懂的话，尤其把「completed 但没有内容」
 * 这种被 dsh 判成 error 的失败原样带出来。
 */
function renderTurnFailure(reason) {
  if (!reason || reason.kind === "completed") return null
  switch (reason.kind) {
    case "error": {
      const message = reason.error?.message ?? "未知错误"
      const code = reason.error?.code ?? ""
      return code ? `${message}（${code}）` : message
    }
    case "aborted":
      return "这一轮被中止了"
    case "blocked":
      return "这一轮被拦住了（可能等人工授权）"
    case "max-tokens":
      return "这一轮触发了输出上限（max-tokens）"
    case "interrupted":
      return "这一轮在崩溃前被打断，没能正常结束"
    default:
      return `这一轮以未预期的原因结束：${reason.kind ?? "未知"}`
  }
}

async function callBridge(link, name, args) {
  let wire
  try {
    wire = normalizeToolArgs(name, args)
  } catch (error) {
    throw new Error(error.message)
  }
  try {
    const result = await link.call(name, wire)
    if (result === null || result === undefined) return "好了"
    return typeof result === "string" ? result : JSON.stringify(result, null, 2)
  } catch (error) {
    const code = error instanceof BridgeError ? error.code : "internal"
    throw new Error(`${name} 没成（${code}）：${error.message}`)
  }
}

// 作为主模块（node src/driver.mjs）时直接跑；被测试 import 时不动。
import { fileURLToPath } from "node:url"

const isMain = process.argv[1] && fileURLToPath(import.meta.url) === process.argv[1]
if (isMain) {
  main().catch((error) => {
    log("error", `驱动器起不来：${error?.stack ?? error}`)
    process.exit(1)
  })
}

export { TurnDriver, callBridge }
