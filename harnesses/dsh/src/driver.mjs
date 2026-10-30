// acb 的 dsh 驱动器：一个进程同时干三件事。
//
//   1. 作为 acb 桥的客户端（BridgeLink，复用 opencode 的 acb-wire.mjs）：
//      连桥、被推 turn_start、发 tool_call、回 turn_end。
//   2. 作为 MCP server（stdio）：被 dsh 的 dsh-mcp-client 拉起，暴露桥上的工具，
//      工具名在 dsh 那边变成 mcp__<serverName>__<tool>。
//   3. 作为 dsh 的客户端（DshClient）：调 dsh webui 的 HTTP/WS API 驱动会话，
//      session/prompt 投话、session/follow 等 turn/end。
//
// 为什么要合成一个进程：tool_call 必须带当前轮次的 ticket，而 ticket 是从
// turn_start 拿到的。MCP server 和轮次驱动分开两个进程的话，ticket 没法共享，
// 模型调工具时发不出带正确票的 tool_call。
//
// 拓扑：驱动器必须跟 dsh 同机（MCP stdio 是 dsh spawn 出来的子进程），
// 桥在哪无所谓——acb 协议是 TCP，端点可配，跨机也能连。
//
//   [机器A：桥 + 平台]  ←acb TCP(跨机可)→  [机器B：dsh + 本驱动器]
//                                            ├─ MCP stdio（dsh 拉起本进程）
//                                            └─ 本进程连 A 的桥
//
// 配置走环境变量（dsh 的 MCP 配置里写 env，或启动时注入）：
//   ACB_ENDPOINT  桥的接入点，默认 127.0.0.1:7700
//   ACB_TOKEN     设备令牌，必填；和 config.toml 里设备的 token 一致
//   DSH_BASE_URL  dsh webui 地址，默认 http://127.0.0.1:3080
//   DSH_TOKEN     dsh 启动 token（可选，没有就走离线铸造 Cookie）

import { BridgeLink, BridgeError, normalizeToolArgs, renderTurn } from "../../opencode/plugin/acb-wire.mjs"
import { TOOLS } from "../../shared/tools.mjs"
import { McpServer } from "./mcp-server.mjs"
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
  const driver = new TurnDriver({ dsh, log, cwd: config.cwd })

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
  const mcp = new McpServer({
    name: "acb",
    version: "0.1.0",
    tools: TOOLS.map((t) => ({
      name: t.name,
      description: t.description,
      inputSchema: t.schema,
    })),
    onCall: (name, args) => callBridge(link, name, args),
    log,
  })

  // 桥是独立的 TCP 连接，可以现在就连；MCP 的 stdio 由 dsh 拉起时接管。
  void link
    .connect()
    .then(() => log("info", `已接入桥，绑定会话组 ${link.sessions.join("、")}`))
    .catch((error) => log("warn", `暂时连不上桥：${error.message}，会一直重试`))

  // stdio 挂上（dsh 拉起本进程时，stdin/stdout 已经就位）。
  mcp.start()

  log("info", `dsh 驱动器就位：桥 ${config.endpoint}，dsh ${config.dshBaseUrl}`)

  process.on("SIGINT", () => {
    link.close()
    mcp.stop()
    process.exit(0)
  })
}

// ---------------------------------------------------------------------------
// 轮次驱动
// ---------------------------------------------------------------------------

class TurnDriver {
  constructor({ dsh, log, cwd = "" }) {
    this.dsh = dsh
    this.log = log
    this.cwd = cwd
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
      await this.#waitTurnEnd(sessionId)
    } catch (error) {
      failure = error?.message ?? String(error)
      this.log("error", `这一轮失败了：${failure}`)
      this.sessions.delete(delivery.session)
    } finally {
      try {
        link.endTurn(delivery.ticket, failure)
      } catch (error) {
        this.log("warn", `回 turn_end 失败：${error.message}`)
      }
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
    })
    this.sessions.set(session, sessionId)
    this.log("info", `dsh 会话 ${sessionId}（${fixed}${this.cwd ? `，归组 ${this.cwd}` : ""}）`)
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
   */
  async #waitTurnEnd(sessionId) {
    const timeout = 5 * 60 * 1000 // 5 分钟兜底
    const stream = await this.dsh.follow(sessionId)
    const deadline = Date.now() + timeout
    for await (const event of stream) {
      if (event.type === "turn/end") return
      if (Date.now() > deadline) {
        this.log("warn", `等 turn/end 超时（${sessionId}），按本轮结束处理`)
        return
      }
    }
    // 流断了但没等到 turn/end：也当结束，别让轮次挂死。
  }
}

// ---------------------------------------------------------------------------
// MCP 工具调用 → 桥的 tool_call
// ---------------------------------------------------------------------------

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
