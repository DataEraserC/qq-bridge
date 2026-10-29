// 驱动器逻辑的单测：假桥 + 假 dsh，验「turn_start → prompt → 等 turn/end → turn_end」
// 闭环，以及 MCP 工具调用转成带票的 tool_call。
//
// 假桥直接复用 opencode 的 fake-bridge（同一个协议，不重复造）。

import test from "node:test"
import assert from "node:assert/strict"

import { BridgeLink } from "../../opencode/plugin/acb-wire.mjs"
import { TurnDriver, callBridge } from "../src/driver.mjs"
import { FakeBridge, TICKET, delivery } from "../../opencode/test/fake-bridge.mjs"

// 可控制 follow 的假 dsh。
class ControllableDsh {
  constructor() {
    this.prompts = []
    this.sessions = []
    this.lastFollow = null
  }

  async createSession(title) {
    const id = `session-${this.sessions.length + 1}`
    this.sessions.push(id)
    return id
  }

  async prompt(sessionId, content) {
    this.prompts.push({ sessionId, content })
    return { accepted: true }
  }

  async follow(sessionId) {
    const follow = makeManualStream()
    this.lastFollow = follow
    return follow
  }
}

function makeManualStream() {
  const queue = []
  let wake
  let ended = false
  return {
    async *[Symbol.asyncIterator]() {
      while (true) {
        while (queue.length > 0) {
          const item = queue.shift()
          if (item === "end") return
          yield item
        }
        if (ended) return
        await new Promise((resolve) => {
          wake = resolve
        })
      }
    },
    push(item) {
      queue.push(item)
      wake?.()
      wake = undefined
    },
    end() {
      ended = true
      wake?.()
      wake = undefined
    },
  }
}

/** 起一个假桥 + 连上 + 握手的 link。FakeBridge 的 #greet 在 connect 时自动回 ready。 */
async function connectedLink(bridge, token = "t-main") {
  const link = new BridgeLink({ endpoint: bridge.endpoint, token, log: () => {} })
  await link.connect()
  return link
}

async function waitFor(predicate, message) {
  const deadline = Date.now() + 2000
  while (Date.now() < deadline) {
    if (predicate()) return
    await new Promise((resolve) => setTimeout(resolve, 5))
  }
  assert.fail(message)
}

test("轮次驱动：turn_start → prompt → 等 turn/end → turn_end", async (t) => {
  const bridge = await new FakeBridge().start()
  const dsh = new ControllableDsh()
  const link = await connectedLink(bridge)
  t.after(async () => {
    link.close()
    await bridge.stop()
  })

  const driver = new TurnDriver({ dsh, log: () => {} })
  const runPromise = driver.run(delivery(), link)

  // 等 prompt 被调。
  await waitFor(() => dsh.prompts.length === 1, "prompt 没被调")
  assert.equal(dsh.prompts[0].sessionId, "session-1")
  assert.ok(dsh.prompts[0].content[0].text.includes("在吗"), "投递内容应包含消息")

  // 推一个 turn/end。
  dsh.lastFollow.push({ type: "turn/end", data: { turn: "t1" } })
  await runPromise

  // 应该有一个 turn_end 帧（用 next() 从 LineQueue 里取）。
  const turnEnd = await bridge.next()
  assert.equal(turnEnd.frame, "turn_end")
  assert.equal(turnEnd.ticket, TICKET)
})

test("turn/end 后回 turn_end 帧（无错误）", async (t) => {
  const bridge = await new FakeBridge().start()
  const dsh = new ControllableDsh()
  const link = await connectedLink(bridge)
  t.after(async () => {
    link.close()
    await bridge.stop()
  })

  const driver = new TurnDriver({ dsh, log: () => {} })
  const runPromise = driver.run(delivery(), link)

  await waitFor(() => dsh.prompts.length === 1, "prompt 没被调")
  dsh.lastFollow.push({ type: "turn/end", data: { turn: "t1" } })
  await runPromise

  const turnEnd = await bridge.next()
  assert.equal(turnEnd.frame, "turn_end")
  assert.equal(turnEnd.ticket, TICKET)
  assert.equal(turnEnd.error, undefined)
})

test("callBridge 把工具参数转成带票的 tool_call", async (t) => {
  const bridge = await new FakeBridge().start()
  const link = await connectedLink(bridge)
  t.after(async () => {
    link.close()
    await bridge.stop()
  })

  // 手动设一个 ticket（模拟轮次中）。
  link.ticket = TICKET

  const callPromise = callBridge(link, "send_message", { text: "收到" })

  // 桥那边应该收到一个 tool_call 帧。
  const frame = await bridge.next()
  assert.equal(frame.frame, "tool_call")
  assert.equal(frame.ticket, TICKET)
  assert.equal(frame.tool, "send_message")
  assert.deepEqual(frame.args, { elements: [{ type: "text", text: "收到" }] })

  // 回一个 tool_result。
  bridge.socket.write(JSON.stringify({ frame: "tool_result", id: frame.id, result: { platform_id: "123" } }) + "\n")
  const result = await callPromise
  assert.ok(result.includes("123"))
})

test("没有 ticket 时 callBridge 报 unauthorized", async (t) => {
  const bridge = await new FakeBridge().start()
  const link = await connectedLink(bridge)
  t.after(async () => {
    link.close()
    await bridge.stop()
  })

  // 不设 ticket，直接调。
  await assert.rejects(
    () => callBridge(link, "send_message", { text: "收到" }),
    /没有进行中的轮次|调不动/,
  )
})
