// 协议客户端和海投渲染的测试。
//
// 这里对面是个**假的桥**（就是本仓库 Rust 那侧的协议），不是假平台——平台那一段
// 用户有真的 OneBot，不在这里装。这个假桥只验一件事：我们说的帧，桥认不认。

import net from "node:net"
import { test } from "node:test"
import assert from "node:assert/strict"

import {
  BridgeError,
  BridgeLink,
  normalizeToolArgs,
  parseEndpoint,
  renderElements,
  renderTurn,
} from "../qqab-wire.mjs"

const TICKET = "main-1"

function delivery(overrides = {}) {
  return {
    ticket: TICKET,
    device: "main",
    room: { platform: "qq", room: "group/158296748" },
    reason: "wake",
    records: [
      {
        id: 12,
        room: { platform: "qq", room: "group/158296748" },
        kind: { kind: "message" },
        sender: { id: "10001", name: "老王" },
        at: Date.now(),
        elements: [{ type: "text", text: "在吗" }],
        at_self: true,
      },
    ],
    ...overrides,
  }
}

// ---------------------------------------------------------------------------
// 假桥
// ---------------------------------------------------------------------------

class FakeBridge {
  constructor({ device = "main", token = "t-main" } = {}) {
    this.device = device
    this.token = token
    this.frames = []
    this.socket = null
  }

  async start() {
    this.server = net.createServer((socket) => {
      this.socket = socket
      socket.setEncoding("utf8")
      this.lines = new LineQueue(socket)
      void this.#greet()
    })
    await new Promise((resolve) => this.server.listen(0, "127.0.0.1", resolve))
    this.port = this.server.address().port
    return this
  }

  get endpoint() {
    return `127.0.0.1:${this.port}`
  }

