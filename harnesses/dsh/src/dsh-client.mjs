// acb 的 dsh 客户端：调 DeepSeek Harness (dsh) 的 Web API 驱动会话。
//
// dsh 是「进程外驱动」——它起一个 web 服务（默认 http://127.0.0.1:3080），
// 前端和后端靠 HTTP/WS 说话。我们这里不写 dsh 插件，而是像它的 webui 一样，
// 用同样的 HTTP/WS API 去驱动会话：session/prompt 把话投进去，session/follow
// 的事件流里等 turn/end，知道一轮什么时候真正结束。
//
// 协议（dsh 0.1.2 引入、0.1.5 复核，本文件按它写）：
//   - unary RPC：POST /api/<endpoint>，body { type:"client-request", rpcId, method, payload:{args} }
//   - 参数包装：session/list 用 args._request，其余多用 args.request
//   - 事件流：WS /api/remote.mux，open 一帧 session/follow 订阅某个会话，
//     收 session 事件（turn/start、turn/end 等）
//
// 鉴权两条路（二选一，都探过）：
//   1. launch token：`dsh web` 启动时打在 stdout 的 `?token=<32字节随机>`，
//      GET /?token=... 换 set-cookie。token 只活在这个进程，进程重启就失效。
//   2. 离线铸造 Cookie：读 ~/.dsh/.credentials.yaml 里 client-connection/browser-session
//      的签名密钥，本地 HMAC 签一个 Cookie。密钥是持久化的，不依赖启动器记没记日志。
//
// 这个文件只认 dsh，不认 acb 桥，也不认任何聊天平台——所以能单独测
// （test/dsh-client.test.mjs 用假的 HTTP 服务器验它）。
//
// 注意：HTTP 用 node:http、WS 用 node:net 手写，不用全局 fetch/WebSocket。
// 全局 fetch/WebSocket 是 undici，它的 HTTP 解析器走 WASM（llhttp），在受限
// 沙箱里会 WebAssembly 内存分配失败（"fetch failed" / "Received network error"）。
// 这里跟 acb 其它 harness 一样，只用 node 内置模块。

import { createHash, createHmac, randomUUID, randomBytes as cryptoRandomBytes } from "node:crypto"
import fs from "node:fs"
import http from "node:http"
import https from "node:https"
import net from "node:net"
import os from "node:os"
import path from "node:path"

/// dsh home：凭据文件（签名密钥）在这下面。
export function resolveDshHome() {
  return process.env.DSH_HOME || path.join(os.homedir(), ".dsh")
}

// ---------------------------------------------------------------------------
// 鉴权：launch token 换 Cookie，或离线铸造 Cookie
// ---------------------------------------------------------------------------

const BROWSER_SESSION_KEY = "client-connection/browser-session"
const COOKIE_PREFIX = "dsh-auth-"
const SECRET_BYTES = 32
const MINTED_COOKIE_LIFETIME_MS = 60 * 60 * 1000 // 1 小时，远小于 dsh 默认 30 天上限
const MINTED_COOKIE_REFRESH_MARGIN_MS = 5 * 60 * 1000 // 到期前 5 分钟主动重铸

const base64url = (buffer) =>
  Buffer.from(buffer)
    .toString("base64")
    .replaceAll("+", "-")
    .replaceAll("/", "_")
    .replace(/=+$/, "")

/** 从 dsh 的 stdout URL 或日志里发现 launch token。找不到返回空串。 */
export function discoverDshLaunchToken() {
  const fromEnv = process.env.DSH_LAUNCH_TOKEN
  if (typeof fromEnv === "string" && /^[A-Za-z0-9_-]{16,}$/.test(fromEnv)) {
    return fromEnv
  }
  const home = resolveDshHome()
  const dirs = [path.join(home, "logs"), path.join(home, "guard", "logs")]
  for (const logsDir of dirs) {
    let files
    try {
      files = fs
        .readdirSync(logsDir)
        .filter((name) => /^server-.*\.(?:out\.)?log$/.test(name))
        .map((name) => ({ name, mtime: fs.statSync(path.join(logsDir, name)).mtimeMs }))
        .sort((a, b) => b.mtime - a.mtime)
    } catch {
      continue
    }
    for (const { name } of files) {
      try {
        const text = fs.readFileSync(path.join(logsDir, name), "utf8")
        const match = text.match(/[?&]token=([A-Za-z0-9_-]+)/)
        if (match) return match[1]
      } catch {
        // 单个日志可能正被 dsh 占用，跳过。
      }
    }
  }
  return ""
}

