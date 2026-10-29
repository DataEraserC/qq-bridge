// dsh 客户端的单测：用一个假的 HTTP 服务器验鉴权、unary RPC；
// follow 的事件解析用纯函数 parseFollowFrame 验。零外部依赖（只用 node:test）。

import test from "node:test"
import assert from "node:assert/strict"
import http from "node:http"
import { createHmac, createHash } from "node:crypto"

import {
  DshClient,
  mintBrowserSessionCookie,
  isLoopbackBase,
  parseFollowFrame,
  resolveDshHome,
} from "../src/dsh-client.mjs"

const base64url = (buffer) =>
  Buffer.from(buffer).toString("base64").replaceAll("+", "-").replaceAll("/", "_").replace(/=+$/, "")

const SECRET = Buffer.alloc(32, 7)

// 一个极简的假 dsh 服务：记下收到的请求，按路径回对应的响应。
class FakeDsh {
  constructor() {
    this.requests = []
    this.sessions = []
    this.server = http.createServer((req, res) => this.#handle(req, res))
  }

  async start() {
    await new Promise((resolve) => this.server.listen(0, "127.0.0.1", resolve))
    this.port = this.server.address().port
    return this
  }

  get baseUrl() {
    return `http://127.0.0.1:${this.port}`
  }

  async stop() {
    await new Promise((resolve) => this.server.close(resolve))
  }

  #handle(req, res) {
    let body = ""
    req.on("data", (chunk) => (body += chunk))
    req.on("end", () => {
      const url = new URL(req.url, "http://x")
      const path = url.pathname
      this.requests.push({ method: req.method, path, body: body ? JSON.parse(body) : null, cookie: req.headers.cookie })

      if (req.method === "GET" && url.searchParams.has("token")) {
        res.writeHead(303, { "set-cookie": "dsh-auth-test=v1.abc.sig" })
        res.end()
        return
      }

      if (req.method === "POST" && path.startsWith("/api/")) {
        const endpoint = path.slice("/api/".length)
        const message = body ? JSON.parse(body) : null
        if (message?.type !== "client-request") {
          res.writeHead(400)
          res.end()
          return
        }
        const result = this.#rpcResult(endpoint)
        res.writeHead(200, { "content-type": "application/json" })
        res.end(JSON.stringify({ type: "server-response", rpcId: message.rpcId, result }))
        return
      }

      res.writeHead(404)
      res.end()
    })
  }

  #rpcResult(endpoint) {
    switch (endpoint) {
      case "session/list":
        return { ok: true, value: { items: this.sessions } }
      case "session/create": {
        const id = `session-${this.sessions.length + 1}`
        this.sessions.push({ sessionId: id })
        return { ok: true, value: { sessionId: id } }
      }
      case "session/prompt":
        return { ok: true, value: { accepted: true } }
      default:
        return { ok: true, value: {} }
    }
  }
}

test("isLoopbackBase 认回环地址", () => {
  assert.equal(isLoopbackBase("http://127.0.0.1:3080"), true)
  assert.equal(isLoopbackBase("http://localhost:3080"), true)
  assert.equal(isLoopbackBase("http://127.1.2.3:3080"), true)
  assert.equal(isLoopbackBase("http://example.com:3080"), false)
})

test("mintBrowserSessionCookie 铸造的 Cookie 名带 dsh-auth- 前缀", () => {
  const cookie = mintBrowserSessionCookie("http://127.0.0.1:3080", SECRET)
  assert.ok(cookie.startsWith("dsh-auth-"))
  assert.ok(cookie.includes("v1."))
})

test("resolveDshHome 返回非空路径", () => {
  assert.ok(resolveDshHome().length > 0)
})

test("launch token 交换换到 Cookie", async () => {
  const dsh = new FakeDsh()
  await dsh.start()
  try {
    // secret: null 强制走 token 交换（不读真实 ~/.dsh）。
    const client = new DshClient({ baseUrl: dsh.baseUrl, token: "abc123token456", secret: null })
    const cookie = await client.ensureAuth()
    assert.equal(cookie, "dsh-auth-test=v1.abc.sig")
  } finally {
    await dsh.stop()
  }
})

test("有签名密钥时离线铸造（不交换 token）", async () => {
  const dsh = new FakeDsh()
  await dsh.start()
  try {
    const client = new DshClient({ baseUrl: dsh.baseUrl, token: "tok", secret: SECRET })
    const cookie = await client.ensureAuth()
    assert.ok(cookie.startsWith("dsh-auth-"))
    // 没走 token 交换：FakeDsh 没收到 GET /?token=
    assert.equal(dsh.requests.filter((r) => r.method === "GET").length, 0)
  } finally {
    await dsh.stop()
  }
})

test("unary RPC 按正确形状发出去（client-request + args 包装）", async () => {
  const dsh = new FakeDsh()
  await dsh.start()
  try {
    const client = new DshClient({ baseUrl: dsh.baseUrl, token: "tok", secret: SECRET })
    const sessionId = await client.createSession("acb:test")
    assert.ok(sessionId.startsWith("session-"))

    const createReq = dsh.requests.find((r) => r.path === "/api/session/create")
    assert.equal(createReq.method, "POST")
    assert.equal(createReq.body.method, "session/create")
    assert.ok(createReq.body.payload.args.request)
    assert.equal(createReq.body.payload.args.request.title, "acb:test")

    await client.prompt(sessionId, [{ type: "text", text: "你好" }])
    const promptReq = dsh.requests.find((r) => r.path === "/api/session/prompt")
    assert.ok(promptReq.body.payload.args.request.requestId)
    assert.deepEqual(promptReq.body.payload.args.request.content, [{ type: "text", text: "你好" }])
  } finally {
    await dsh.stop()
  }
})

test("session/list 用 args._request 包装（和别的 endpoint 不同）", async () => {
  const dsh = new FakeDsh()
  await dsh.start()
  try {
    const client = new DshClient({ baseUrl: dsh.baseUrl, token: "tok", secret: SECRET })
    await client.listSessions()
    const req = dsh.requests.find((r) => r.path === "/api/session/list")
    assert.ok(req.body.payload.args._request)
  } finally {
    await dsh.stop()
  }
})

test("parseFollowFrame 解析事件帧", () => {
  const sid = "stream-1"

  // turn/end 事件
  const end = parseFollowFrame(
    JSON.stringify({ type: "item", streamId: sid, value: { type: "event", event: { type: "turn/end", data: { turn: "t1" } } } }),
    sid,
  )
  assert.deepEqual(end, [{ kind: "event", type: "turn/end", data: { turn: "t1" } }])

  // turn/start 事件
  const start = parseFollowFrame(
    JSON.stringify({ type: "item", streamId: sid, value: { type: "event", event: { type: "turn/start", data: {} } } }),
    sid,
  )
  assert.equal(start[0].type, "turn/start")

  // 别的 stream 的帧忽略
  const other = parseFollowFrame(
    JSON.stringify({ type: "item", streamId: "other", value: { type: "event", event: { type: "turn/end" } } }),
    sid,
  )
  assert.deepEqual(other, [])

  // end 帧
  const ended = parseFollowFrame(JSON.stringify({ type: "end", streamId: sid }), sid)
  assert.deepEqual(ended, [{ kind: "end" }])

  // 非 JSON 忽略
  assert.deepEqual(parseFollowFrame("not json", sid), [])
})
