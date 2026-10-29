// 最小 MCP server（stdio，newline-delimited JSON-RPC 2.0）。
//
// 不引 @modelcontextprotocol/sdk，只实现 acb 需要的三个方法：
//   initialize / notifications/initialized / tools/list / tools/call / ping。
// dsh 的 dsh-mcp-client 用 StdioClientTransport 拉起本进程，工具名会变成
// mcp__<serverName>__<tool>（serverName 在 dsh 配置里定，比如 acb）。
//
// 帧格式（和 MCP stdio 规范一致）：每条消息一行 JSON，末尾 \n。stdout 是唯一
// 通道，一个字都不能乱写（stderr 可以自由打日志）。
//
// 这个文件只负责「把 MCP 协议说对」，不认识 acb 桥、不认识 dsh，也不认识任何
// 聊天平台——所以能单独测（test/mcp-server.test.mjs 喂 JSON-RPC 帧验它）。

import readline from "node:readline"

const PROTOCOL_VERSION = "2024-11-05"

/**
 * 一个 MCP server。`tools` 是 [{ name, description, inputSchema }]，
 * `onCall(name, args)` 收到 tools/call 时被调，返回结果对象或抛错。
 */
export class McpServer {
  constructor({ name = "acb", version = "0.1.0", tools = [], onCall, log = () => {} } = {}) {
    this.name = name
    this.version = version
    this.tools = tools
    this.onCall = onCall ?? (async () => ({}))
    this.log = log
    this._initialized = false
    this._rl = null
  }

  /** 挂到 stdin/stdout 上开始收请求。返回后 stdout 就能发响应。 */
  start({ input = process.stdin, output = process.stdout } = {}) {
    this._output = output
    this._rl = readline.createInterface({ input, crlfDelay: Infinity })
    this._rl.on("line", (line) => {
      const text = line.trim()
      if (!text) return
      let message
      try {
        message = JSON.parse(text)
      } catch {
        this.log("warn", `收到非 JSON 的 MCP 消息：${text.slice(0, 200)}`)
        return
      }
      this.#dispatch(message)
    })
  }

  /** 停掉，不再收消息。 */
  stop() {
    this._rl?.close()
    this._rl = null
  }

  #dispatch(message) {
    // 通知（没有 id）不回复。
    if (message.method === "notifications/initialized") {
      this._initialized = true
      return
    }
    if (typeof message.id === "undefined") return

    switch (message.method) {
      case "initialize":
        this.#respond(message.id, {
          protocolVersion: PROTOCOL_VERSION,
          capabilities: { tools: {} },
          serverInfo: { name: this.name, version: this.version },
        })
        return
      case "ping":
        this.#respond(message.id, {})
        return
      case "tools/list":
        this.#respond(message.id, { tools: this.tools.map((t) => ({
          name: t.name,
          description: t.description ?? "",
          inputSchema: t.inputSchema ?? { type: "object", properties: {} },
        })) })
        return
      case "tools/call":
        this.#handleCall(message)
        return
      default:
        this.#respond(message.id, null, -32601, `不知道的 MCP 方法 ${message.method}`)
    }
  }

  async #handleCall(message) {
    const { name, arguments: args } = message.params ?? {}
    try {
      const result = await this.onCall(name, args ?? {})
      this.#respond(message.id, {
        content: [
          {
            type: "text",
            text: typeof result === "string" ? result : JSON.stringify(result),
          },
        ],
        isError: false,
      })
    } catch (error) {
      // 工具报错：isError=true 让模型看到失败原因，而不是整个 MCP 连接断掉。
      this.#respond(message.id, {
        content: [{ type: "text", text: error?.message ?? String(error) }],
        isError: true,
      })
    }
  }

  #respond(id, result, code, message) {
    if (result !== null && result !== undefined) {
      this.#write({ jsonrpc: "2.0", id, result })
    } else {
      this.#write({ jsonrpc: "2.0", id, error: { code: code ?? -32600, message: message ?? "error" } })
    }
  }

  #write(object) {
    this._output?.write(JSON.stringify(object) + "\n")
  }
}
