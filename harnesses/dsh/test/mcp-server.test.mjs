// MCP server 的单测：喂 JSON-RPC 帧，验 initialize / tools/list / tools/call。
// 零外部依赖。

import test from "node:test"
import assert from "node:assert/strict"
import { Writable, Readable } from "node:stream"

import { McpServer } from "../src/mcp-server.mjs"
import { TOOLS } from "../../shared/tools.mjs"

// 收集 server 写到 stdout 的响应。
function collect() {
  const lines = []
  const output = new Writable({
    write(chunk, _enc, cb) {
      lines.push(JSON.parse(chunk.toString()))
      cb()
    },
  })
  return { lines, output }
}

// 一个能喂消息的 stdin。
function feed(lines) {
  const input = new Readable({ read() {} })
  return { input, push: (line) => input.push(JSON.stringify(line) + "\n") }
}

test("initialize 回 protocolVersion + capabilities", async () => {
  const { lines, output } = collect()
  const server = new McpServer({ tools: [], onCall: async () => ({}) })
  server.start({ input: feed([]).input, output })

  // 直接测 dispatch 的响应：用 start 挂上，然后 push 一行 initialize。
  const { input, push } = feed([])
  server.stop()
  server.start({ input, output })
  push({ jsonrpc: "2.0", id: 1, method: "initialize", params: {} })

  await new Promise((resolve) => setTimeout(resolve, 20))
  assert.equal(lines.length, 1)
  assert.equal(lines[0].id, 1)
  assert.ok(lines[0].result.protocolVersion)
  assert.deepEqual(lines[0].result.capabilities, { tools: {} })
})

test("tools/list 返回全部工具", async () => {
  const { lines, output } = collect()
  const server = new McpServer({ tools: TOOLS.map((t) => ({ name: t.name, description: t.description, inputSchema: t.schema })), onCall: async () => ({}) })
  const { input, push } = feed([])
  server.start({ input, output })
  push({ jsonrpc: "2.0", id: 2, method: "tools/list", params: {} })

  await new Promise((resolve) => setTimeout(resolve, 20))
  assert.equal(lines.length, 1)
  assert.equal(lines[0].result.tools.length, TOOLS.length)
  const names = lines[0].result.tools.map((t) => t.name)
  assert.ok(names.includes("send_message"))
  assert.ok(names.includes("ack_messages"))
  assert.ok(names.includes("send_message_to"))
})

test("tools/call 调用 onCall 并把结果包成 content", async () => {
  let called = null
  const { lines, output } = collect()
  const server = new McpServer({
    tools: [],
    onCall: async (name, args) => {
      called = { name, args }
      return JSON.stringify({ ok: true })
    },
  })
  const { input, push } = feed([])
  server.start({ input, output })
  push({ jsonrpc: "2.0", id: 3, method: "tools/call", params: { name: "send_message", arguments: { text: "hi" } } })

  await new Promise((resolve) => setTimeout(resolve, 20))
  assert.deepEqual(called, { name: "send_message", args: { text: "hi" } })
  assert.equal(lines[0].id, 3)
  assert.equal(lines[0].result.content[0].type, "text")
  assert.equal(lines[0].result.content[0].text, JSON.stringify({ ok: true }))
  assert.equal(lines[0].result.isError, false)
})

test("tools/call 出错时回 isError=true，不炸连接", async () => {
  const { lines, output } = collect()
  const server = new McpServer({
    tools: [],
    onCall: async () => {
      throw new Error("工具没成")
    },
  })
  const { input, push } = feed([])
  server.start({ input, output })
  push({ jsonrpc: "2.0", id: 4, method: "tools/call", params: { name: "x", arguments: {} } })

  await new Promise((resolve) => setTimeout(resolve, 20))
  assert.equal(lines[0].result.isError, true)
  assert.equal(lines[0].result.content[0].text, "工具没成")
})

test("notifications/initialized 不回复", async () => {
  const { lines, output } = collect()
  const server = new McpServer({ tools: [], onCall: async () => ({}) })
  const { input, push } = feed([])
  server.start({ input, output })
  push({ jsonrpc: "2.0", method: "notifications/initialized" })

  await new Promise((resolve) => setTimeout(resolve, 20))
  assert.equal(lines.length, 0)
})

test("ping 回空 result", async () => {
  const { lines, output } = collect()
  const server = new McpServer({ tools: [], onCall: async () => ({}) })
  const { input, push } = feed([])
  server.start({ input, output })
  push({ jsonrpc: "2.0", id: 5, method: "ping" })

  await new Promise((resolve) => setTimeout(resolve, 20))
  assert.equal(lines[0].id, 5)
  assert.deepEqual(lines[0].result, {})
})
