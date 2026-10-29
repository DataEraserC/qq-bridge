// 插件本体的测试：对面是一个**模拟的 opencode v2 宿主**。
//
// 为什么要有这一层：本机 opencode2 2.0.22（Bun 1.4.2）加载**任何**插件都会段错误
// （连 `export default { id, setup(){} }` 这四行都崩，官方提示是 Bun 的 bug），
// 所以「真 opencode 里跑一遍」这条路暂时走不通。退一步，用一个照着 v2 文档实现的
// 假宿主，把插件要用的那几样（tool.transform / session / storage / app）摆出来，
// 验的是我们自己的逻辑，不是 opencode 内部。
//
// 假宿主的关键一点：`prompt` 只是把话排进去，模型**之后**才干活，工具是在这段时间里
// 被调用的；所以 `wait` 由测试决定什么时候结束（finishTurn），一轮才会真正收工。
// 不然会验出一个假结论：工具调用总是 unauthorized。

import { test } from "node:test"
import assert from "node:assert/strict"

import plugin from "../plugin/index.js"
import { FakeBridge, TICKET, delivery, waitFor } from "./fake-bridge.mjs"

const TOKEN = "t-main"

class FakeHost {
  constructor() {
    this.tools = new Map()
    this.created = []
    this.sessions = new Map()
    this.prompts = []
    this.waits = []
    this.storage = new Map()
    this.logs = []
    this.waiters = []
    this.options = {}
    this.#seq = 0
  }

  #seq

  /** 让「这一轮助手说完了」发生。 */
  finishTurn() {
    const waiting = this.waiters.splice(0)
    for (const resolve of waiting) resolve()
  }

  get ctx() {
    const self = this
    return {
      options: this.options,
      app: {
        version: "fake-2.0",
        log: async ({ body }) => self.logs.push(body),
      },
      tool: {
        async transform(callback) {
          callback({
            add: (spec) => self.tools.set(spec.name, spec),
            update: () => {},
            remove: (name) => self.tools.delete(name),
            namespace: () => {},
            list: () => [],
            get: (name) => self.tools.get(name),
          })
          return { dispose: async () => {} }
        },
      },
      session: {
        async create({ title }) {
          const id = `ses_${++self.#seq}`
          self.created.push({ id, title })
          self.sessions.set(id, { id, title })
          return { id, title }
        },
        async get({ sessionID }) {
          const found = self.sessions.get(sessionID)
          if (!found) throw new Error(`没有这个会话：${sessionID}`)
          return found
        },
        async prompt({ sessionID, text }) {
          self.prompts.push({ sessionID, text })
        },
        async wait({ sessionID }) {
          self.waits.push(sessionID)
          await new Promise((resolve) => self.waiters.push(resolve))
        },
      },
      storage: {
        async get(key) {
          return self.storage.get(key)
        },
        async set(key, value) {
          self.storage.set(key, value)
        },
        async remove(key) {
          self.storage.delete(key)
        },
      },
    }
  }
}

/** 起假桥 + 假宿主，装上插件，测完都收掉。 */
async function boot(t, { token = TOKEN } = {}) {
  const bridge = await new FakeBridge({ devices: ["main"], token: TOKEN }).start()
  const host = new FakeHost()
  // 选项必须在 setup 之前摆好：插件读一次配置就定了。
  host.options = { endpoint: bridge.endpoint }

  const saved = process.env.ACB_TOKEN
  if (token === null) delete process.env.ACB_TOKEN
  else process.env.ACB_TOKEN = token

  const cleanup = await plugin.setup(host.ctx)

  t.after(async () => {
    host.finishTurn()
    await cleanup?.()
    await bridge.stop()
    if (saved === undefined) delete process.env.ACB_TOKEN
    else process.env.ACB_TOKEN = saved
  })

  await bridge.whenConnected()
  return { bridge, host }
}

/** 开一轮，并等到「模型开始干活」那一刻（wait 已经挂上）。 */
async function openTurn(bridge, host, payload = delivery()) {
  const promptsBefore = host.prompts.length
  const waitsBefore = host.waits.length
  await bridge.send({ frame: "turn_start", delivery: payload })
  await waitFor(
    () =>
      host.prompts.length > promptsBefore && host.waits.length > waitsBefore,
    "插件应该提问并进入等待",
  )
}

test("装上之后工具面齐全，名字和桥上的对得上", async (t) => {
  const { host } = await boot(t)

  assert.deepEqual([...host.tools.keys()].sort(), [
    "ack_messages",
    "del_note",
    "get_history",
    "get_notes",
    "get_platform_events",
    "list_rooms",
    "send_message",
    "send_message_to",
    "set_note",
  ])
  // v2 的参数是 JSON Schema，不是 zod。
  const send = host.tools.get("send_message")
  assert.equal(send.input.type, "object")
  assert.ok(send.input.properties.elements)
  assert.match(send.description, /内容段数组/)
})

test("一轮走完：建会话 → 提问 → 等它说完 → 收工", async (t) => {
  const { bridge, host } = await boot(t)

  await openTurn(bridge, host)

  assert.equal(host.created.length, 1, "应该建了一个会话")
  assert.equal(host.created[0].title, "acb:main-memory", "会话按记忆一份")
  assert.equal(host.prompts[0].sessionID, host.created[0].id)
  assert.match(host.prompts[0].text, /在吗/, "投递的内容要进上下文")
  assert.match(
    host.prompts[0].text,
    /tools\.send_message/,
    "要顺带告诉模型这些工具得走 execute 才调得动（opencode 的 Code Mode）",
  )
  assert.deepEqual(host.waits, [host.created[0].id], "prompt 之后要等这一轮说完")

  host.finishTurn()
  assert.deepEqual(await bridge.next(), { frame: "turn_end", ticket: TICKET })
})