/** 读 dsh 持久化的浏览器会话签名密钥。读不到返回 null。 */
export function readBrowserSessionSecret() {
  try {
    // .credentials.yaml 是 YAML；这里只做最省事的正则抠，不引 yaml 依赖。
    const text = fs.readFileSync(
      path.join(resolveDshHome(), ".credentials.yaml"),
      "utf8",
    )
    const match = text.match(
      new RegExp(`${BROWSER_SESSION_KEY.replaceAll("/", "\\/")}[^}]*?secret:\\s*['"]?([A-Za-z0-9_-]+)`),
    )
    if (!match) return null
    const secret = match[1]
    const decoded = Buffer.from(
      secret.replaceAll("-", "+").replaceAll("_", "/"),
      "base64",
    )
    return decoded.byteLength === SECRET_BYTES ? decoded : null
  } catch {
    return null
  }
}

/** 用签名密钥为本机 dsh 地址铸造一个会话 Cookie。 */
export function mintBrowserSessionCookie(baseUrl, secret, { lifetimeMs = MINTED_COOKIE_LIFETIME_MS } = {}) {
  const authority = new URL(String(baseUrl)).host
  if (!authority) throw new Error("dsh baseUrl 没有 authority")
  const issuedAt = Date.now()
  const expiresAt = issuedAt + lifetimeMs
  const body = base64url(
    Buffer.from(
      JSON.stringify({ version: 1, authority, issuedAt, expiresAt }),
      "utf8",
    ),
  )
  const signature = base64url(createHmac("sha256", secret).update(body).digest())
  const name =
    COOKIE_PREFIX + base64url(createHash("sha256").update(authority).digest())
  return `${name}=v1.${body}.${signature}`
}

/** baseUrl 是不是本机回环地址（launch token 只该发往本机）。 */
export function isLoopbackBase(baseUrl) {
  try {
    const host = new URL(String(baseUrl)).hostname.toLowerCase()
    if (host === "localhost" || host.endsWith(".localhost")) return true
    if (host === "::1" || host === "[::1]") return true
    return /^127\.\d{1,3}\.\d{1,3}\.\d{1,3}$/.test(host)
  } catch {
    return false
  }
}

// ---------------------------------------------------------------------------
// 参数包装与端点
// ---------------------------------------------------------------------------

/// 各 endpoint 的 args 包装参数名。null = 空 args，undefined = 直接放 args 里。
const METHOD_ARG_WRAPPERS = {
  "session/list": "_request",
  "session/create": "request",
  "session/prompt": "request",
  "session/cancel": "request",
  "session/selectModel": "request",
  "session/follow": "request",
}

function endpointOf(method) {
  return method.replace(/\./g, "/")
}

function wrapArgs(method, payload) {
  const endpoint = endpointOf(method)
  let body = payload ?? {}
  // 新版 SessionPromptRequest 强制要 requestId。
  if (endpoint === "session/prompt" && typeof body.requestId !== "string") {
    body = { ...body, requestId: randomUUID() }
  }
  const wrapper = METHOD_ARG_WRAPPERS[endpoint]
  if (wrapper === null) return { args: {} }
  if (wrapper === undefined) return { args: body }
  return { args: { [wrapper]: body } }
}

// ---------------------------------------------------------------------------
// 客户端本体
// ---------------------------------------------------------------------------

export class DshError extends Error {
  constructor(code, message) {
    super(message)
    this.name = "DshError"
    this.code = code
  }
}

/**
 * 一个到 dsh 的客户端。用法：
 *   const dsh = new DshClient({ baseUrl, token })
 *   await dsh.auth()                       // 换/铸造 Cookie
 *   const { sessionId } = await dsh.createSession()
 *   await dsh.prompt(sessionId, "你好")     // 投话
 *   const follow = await dsh.follow(sessionId)  // 订阅事件，等 turn/end
 *   for await (const event of follow) { ... }
 */
