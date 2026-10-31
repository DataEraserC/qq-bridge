// 测试共用的「假桥」与等待帮手。
//
// 对面是**假的桥**（本仓库 Rust 那侧的协议），不是假平台——平台那一段用户有真的
// OneBot，不在这里装。假桥只验一件事：我们说的帧，桥认不认。

import net from "node:net"
import assert from "node:assert/strict"

export const TICKET = "main-1"

export function delivery(overrides = {}) {
  return {
    ticket: TICKET,
    session: "main",
    room: { connection: "qq", path: ["group", "158296748"] },
    reason: "wake",
    records: [
      {
        id: 12,
        room: { connection: "qq", path: ["group", "158296748"] },
        kind: { kind: "message" },
        sender: { id: "10001", name: "老王" },
        at: Date.now(),
        elements: [{ type: "text", text: "在吗" }],
        at_self: true,
      },
    ],
    session_info: { id: "main", name: "main", harness: "opencode", model: null },
    ...overrides,
  }
}

export class FakeBridge {
  constructor({ sessions = ["main"], token = "t-main" } = {}) {
    this.sessions = sessions
    this.token = token
    this.frames = []
    this.socket = null
    this.connected = null
    this.#connected = new Promise((resolve) => {
      this.connected = resolve
    })
  }

  #connected

  async start() {
    this.sockets = new Set()
    this.server = net.createServer((socket) => {
      this.socket = socket
      this.sockets.add(socket)
      socket.setEncoding("utf8")
      this.lines = new LineQueue(socket)
      this.connected()
      // 收尾时新连接可能还挂着等 hello，别让它变成未处理的 rejection。
      void this.#greet().catch(() => {})
    })
    await new Promise((resolve) => this.server.listen(0, "127.0.0.1", resolve))
    this.port = this.server.address().port
    return this
  }

  /** 等客户端真的连上来（插件是异步连接的，测试得等它）。 */
  whenConnected() {
    return this.#connected
  }

  get endpoint() {
    return `127.0.0.1:${this.port}`
  }

  async #greet() {
    const hello = await this.lines.next()
    this.hello = hello
    this.#write({ frame: "ready", sessions: this.sessions })
  }

  async next() {
    // report_map 的 control_result 是插件建会话后的旁路回报，不属于轮次帧。
    // 测试关心的是 turn_end / tool_call，跳过它，别让每个用例都先撞一帧。
    for (;;) {
      const frame = await this.lines.next()
      this.frames.push(frame)
      if (frame.frame === "control_result") continue
      return frame
    }
  }

  async send(frame) {
    this.#write(frame)
  }

  #write(frame) {
    this.socket.write(`${JSON.stringify(frame)}\n`)
  }

  async stop() {
    for (const socket of this.sockets ?? []) socket.destroy()
    this.sockets?.clear()
    await new Promise((resolve) => this.server.close(resolve))
  }
}

class LineQueue {
  constructor(socket) {
    this.buffer = ""
    this.waiting = []
    this.lines = []
    socket.on("data", (chunk) => {
      this.buffer += chunk
      for (;;) {
        const at = this.buffer.indexOf("\n")
        if (at < 0) return
        const line = this.buffer.slice(0, at)
        this.buffer = this.buffer.slice(at + 1)
        this.#push(JSON.parse(line))
      }
    })
  }

  #push(value) {
    const waiter = this.waiting.shift()
    if (waiter) waiter(value)
    else this.lines.push(value)
  }

  next() {
    if (this.lines.length) return Promise.resolve(this.lines.shift())
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error("假桥等帧超时")), 2000)
      this.waiting.push((value) => {
        clearTimeout(timer)
        resolve(value)
      })
    })
  }
}

export async function waitFor(predicate, message) {
  const deadline = Date.now() + 2000
  while (Date.now() < deadline) {
    if (predicate()) return
    await new Promise((resolve) => setTimeout(resolve, 5))
  }
  assert.fail(message)
}
