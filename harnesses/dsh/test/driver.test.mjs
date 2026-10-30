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
    this.modelPicks = []
    this.lastFollow = null
  }

  async createSession(title) {
    const id = `session-${this.sessions.length + 1}`
    this.sessions.push(id)
    return id
  }

  async selectModel(sessionId, choice) {
    this.modelPicks.push({ sessionId, ...choice })
    return { selected: choice }
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
  const link = new BridgeLink({ endpoint: bridge.endpoint, token, harness: "dsh", log: () => {} })
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

/** 取下一帧，但跳过 report_map 的 control_result（建会话时 driver 会先回报映射）。 */
async function nextTurnEnd(bridge) {
  for (;;) {
    const frame = await bridge.next()
    if (frame.frame === "turn_end") return frame
    // 不是 turn_end（比如 report_map 的 control_result）就接着取下一帧。
  }
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

  // 应该有一个 turn_end 帧（跳过建会话时回报映射的 control_result）。
  const turnEnd = await nextTurnEnd(bridge)
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

  const turnEnd = await nextTurnEnd(bridge)
  assert.equal(turnEnd.frame, "turn_end")
  assert.equal(turnEnd.ticket, TICKET)
  assert.equal(turnEnd.error, undefined)
})

test("seedMaps 复用桥下发的旧会话映射，不再 createSession", async (t) => {
  const bridge = await new FakeBridge().start()
  const dsh = new ControllableDsh()
  const link = await connectedLink(bridge)
  t.after(async () => {
    link.close()
    await bridge.stop()
  })

  const driver = new TurnDriver({ dsh, log: () => {} })
  // 模拟桥 Ready 帧带回来的已落盘映射。delivery 的 session 是 "main"（见 fake-bridge）。
  driver.seedMaps([{ session: "main", harness_session: "session-acb-main-old" }])

  const runPromise = driver.run(delivery(), link)
  await waitFor(() => dsh.prompts.length === 1, "prompt 没被调")

  // 关键断言：用的是 seed 的旧会话，dsh 没有 createSession。
  assert.equal(dsh.prompts[0].sessionId, "session-acb-main-old", "应复用 seed 的会话 id")
  assert.equal(dsh.sessions.length, 0, "不应调用 createSession")

  dsh.lastFollow.push({ type: "turn/end", data: { turn: "t1" } })
  await runPromise
})

test("一轮失败后下次投递复用同一会话，不新建", async (t) => {
  const bridge = await new FakeBridge().start()
  const dsh = new ControllableDsh()
  const link = await connectedLink(bridge)
  t.after(async () => {
    link.close()
    await bridge.stop()
  })

  const driver = new TurnDriver({ dsh, log: () => {} })

  // 第一轮：推一个带 error reason 的 turn/end，让它失败。
  const first = driver.run(delivery(), link)
  await waitFor(() => dsh.prompts.length === 1, "第一轮 prompt 没被调")
  dsh.lastFollow.push({
    type: "turn/end",
    data: { turn: "t1", reason: { kind: "error", error: { message: "炸了" } } },
  })
  await first

  // 第二轮：同一会话再投一次。
  const second = driver.run(delivery(), link)
  await waitFor(() => dsh.prompts.length === 2, "第二轮 prompt 没被调")

  // 关键断言：两次 prompt 用的是同一个 sessionId，dsh 只 create 了一次。
  assert.equal(dsh.prompts[0].sessionId, dsh.prompts[1].sessionId, "失败后应复用同一会话")
  assert.equal(dsh.sessions.length, 1, "只应 createSession 一次")

  // 收尾：第二轮正常结束。
  dsh.lastFollow.push({ type: "turn/end", data: { turn: "t2" } })
  await second
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

// 为什么要有这条：dsh 把「上次用的模型」记在会话里，复用老会话时它优先于默认模型配置。
// 只改配置、不调 selectModel，老会话会悄悄继续打旧渠道（我们踩过一次：网关日志里还是旧模型）。
test("配了模型就给会话钉一次，同一会话不重复钉", async (t) => {
  const bridge = await new FakeBridge().start()
  const dsh = new ControllableDsh()
  const link = await connectedLink(bridge)
  t.after(async () => {
    link.close()
    await bridge.stop()
  })

  const driver = new TurnDriver({
    dsh,
    log: () => {},
    model: { provider: "trae", model: "deepseek-v4.1-flash", reasoningEffort: "high" },
  })

  const first = driver.run(delivery(), link)
  await waitFor(() => dsh.prompts.length === 1, "prompt 没被调")
  dsh.lastFollow.push({ type: "turn/end", data: { turn: "t1" } })
  await first

  assert.equal(dsh.modelPicks.length, 1, "第一轮应该钉一次模型")
  assert.deepEqual(dsh.modelPicks[0], {
    sessionId: "session-1",
    provider: "trae",
    model: "deepseek-v4.1-flash",
    reasoningEffort: "high",
  })

  // 第二轮：同一会话，不该再钉一次。
  const second = driver.run(delivery(), link)
  await waitFor(() => dsh.prompts.length === 2, "第二轮 prompt 没被调")
  dsh.lastFollow.push({ type: "turn/end", data: { turn: "t2" } })
  await second
  assert.equal(dsh.modelPicks.length, 1, "同一会话只该钉一次")
})

test("没配模型就不碰会话的模型选择", async (t) => {
  const bridge = await new FakeBridge().start()
  const dsh = new ControllableDsh()
  const link = await connectedLink(bridge)
  t.after(async () => {
    link.close()
    await bridge.stop()
  })

  // 只给 provider 不给 model：算没配全，不干预。
  const driver = new TurnDriver({ dsh, log: () => {}, model: { provider: "trae", model: "" } })
  const runPromise = driver.run(delivery(), link)
  await waitFor(() => dsh.prompts.length === 1, "prompt 没被调")
  dsh.lastFollow.push({ type: "turn/end", data: { turn: "t1" } })
  await runPromise

  assert.equal(dsh.modelPicks.length, 0, "没配模型就不该调 selectModel")
})

test("selectModel 失败不影响这一轮", async (t) => {
  const bridge = await new FakeBridge().start()
  const dsh = new ControllableDsh()
  dsh.selectModel = async () => {
    throw new Error("provider not found")
  }
  const link = await connectedLink(bridge)
  t.after(async () => {
    link.close()
    await bridge.stop()
  })

  const driver = new TurnDriver({
    dsh,
    log: () => {},
    model: { provider: "trae", model: "deepseek-v4.1-flash" },
  })
  const runPromise = driver.run(delivery(), link)
  await waitFor(() => dsh.prompts.length === 1, "钉模型失败也得照常投话")
  dsh.lastFollow.push({ type: "turn/end", data: { turn: "t1" } })
  await runPromise

  const turnEnd = await nextTurnEnd(bridge)
  assert.equal(turnEnd.frame, "turn_end")
  assert.equal(turnEnd.error, undefined, "钉模型失败不该让整轮算失败")
})