export class DshClient {
  constructor({ baseUrl = "http://127.0.0.1:3080", token = "", timeoutMs = 15_000, log = () => {}, secret } = {}) {
    this.baseUrl = String(baseUrl).replace(/\/+$/, "")
    this.launchToken = token
    this.timeoutMs = timeoutMs
    this.log = log
    this.cookie = null
    // secret 显式传入时用它（测试用）；undefined = 惰性读真实 .credentials.yaml。
    this._secret = secret === undefined ? undefined : (secret ?? null)
    this._strategy = null
    this._cookieExpiresAt = 0
  }

  /** 读签名密钥（惰性，只探一次）。secret 已注入时直接用它。 */
  browserSessionSecret() {
    if (this._secret === undefined) this._secret = readBrowserSessionSecret()
    return this._secret
  }

  /** 有没有至少一种鉴权手段。 */
  hasAuth() {
    return Boolean(this.launchToken) || this.browserSessionSecret() !== null
  }

  /**
   * 换或铸造一个会话 Cookie，返回 Cookie 串。
   * 优先级：有持久签名密钥 + 回环地址 → 离线铸造（稳，不依赖启动器）；
   * 否则有 launch token → 官方 token 交换。都没有就抛错。
   */
  async ensureAuth() {
    if (this.cookie) return this.cookie
    const secret = this.browserSessionSecret()
    const canMint = secret !== null && isLoopbackBase(this.baseUrl)
    if (canMint) {
      this.cookie = mintBrowserSessionCookie(this.baseUrl, secret)
      this._cookieExpiresAt = Date.now() + MINTED_COOKIE_LIFETIME_MS
      this._strategy = "minted"
      return this.cookie
    }
    if (!this.launchToken) {
      const found = discoverDshLaunchToken()
      if (found) this.launchToken = found
    }
    if (!this.launchToken) {
      throw new DshError(
        "auth",
        `没有可用的 dsh 鉴权：找不到 launch token，也读不到 ${path.join(resolveDshHome(), ".credentials.yaml")} 里的签名密钥。`,
      )
    }
    if (!isLoopbackBase(this.baseUrl)) {
      throw new DshError(
        "auth",
        `拒绝把 dsh launch token 发往非本机地址（${this.baseUrl}）。`,
      )
    }
    const url = new URL(this.baseUrl)
    url.pathname = "/"
    url.search = ""
    url.searchParams.set("token", this.launchToken)
    const res = await httpRequest(url.toString(), { method: "GET", timeoutMs: this.timeoutMs })
    // node:http 里 set-cookie 是数组（可能多个），取第一个。
    const setCookieRaw = res.headers["set-cookie"]
    const setCookie = Array.isArray(setCookieRaw) ? setCookieRaw[0] : setCookieRaw
    if (!setCookie) {
      throw new DshError("auth", `dsh token 交换失败：HTTP ${res.status}`)
    }
    this.cookie = setCookie.split(";")[0]
    this._strategy = "token"
    return this.cookie
  }

  /** 失效当前鉴权，下次请求重新换/铸造。 */
  invalidateAuth() {
    this.cookie = null
    this._secret = undefined
    this._strategy = null
    this._cookieExpiresAt = 0
    const found = discoverDshLaunchToken()
    if (found) this.launchToken = found
  }

  /** unary RPC：POST /api/<endpoint>，解出 server-response 的结果槽。 */
  async callUnary(method, payload = {}) {
    const endpoint = endpointOf(method)
    const rpcId = randomUUID()
    const message = {
      type: "client-request",
      rpcId,
      method: endpoint,
      payload: wrapArgs(method, payload),
    }
    const response = await this.#fetchWithAuth(`/api/${endpoint}`, message)
    const full = await response.json()
    if (!full || full.type !== "server-response" || full.rpcId !== rpcId || !full.result) {
      throw new DshError("protocol", `dsh 回的 ${endpoint} 不是合法 server-response`)
    }
    // dsh 的业务结果是 { ok, value } 包装；ok=false 时带 error。
    if (full.result.ok === false) {
      const { code, message: errMsg } = full.result.error ?? {}
      throw new DshError(code ?? "rpc", `${endpoint} 失败${code ? `（${code}）` : ""}：${errMsg ?? "未知错误"}`)
    }
    return full.result.value ?? full.result
  }

