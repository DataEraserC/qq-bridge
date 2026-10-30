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
    // dsh 的 agent preset id（config.id）：本进程建会话时指名用哪个 preset。
    // 每个 acb 会话一个 preset，每个 preset 只挂它自己的 MCP，这样模型在某个
    // dsh 会话里只会看到那一套工具，不会拿到「另一套没票的工具」去试错（ADR-0019）。
    preset: process.env.ACB_PRESET?.trim() || "",
    // 设了就起 streamable-http MCP（常驻进程模式，生产用）；不设走 stdio（兼容/测试）。
    mcpPort: process.env.ACB_MCP_PORT ? Number(process.env.ACB_MCP_PORT) : 0,
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
  const driver = new TurnDriver({ dsh, log, cwd: config.cwd, preset: config.preset })

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
  constructor({ dsh, log, cwd = "", preset = "" }) {
    this.dsh = dsh
    this.log = log
    this.cwd = cwd
    this.preset = preset
    // acb session id -> 固定的 dsh sessionId（`acb:<id>`）。用固定 id 而不是 dsh 造的
    // uuid，是为了持久化：进程重启后 createSession 传同一个 sessionId + cwd，
    // dsh 的 claim 逻辑会直接复用旧会话，不丢上下文。
    this.sessions = new Map()
    this.chain = Promise.resolve()
  }

  /** 这个 acb 会话对应的固定 dsh sessionId（不带时间戳后缀）。 */
  #fixedSessionId(acbSession) {
    // dsh 的 sessionId 都是 `session-<uuid>` 风格；这里用 `session-acb-<id>` 保持
    // 一致的命名空间，且不含冒号（避免任何潜在的 id 校验歧义）。
    return `session-acb-${acbSession}`
  }

  /**
   * 重置某个 acb 会话：丢掉当前映射，下次开轮用带时间戳的新 id 新建一个干净会话。
   * 旧会话留在 dsh 里不删（符合「新会话即可」的重置语义）。
   */
  resetSession(acbSession) {
    this.sessions.delete(acbSession)
    this.log("info", `已重置会话 ${acbSession}，下次开轮会新建`)
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
   * 这个会话的 dsh 会话：内存缓存 > 按固定 id 复用（dsh 侧 persist，重启不丢）。
   * 固定 id 是 `acb:<session>`，配合固定 cwd，dsh 的 claim 会直接复用它；
   * 换新（重置）后 id 带时间戳，就新建一个。
   */
  async #session(session, link) {
    const cached = this.sessions.get(session)
    if (cached) return cached
    const fixed = this.#fixedSessionId(session)
    const sessionId = await this.dsh.createSession({
      sessionId: fixed,
      ...(this.cwd ? { cwd: this.cwd } : {}),
      ...(this.preset ? { agentPreset: this.preset } : {}),
    })
    this.sessions.set(session, sessionId)
    this.log("info", `dsh 会话 ${sessionId}（${fixed}${this.cwd ? `，归组 ${this.cwd}` : ""}${this.preset ? `，preset ${this.preset}` : ""}）`)
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