test("同一设备第二轮接着用同一个会话，不新建", async (t) => {
  const { bridge, host } = await boot(t)

  await openTurn(bridge, host)
  host.finishTurn()
  await bridge.next()

  await openTurn(bridge, host, delivery({ ticket: "main-2" }))
  host.finishTurn()
  assert.deepEqual(await bridge.next(), { frame: "turn_end", ticket: "main-2" })

  assert.equal(host.created.length, 1, "第二次不该再建会话")
  assert.equal(host.prompts.length, 2)
})

test("会话没了就重建，不往死会话里灌", async (t) => {
  const { bridge, host } = await boot(t)

  await openTurn(bridge, host)
  host.finishTurn()
  await bridge.next()

  host.sessions.clear() // 模拟用户在 opencode 里删了会话

  await openTurn(bridge, host, delivery({ ticket: "main-2" }))
  host.finishTurn()
  await bridge.next()

  assert.equal(host.created.length, 2, "旧会话没了就该新建一个")
})

test("工具调用转发给桥，结果原样交回模型", async (t) => {
  const { bridge, host } = await boot(t)
  await openTurn(bridge, host)

  const pending = host.tools.get("send_message").execute({ text: "在的" })
  const call = await bridge.next()
  assert.equal(call.frame, "tool_call")
  assert.equal(call.tool, "send_message")
  assert.equal(call.ticket, TICKET, "工具调用要带本轮的票")
  assert.deepEqual(call.args, { elements: [{ type: "text", text: "在的" }] })

  await bridge.send({
    frame: "tool_result",
    id: call.id,
    result: { platform_id: "p-1", watermark: 13 },
  })
  const out = await pending
  assert.match(out.content, /p-1/)

  host.finishTurn()
  await bridge.next()
})

test("桥报错时工具抛错，带上错误码", async (t) => {
  const { bridge, host } = await boot(t)
  await openTurn(bridge, host)

  const pending = host.tools
    .get("send_message_to")
    .execute({ to: "qq:group/999", text: "喂" })
  const call = await bridge.next()
  await bridge.send({
    frame: "tool_result",
    id: call.id,
    error: { code: "room_not_subscribed", message: "没有订阅这个房间" },
  })

  await assert.rejects(pending, (error) => {
    assert.match(error.message, /room_not_subscribed/)
    assert.match(error.message, /没有订阅/)
    return true
  })

  host.finishTurn()
  await bridge.next()
})

test("没有令牌就不启用：不注册工具、不连桥", async (t) => {
  const bridge = await new FakeBridge({ devices: ["main"], token: TOKEN }).start()
  const host = new FakeHost()
  const saved = process.env.ACB_TOKEN
  delete process.env.ACB_TOKEN
  host.options = { endpoint: bridge.endpoint }
  t.after(async () => {
    await bridge.stop()
    if (saved !== undefined) process.env.ACB_TOKEN = saved
  })

  const cleanup = await plugin.setup(host.ctx)

  assert.equal(host.tools.size, 0, "没令牌就别往模型面前摆工具")
  assert.equal(host.logs.length, 1)
  assert.equal(host.logs[0].level, "warn")

  await new Promise((resolve) => setTimeout(resolve, 150))
  assert.equal(bridge.socket, null, "没令牌就不该连桥")
  await cleanup?.()
})

test("卸载时把连接关掉（不留后台线程）", async (t) => {
  const bridge = await new FakeBridge({ devices: ["main"], token: TOKEN }).start()
  const host = new FakeHost()
  const saved = process.env.ACB_TOKEN
  process.env.ACB_TOKEN = TOKEN
  t.after(async () => {
    await bridge.stop()
    if (saved === undefined) delete process.env.ACB_TOKEN
    else process.env.ACB_TOKEN = saved
  })

  host.options = { endpoint: bridge.endpoint }
  const cleanup = await plugin.setup(host.ctx)
  await bridge.whenConnected()

  const closed = new Promise((resolve) => bridge.socket.on("close", resolve))
  await cleanup()
  await closed
})

test("send_message_to 留了 device 这个口子：同一房间两台设备时指明身份", async (t) => {
  const { host } = await boot(t)

  const spec = host.tools.get("send_message_to")
  assert.ok(spec.input.properties.device, "要给 device 参数，否则模型没法指明用谁说")
  assert.match(spec.description, /device/, "说明里要提到 device")
  assert.match(spec.description, /订阅/, "说明里要讲清范围是订阅")
})

test("两台设备共用一份记忆时，落进同一个会话", async (t) => {
  const { bridge, host } = await boot(t)

  // 同一份记忆 m，先由 main 跑一轮。
  await openTurn(bridge, host, delivery({ ticket: "main-1", device: "main", memory: "m" }))
  host.finishTurn()
  await bridge.next()

  // helper 那台设备也看同一份记忆：不该再建一个会话。
  await openTurn(bridge, host, delivery({ ticket: "helper-2", device: "helper", memory: "m" }))
  host.finishTurn()
  await bridge.next()

  assert.equal(host.created.length, 1, "记忆就是会话，两台设备共用记忆只该有一个会话")
  assert.equal(host.prompts[1].sessionID, host.created[0].id)
})