  async #fetchWithAuth(path, body) {
    const headers = { "content-type": "application/json" }
    if (this.hasAuth()) {
      const cookie = await this.ensureAuth()
      headers.cookie = cookie
    }
    const res = await httpRequest(this.baseUrl + path, {
      method: "POST",
      headers,
      body: JSON.stringify(body),
      timeoutMs: this.timeoutMs,
    })
    // 401 = 鉴权失效（dsh 重启 / 密钥对不上），重铸一次再试。
    if (res.status === 401 && this.hasAuth()) {
      this.invalidateAuth()
      const cookie = await this.ensureAuth()
      headers.cookie = cookie
      return httpRequest(this.baseUrl + path, {
        method: "POST",
        headers,
        body: JSON.stringify(body),
        timeoutMs: this.timeoutMs,
      })
    }
    return res
  }

  /** 列会话。 */
  async listSessions() {
    const result = await this.callUnary("session/list", {})
    return result.items ?? []
  }

  /**
   * 建（或复用）一个会话，返回 sessionId。
   * - sessionId：显式命名。传了就用它复用/创建（dsh 的 claim 逻辑：同名同 cwd 直接复用），
   *   不传就由 dsh 造一个 `session-<uuid>`。持久化靠这个固定 id。
   * - cwd：会话的工作目录，决定它归到 dsh 的哪个 workspace（分组）。dsh 会先 mkdir 这个目录。
   *   复用 named 会话时 cwd 必须和首次一致，否则报 cwd 冲突。
   * - agentPreset：dsh 的 agent preset id（config.id）。每个 acb 会话一个 preset，每个
   *   preset 只挂它自己的 MCP，模型就只会看到那一套工具（ADR-0019）。
   */
  async createSession({ sessionId, cwd, agentPreset } = {}) {
    const result = await this.callUnary("session/create", {
      ...(sessionId === undefined ? {} : { sessionId }),
      ...(cwd === undefined ? {} : { cwd }),
      ...(agentPreset === undefined ? {} : { agentPreset }),
    })
    if (!result.sessionId) {
      throw new DshError("protocol", `dsh 建会话没回 sessionId：${JSON.stringify(result).slice(0, 200)}`)
    }
    return result.sessionId
  }

  /**
   * 把一条消息投进会话（mode: queue 排队，steer 打断当前轮）。
   * 返回 dsh 的响应（通常 { accepted: true }）。
   */
  async prompt(sessionId, content, { mode = "queue", requestId } = {}) {
    return this.callUnary("session/prompt", {
      sessionId,
      mode,
      requestId,
      content,
    })
  }

  /**
   * 给会话选模型（会话级）。
   *
   * 为什么必须显式调：dsh 把「上次用的模型」存在会话记录里（modelSelection.lastUsed），
   * 复用一个老会话时它会盖住 agent-default-model。也就是说只改 dsh 的默认模型配置，
   * 老会话照样跑老模型——换模型不生效多半就是这个原因。
   */
  async selectModel(sessionId, { provider, model, reasoningEffort } = {}) {
    if (!sessionId || !provider || !model) {
      throw new DshError("bad_args", "selectModel 要 sessionId + provider + model")
    }
    return this.callUnary("session/selectModel", {
      sessionId,
      provider,
      model,
      ...(reasoningEffort ? { reasoningEffort } : {}),
    })
  }

