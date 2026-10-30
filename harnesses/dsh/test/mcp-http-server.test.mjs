// MCP streamable-http server 的单测：起真 HTTP 端口，验 GET 405 / POST initialize /
// tools/list / tools/call。零外部依赖，只依赖 node 内置 http。

import test from "node:test"
import assert from "node:assert/strict"
import http from "node:http"

import { McpHttpServer } from "../src/mcp-http-server.mjs"
import { TOOLS } from "../../shared/tools.mjs"

async function startServer(tools, onCall) {
  const server = new McpHttpServer({ tools, onCall, log: () => {} })
  const port = await server.start({ port: 0 })
  return { server, port }
}

/** 发一个 POST，返回 { status, headers, body(对象或数组) }。 */
function post(port, body) {
  return new Promise((resolve, reject) => {
    const req = http.request(
      { host: "127.0.0.1", port, path: "/mcp", method: "POST", headers: { "content-type": "application/json" } },
      (res) => {
        const chunks = []
        res.on("data", (c) => chunks.push(c))
        res.on("end", () => {
          const text = Buffer.concat(chunks).toString("utf8")
          resolve({ status: res.statusCode, headers: res.headers, body: text ? JSON.parse(text) : null })
        })
      },
    )
    req.on("error", reject)
    req.write(typeof body === "string" ? body : JSON.stringify(body))
    req.end()
  })
}

function get(port) {
  return new Promise((resolve, reject) => {
    const req = http.request({ host: "127.0.0.1", port, path: "/mcp", method: "GET" }, (res) => {
      const chunks = []
      res.on("data", (c) => chunks.push(c))
      res.on("end", () => resolve({ status: res.statusCode, body: Buffer.concat(chunks).toString("utf8") }))
    })
    req.on("error", reject)
    req.end()
  })
}

test("GET 回 405（不提供 SSE，客户端退回纯 POST）", async (t) => {
  const { server, port } = await startServer([], async () => ({}))
  t.after(() => server.stop())

  const res = await get(port)
  assert.equal(res.status, 405)
})

test("initialize 回 protocolVersion + capabilities", async (t) => {
  const { server, port } = await startServer([], async () => ({}))
  t.after(() => server.stop())

  const res = await post(port, { jsonrpc: "2.0", id: 1, method: "initialize", params: {} })
  assert.equal(res.status, 200)
  assert.equal(res.headers["content-type"].startsWith("application/json"), true)
  assert.equal(res.body.id, 1)
  assert.ok(res.body.result.protocolVersion)
  assert.deepEqual(res.body.result.capabilities, { tools: {} })
})

test("tools/list 返回全部工具", async (t) => {
  const { server, port } = await startServer(
    TOOLS.map((tool) => ({ name: tool.name, description: tool.description, inputSchema: tool.schema })),
    async () => ({}),
  )
  t.after(() => server.stop())

  const res = await post(port, { jsonrpc: "2.0", id: 2, method: "tools/list", params: {} })
  assert.equal(res.status, 200)
  assert.equal(res.body.result.tools.length, TOOLS.length)
  const names = res.body.result.tools.map((tool) => tool.name)
  assert.ok(names.includes("send_message"))
  assert.ok(names.includes("ack_messages"))
})

test("tools/call 调用 onCall 并把结果包成 content", async (t) => {
  let called = null
  const { server, port } = await startServer([], async (name, args) => {
    called = { name, args }
    return "好了"
  })
  t.after(() => server.stop())

  const res = await post(port, { jsonrpc: "2.0", id: 3, method: "tools/call", params: { name: "send_message", arguments: { text: "hi" } } })
  assert.equal(res.status, 200)
  assert.deepEqual(called, { name: "send_message", args: { text: "hi" } })
  assert.equal(res.body.result.content[0].text, "好了")
  assert.equal(res.body.result.isError, false)
})

test("tools/call 出错回 isError=true", async (t) => {
  const { server, port } = await startServer([], async () => {
    throw new Error("工具没成")
  })
  t.after(() => server.stop())

  const res = await post(port, { jsonrpc: "2.0", id: 4, method: "tools/call", params: { name: "x", arguments: {} } })
  assert.equal(res.body.result.isError, true)
  assert.equal(res.body.result.content[0].text, "工具没成")
})

test("通知（无 id）回 202 空 body", async (t) => {
  const { server, port } = await startServer([], async () => ({}))
  t.after(() => server.stop())

  const res = await post(port, { jsonrpc: "2.0", method: "notifications/initialized" })
  assert.equal(res.status, 202)
})

test("非 JSON body 回 400 parse error", async (t) => {
  const { server, port } = await startServer([], async () => ({}))
  t.after(() => server.stop())

  const res = await post(port, "这不是 JSON")
  assert.equal(res.status, 400)
  assert.equal(res.body.error.code, -32700)
})
