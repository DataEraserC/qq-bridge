// 最小 MCP server（streamable HTTP transport，无状态、纯 JSON、无 SSE）。
//
// dsh 的 dsh-mcp-client 用 StreamableHTTPClientTransport 连本进程，工具名会变成
// mcp__<serverName>__<tool>（serverName 在 dsh 的 preset 配置里定，比如 acb）。
//
// 为什么用 streamable-http 而不是 stdio（见 ADR-0019）：
//   - stdio 是「dsh spawn 子进程」，driver 的生命周期被 dsh 捏着；而 driver 又要
//     调 dsh 建 session，会形成「driver 靠 dsh 拉起、dsh 靠 driver 建 session」的死结。
//   - streamable-http 是「dsh 主动连我们的 URL」，driver 是独立常驻进程，自己起、
//     自己连桥、自己建 session，没有死结。MCP server 和轮次驱动仍在同一进程，
//     ticket 照旧共享。
//
// 无状态实现要点（对照 MCP Streamable HTTP 规范 1.31.0 客户端）：
//   - GET 返回 405：客户端把它当「这台 server 不提供 SSE 流」，自动退回纯 POST 模式，
//     这是规范里明说的 expected case，不报错。
//   - POST 直接回 application/json（JSON-RPC 响应），不带 Mcp-Session-Id 头
//     （= 无状态会话，每次 POST 独立）。
//
// 这个文件只负责「把 MCP over HTTP 说对」，不认识 acb 桥、不认识 dsh，也不认识任何
// 聊天平台——所以能单独测（喂 HTTP 请求验它）。

import http from "node:http"
import { dispatch } from "./mcp-core.mjs"

/**
 * 一个 streamable HTTP MCP server。`tools` 是 [{ name, description, inputSchema }]，
 * `onCall(name, args)` 收到 tools/call 时被调（异步），返回结果对象或抛错。
 */
export class McpHttpServer {
  constructor({ name = "acb", version = "0.1.0", tools = [], onCall, log = () => {} } = {}) {
    this.name = name
    this.version = version
    this.tools = tools
    this.onCall = onCall ?? (async () => ({}))
    this.log = log
    this.server = null
    this.port = 0
  }

  /** 起一个 HTTP server 监听本地端口，返回实际端口（传 0 就由系统分配）。 */
  start({ host = "127.0.0.1", port = 0 } = {}) {
    const server = http.createServer((req, res) => this.#handle(req, res))
    return new Promise((resolve, reject) => {
      server.on("error", reject)
      server.listen(port, host, () => {
        this.server = server
        this.port = server.address().port
        this.log("info", `MCP streamable-http 端点就位：http://${host}:${this.port}/mcp`)
        resolve(this.port)
      })
    })
  }

  /** 停掉。 */
  stop() {
    this.server?.close()
    this.server = null
  }

  async #handle(req, res) {
    // SSE 探测：按规范回 405，客户端会退回纯 POST 模式。
    if (req.method === "GET") {
      res.writeHead(405, { "content-type": "application/json" })
      res.end(JSON.stringify({ jsonrpc: "2.0", error: { code: -32000, message: "no SSE stream; use POST" } }))
      return
    }
    if (req.method === "POST") {
      // 读 body（单个 JSON-RPC 消息或消息数组）。
      let body = ""
      for await (const chunk of req) {
        body += chunk
      }
      let message
      try {
        message = JSON.parse(body)
      } catch {
        res.writeHead(400, { "content-type": "application/json" })
        res.end(JSON.stringify({ jsonrpc: "2.0", error: { code: -32700, message: "parse error" } }))
        return
      }

      const messages = Array.isArray(message) ? message : [message]
      const responses = []
      for (const msg of messages) {
        const out = dispatch(msg, {
          name: this.name,
          version: this.version,
          tools: this.tools,
          onCall: this.onCall,
        })
        for (const item of out) {
          // tools/call 是异步的：dispatch 返回一个 _async 标记，这里 await 它完成。
          responses.push(item._async ? await item.resolve() : item)
        }
      }

      // 通知（无 id）不回；有响应就回 application/json。不带 Mcp-Session-Id = 无状态。
      if (responses.length === 0) {
        res.writeHead(202, { "content-type": "application/json" })
        res.end()
        return
      }
      res.writeHead(200, { "content-type": "application/json" })
      res.end(JSON.stringify(responses.length === 1 ? responses[0] : responses))
      return
    }
    // 其它方法（DELETE 等）：无状态会话，直接 405。
    res.writeHead(405, { "content-type": "application/json" })
    res.end(JSON.stringify({ jsonrpc: "2.0", error: { code: -32000, message: "method not allowed" } }))
  }
}