/**
 * 订阅一个会话的事件流，返回 async iterator。每一帧是
 * { type, data, sessionId }，其中 type 含 turn/start、turn/end 等。
 * 遍历结束（流断）即 return。
 */
  async follow(sessionId) {
    const cookie = await this.ensureAuth()
    const url = new URL("/api/remote.mux", this.baseUrl)
    url.protocol = url.protocol === "https:" ? "wss:" : "ws:"
    const socket = new MiniWebSocket(url, { headers: { cookie }, timeoutMs: this.timeoutMs })
    const streamId = randomUUID()

    return (async function* () {
      const inbox = []
      let wake
      const enqueue = (item) => {
        inbox.push(item)
        wake?.()
        wake = undefined
      }
      const finish = () => enqueue({ kind: "end" })

      // 先挂好事件处理器，再握手，再发 open 帧——避免握手后立刻来的帧被吞。
      socket._onMessage = (event) => {
        for (const item of parseFollowFrame(event.data, streamId)) {
          enqueue(item)
        }
      }
      socket._onError = finish
      socket._onClose = finish

      await socket.connect()
      socket.send(
        JSON.stringify({
          type: "open",
          streamId,
          endpoint: "session/follow",
          payload: {
            args: {
              request: {
                address: { kind: "session", sessionId },
                maxMessages: 50,
              },
            },
          },
        }),
      )

      while (true) {
        while (inbox.length > 0) {
          const item = inbox.shift()
          if (item.kind === "end") {
            socket.close()
            return
          }
          if (item.kind === "event") yield item
          if (item.kind === "error") throw new DshError("stream", JSON.stringify(item.error))
        }
        await new Promise((resolve) => {
          wake = resolve
        })
      }
    })()
  }
}

/**
 * 解析 remote.mux 上一条消息（JSON 字符串），返回要排进事件队列的 item 数组。
 * 这是纯函数，便于单测。
 */
export function parseFollowFrame(rawData, streamId) {
  let msg
  try {
    msg = JSON.parse(rawData)
  } catch {
    return []
  }
  if (!msg || msg.streamId !== streamId) return []
  if (msg.type === "item" && msg.value?.type === "event") {
    return [{ kind: "event", type: msg.value.event?.type, data: msg.value.event?.data }]
  }
  if (msg.type === "end") {
    return [{ kind: "end" }]
  }
  if (msg.type === "error") {
    return [{ kind: "error", error: msg.error }]
  }
  return []
}

// ---------------------------------------------------------------------------
// 用 node:http 替代 fetch（见文件头说明）
// ---------------------------------------------------------------------------

/** 发一个 HTTP 请求，返回 { status, headers, text() }。 */
function httpRequest(urlStr, { method = "GET", headers = {}, body, timeoutMs = 15_000 } = {}) {
  return new Promise((resolve, reject) => {
    const url = new URL(urlStr)
    const lib = url.protocol === "https:" ? https : http
    const req = lib.request(
      url,
      { method, headers: { ...headers }, timeout: timeoutMs },
      (res) => {
        const chunks = []
        res.on("data", (chunk) => chunks.push(chunk))
        res.on("end", () => {
          const text = Buffer.concat(chunks).toString("utf8")
          resolve({
            status: res.statusCode,
            headers: res.headers,
            text: () => text,
            json: () => JSON.parse(text),
          })
        })
      },
    )
    req.on("timeout", () => req.destroy(new Error("请求超时")))
    req.on("error", reject)
    if (body !== undefined && body !== null) {
      req.write(typeof body === "string" ? body : JSON.stringify(body))
    }
    req.end()
  })
}

// ---------------------------------------------------------------------------
// 手写 WebSocket 客户端（基于 node:net）
// ---------------------------------------------------------------------------

/**
 * 一个最小 WebSocket 客户端：只支持发文本帧、收文本帧、正常关闭。
 * 够 dsh 的 session/follow 用（开一条流，收事件帧）。
 */
class MiniWebSocket {
  constructor(url, { headers = {}, timeoutMs = 15_000 } = {}) {
    this.url = new URL(url)
    this.headers = headers
    this.timeoutMs = timeoutMs
    this.socket = null
    this.buffer = Buffer.alloc(0)
    this._onOpen = () => {}
    this._onMessage = () => {}
    this._onError = () => {}
    this._onClose = () => {}
  }

  get readyState() {
    return this.socket ? 1 : 0
  }

  addEventListener(type, handler) {
    if (type === "open") this._onOpen = handler
    else if (type === "message") this._onMessage = handler
    else if (type === "error") this._onError = handler
    else if (type === "close") this._onClose = handler
  }

