// acb 的 dsh 驱动器：一个进程同时干三件事。
//
//   1. 作为 acb 桥的客户端（BridgeLink，复用 opencode 的 acb-wire.mjs）：
//      连桥、被推 turn_start、发 tool_call、回 turn_end。
//   2. 作为 MCP server（stdio）：被 dsh 的 dsh-mcp-client 拉起，暴露桥上九个工具，
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
    dshBaseUrl: process.env.DSH_BASE_URL?.trim() || "http://127.0.0.1:3080",
    dshToken: process.env.DSH_TOKEN?.trim() || "",
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
  const driver = new TurnDriver({ dsh, log })

  const link = new BridgeLink({
    endpoint: config.endpoint,
    token: config.token,
    log,
    onTurn: (delivery) => driver.run(delivery, link),
  })

  // MCP server：暴露九个工具，工具调用转成 tool_call（带当前 ticket）。
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
    .then(() => log("info", `已接入桥，绑定设备组 ${link.devices.join("、")}`))
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
  constructor({ dsh, log }) {
    this.dsh = dsh
    this.log = log
    this.sessions = new Map() // memory -> sessionId
    this.chain = Promise.resolve()
  }

  run(delivery, link) {
    const step = () => this.#one(delivery, link)
    this.chain = this.chain.then(step, step)
    return this.chain
  }

  async #one(delivery, link) {
    let failure = null
    try {
      const sessionId = await this.#session(delivery.memory ?? delivery.device)
      const text = renderTurn(delivery)
      this.log(
        "debug",
        `开轮 ${delivery.ticket}：${delivery.room.connection}:${delivery.room.path.join("/")}，${delivery.records.length} 条`,
      )
      // prompt 投话，follow 等这一轮真正结束（turn/end）。
      await this.dsh.prompt(sessionId, [{ type: "text", text }])
      await this.#waitTurnEnd(sessionId)
    } catch (error) {
      failure = error?.message ?? String(error)
      this.log("error", `这一轮失败了：${failure}`)
      this.sessions.delete(delivery.memory ?? delivery.device)
    } finally {
      try {
        link.endTurn(delivery.ticket, failure)
      } catch (error) {
        this.log("warn", `回 turn_end 失败：${error.message}`)
      }
    }
  }

  /**
   * 这份记忆的会话：记住的 > 新建。dsh 的 sessionId 我们存内存里；进程重启会
   * 丢，但 dsh 那边会话还活着，下次可以用 session/list 按标题找回（先做简单的：
   * 内存缓存 + 进程内复用）。
   */
  async #session(memory) {
    const cached = this.sessions.get(memory)
    if (cached) return cached
    const sessionId = await this.dsh.createSession(`acb:${memory}`)
    this.sessions.set(memory, sessionId)
    this.log("info", `新 dsh 会话 ${sessionId}（acb:${memory}）`)
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