  async #greet() {
    const hello = await this.lines.next()
    this.hello = hello
    this.#write({ frame: "ready", device: this.device })
  }

  async next() {
    const frame = await this.lines.next()
    this.frames.push(frame)
    return frame
  }

  async send(frame) {
    this.#write(frame)
  }

  #write(frame) {
    this.socket.write(`${JSON.stringify(frame)}\n`)
  }

  async stop() {
    this.socket?.destroy()
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

// ---------------------------------------------------------------------------
// 握手与轮次
// ---------------------------------------------------------------------------

/** 起一个假桥 + 一个客户端，测完保证都收掉（不然 node 不退出）。 */
async function setup(t, { onTurn } = {}) {
  const bridge = await new FakeBridge().start()
  const link = new BridgeLink({
    endpoint: bridge.endpoint,
    token: "t-main",
    onTurn,
  })
  t.after(async () => {
    link.close()
    await bridge.stop()
  })
  await link.connect()
  return { bridge, link }
}

/** 开一轮，并等到客户端真的拿到票。 */
async function openTurn(bridge, link, payload = delivery()) {
  await bridge.send({ frame: "turn_start", delivery: payload })
  await waitFor(() => link.ticket === payload.ticket, "客户端应该拿到本轮的票，拿到的是 " + link.ticket)
  return payload
}

test("握手带上协议版本和令牌，拿到设备", async (t) => {
  const { bridge, link } = await setup(t)

  assert.equal(link.device, "main")
  assert.deepEqual(bridge.hello, { frame: "hello", version: 1, token: "t-main" })
})

test("开轮之后工具调用带着本轮的票，结果能对上号", async (t) => {
  const turns = []
  const { bridge, link } = await setup(t, {
    onTurn: async (turn) => turns.push(turn),
  })

  await openTurn(bridge, link)
  await waitFor(() => turns.length === 1, "onTurn 应该被叫到")

  const pending = link.call("send_message", {
    elements: [{ type: "text", text: "在的" }],
  })
  const call = await bridge.next()
  assert.equal(call.frame, "tool_call")
  assert.equal(call.tool, "send_message")
  assert.equal(call.ticket, TICKET)
  assert.deepEqual(call.args, { elements: [{ type: "text", text: "在的" }] })

  await bridge.send({
    frame: "tool_result",
    id: call.id,
    result: { platform_id: "p-1", watermark: 12 },
  })
  assert.deepEqual(await pending, { platform_id: "p-1", watermark: 12 })
})

test("工具失败带着错误码抛出来，不是悄悄返回空", async (t) => {
  const { bridge, link } = await setup(t, { onTurn: async () => {} })
  await openTurn(bridge, link)

  const pending = link.call("send_message_to", {
    elements: [{ type: "text", text: "喂" }],
    to: "qq:group/999",
  })
  const call = await bridge.next()
  await bridge.send({
    frame: "tool_result",
    id: call.id,
    error: { code: "room_not_subscribed", message: "没有订阅这个房间" },
  })

  await assert.rejects(pending, (error) => {
    assert.ok(error instanceof BridgeError, `应该是 BridgeError，实际是 ${error}`)
    assert.equal(error.code, "room_not_subscribed")
    assert.match(error.message, /没有订阅/)
    return true
  })
})

test("没有进行中的轮次时，工具调不动", async (t) => {
  const { link } = await setup(t)

  await assert.rejects(link.call("send_message", { text: "喂" }), (error) => {
    assert.equal(error.code, "unauthorized")
    return true
  })
})

test("收工发 turn_end，票随之作废", async (t) => {
  const { bridge, link } = await setup(t, {
    onTurn: async (turn) => {
      link.endTurn(turn.ticket)
    },
  })
  await bridge.send({ frame: "turn_start", delivery: delivery() })

  const end = await bridge.next()
  assert.deepEqual(end, { frame: "turn_end", ticket: TICKET })
  assert.equal(link.ticket, null)
})

test("桥发 ping 要回 pong", async (t) => {
  const { bridge } = await setup(t)

  await bridge.send({ frame: "ping" })
  assert.deepEqual(await bridge.next(), { frame: "pong" })
})

// ---------------------------------------------------------------------------
// 渲染与参数
// ---------------------------------------------------------------------------

test("一轮摊成给模型看的文本", () => {
  const text = renderTurn(
    delivery({
      records: [
        delivery().records[0],
        {
          id: 13,
          room: { platform: "qq", room: "group/158296748" },
          kind: { kind: "event", event: "poke" },
          sender: { id: "10002", name: "小明" },
          at: Date.now(),
          elements: [{ type: "text", text: "[拍了拍我]" }],
          at_self: true,
          redelivery: 2,
        },
      ],
    }),
    { now: Date.now() },
  )

  assert.match(text, /qq:group\/158296748/)
  assert.match(text, /#12/)
  assert.match(text, /老王\(10001\)/)
  assert.match(text, /冲你来的/)
  assert.match(text, /在吗/)
  assert.match(text, /事件 poke/)
  assert.match(text, /第 2 次投递/)
  assert.match(text, /ack_messages/)
})

test("空记录的一轮也要说清楚是例行检查", () => {
  const text = renderTurn({ ...delivery(), records: [], reason: "clock" })
  assert.match(text, /定时例行检查/)
  assert.match(text, /没有新记录/)
})

test("内容元素都摊得开，平台内容不假装能懂", () => {
  const text = renderElements([
    { type: "text", text: "看图 " },
    { type: "image", url: "https://x/1.png" },
    { type: "at", target: "10001" },
    { type: "at", all: true },
    { type: "quote", message_id: "9" },
    { type: "face", platform: "qq", id: "14" },
    { type: "opaque", platform: "qq", payload: { big: true } },
  ])
  assert.match(text, /看图/)
  assert.match(text, /\[图片 https:\/\/x\/1\.png\]/)
  assert.match(text, /@10001/)
  assert.match(text, /@全体成员/)
  assert.match(text, /\[引用 #9\]/)
  assert.match(text, /\[表情 14\]/)
  assert.match(text, /qq内容/)
})

test("endpoint 只认 host:port", () => {
  assert.deepEqual(parseEndpoint("127.0.0.1:7700"), {
    host: "127.0.0.1",
    port: 7700,
  })
  assert.throws(() => parseEndpoint("127.0.0.1"), /127\.0\.0\.1:7700/)
  assert.throws(() => parseEndpoint("127.0.0.1:abc"), /端口/)
})

test("text 是单段文本的糖，elements 才是本体", () => {
  assert.deepEqual(normalizeToolArgs("send_message", { text: "在的" }), {
    elements: [{ type: "text", text: "在的" }],
  })
  assert.deepEqual(
    normalizeToolArgs("send_message", {
      elements: [{ type: "text", text: "a" }, { type: "at", target: "1" }],
    }),
    { elements: [{ type: "text", text: "a" }, { type: "at", target: "1" }] },
  )
  assert.throws(
    () => normalizeToolArgs("send_message", { elements: [{ type: "sticker" }] }),
    /不认识的元素类型/,
  )
  assert.throws(() => normalizeToolArgs("send_message", {}), /至少要有 text/)
})

test("发到别的房间必须有 to", () => {
  assert.deepEqual(
    normalizeToolArgs("send_message_to", { to: "qq:group/1", text: "喂" }),
    { elements: [{ type: "text", text: "喂" }], to: "qq:group/1" },
  )
  assert.throws(() => normalizeToolArgs("send_message_to", { text: "喂" }), /要写 to/)
})

test("确认的编号必须是整数，空的不算", () => {
  assert.deepEqual(normalizeToolArgs("ack_messages", { ids: [1, 2] }), {
    ids: [1, 2],
  })
  assert.throws(() => normalizeToolArgs("ack_messages", { ids: [] }), /要带 ids/)
  assert.throws(
    () => normalizeToolArgs("ack_messages", { ids: ["1"] }),
    /只能是整数/,
  )
})

test("其它工具原样透传，顺手去掉空字段", () => {
  assert.deepEqual(
    normalizeToolArgs("get_history", { limit: 5, room: undefined }),
    { limit: 5 },
  )
  assert.deepEqual(
    normalizeToolArgs("set_note", {
      key: "k",
      value: "v",
      room: "qq:group/1",
      status: null,
    }),
    { key: "k", value: "v", room: "qq:group/1" },
  )
})

async function waitFor(predicate, message) {
  const deadline = Date.now() + 2000
  while (Date.now() < deadline) {
    if (predicate()) return
    await new Promise((resolve) => setTimeout(resolve, 5))
  }
  assert.fail(message)
}