  send(text) {
    const payload = Buffer.from(text, "utf8")
    // 客户端帧必须 mask。
    const mask = cryptoRandomBytes(4)
    const masked = Buffer.alloc(payload.length)
    for (let i = 0; i < payload.length; i += 1) {
      masked[i] = payload[i] ^ mask[i % 4]
    }
    let header
    if (payload.length < 126) {
      header = Buffer.from([0x81, 0x80 | payload.length])
    } else if (payload.length < 65536) {
      header = Buffer.alloc(4)
      header[0] = 0x81
      header[1] = 0x80 | 126
      header.writeUInt16BE(payload.length, 2)
    } else {
      header = Buffer.alloc(10)
      header[0] = 0x81
      header[1] = 0x80 | 127
      header.writeBigUInt64BE(BigInt(payload.length), 2)
    }
    this.socket.write(Buffer.concat([header, mask, masked]))
  }

  connect() {
    return new Promise((resolve, reject) => {
      const { hostname, port, pathname, search } = this.url
      // Sec-WebSocket-Key 必须是 16 字节随机数的 base64 编码。
      const key = cryptoRandomBytes(16).toString("base64")
      const accept = createHash("sha1")
        .update(key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11")
        .digest("base64")

      const socket = net.createConnection({ host: hostname, port: Number(port || 80) })
      this.socket = socket
      socket.setTimeout(this.timeoutMs)

      const requestLines = [
        `GET ${pathname}${search} HTTP/1.1`,
        `Host: ${hostname}:${port || 80}`,
        "Upgrade: websocket",
        "Connection: Upgrade",
        `Sec-WebSocket-Key: ${key}`,
        "Sec-WebSocket-Version: 13",
        ...Object.entries(this.headers).map(([k, v]) => `${k}: ${v}`),
        "",
        "",
      ].join("\r\n")

      socket.on("connect", () => socket.write(requestLines))
      socket.on("timeout", () => socket.destroy(new Error("WS 连接超时")))

      let handshakeDone = false
      socket.on("data", (chunk) => {
        if (!handshakeDone) {
          const text = chunk.toString("latin1")
          const headerEnd = text.indexOf("\r\n\r\n")
          if (headerEnd < 0) {
            this.buffer = Buffer.concat([this.buffer, chunk])
            return
          }
          const headerText = text.slice(0, headerEnd)
          if (!headerText.includes("101")) {
            reject(new Error(`WS 握手失败：${headerText.split("\r\n")[0]}`))
            socket.destroy()
            return
          }
          handshakeDone = true
          const rest = chunk.subarray(headerEnd + 4)
          this.buffer = Buffer.concat([this.buffer, rest])
          resolve()
          this._onOpen()
          this.#parseFrames()
        } else {
          this.buffer = Buffer.concat([this.buffer, chunk])
          this.#parseFrames()
        }
      })
      socket.on("error", (error) => {
        if (!handshakeDone) reject(error)
        else this._onError({ message: error.message })
      })
      socket.on("close", () => this._onClose())
    })
  }

  close() {
    // 关闭帧：0x88 0x00
    try {
      this.socket?.write(Buffer.from([0x88, 0x80, 0, 0, 0, 0]))
    } catch {
      // 忽略。
    }
    this.socket?.end()
  }

  #parseFrames() {
    for (;;) {
      if (this.buffer.length < 2) return
      const b0 = this.buffer[0]
      const b1 = this.buffer[1]
      const opcode = b0 & 0x0f
      let length = b1 & 0x7f
      let offset = 2
      if (length === 126) {
        if (this.buffer.length < 4) return
        length = this.buffer.readUInt16BE(2)
        offset = 4
      } else if (length === 127) {
        if (this.buffer.length < 10) return
        length = Number(this.buffer.readBigUInt64BE(2))
        offset = 10
      }
      if (this.buffer.length < offset + length) return
      const payload = this.buffer.subarray(offset, offset + length)
      this.buffer = this.buffer.subarray(offset + length)

      if (opcode === 0x8) {
        // 关闭帧。
        this._onClose()
        return
      }
      if (opcode === 0x9) {
        // ping → pong。
        const mask = cryptoRandomBytes(4)
        this.socket.write(Buffer.concat([Buffer.from([0x8a, 0x80 | payload.length]), mask, payload]))
        continue
      }
      if (opcode === 0x1) {
        this._onMessage({ data: payload.toString("utf8") })
      }
      // 其它 opcode（0x0 续帧、0x2 二进制）这里忽略，dsh 只用文本帧。
    }
  }
}
