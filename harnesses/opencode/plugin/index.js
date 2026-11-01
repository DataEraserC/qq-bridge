// acb 的 opencode v2 插件：把「桥推来的一轮」变成「opencode 会话里的一次提问」，
// 同时把桥的九个工具挂进这个实例，让模型直接看见它们。
//
// 装法：把这个目录整体放到 `.opencode/plugins/acb/`（或 `~/.config/opencode/plugins/acb/`）。
// v2 认目录里的 `index.js`，两个文件必须挨着——`index.js` 会 import `acb-wire.mjs`。
//
// 配置走插件选项（opencode.jsonc）或环境变量，选项优先：
//   endpoint  桥的接入点，默认 127.0.0.1:7700
//   token     设备令牌，必填；和 config.toml 里设备的 token 一致
//   session   钉死用哪个会话；不填就按设备找上次那个
//
// 为什么是这种写法：opencode v2 的插件是 `export default { id, setup(ctx) }`，
// 工具用 `ctx.tool.transform` 注册、参数是 JSON Schema。v1 的
// `export const X = async ({client}) => ({tool})` 在 v2 里**不会被加载**（实测），
// 所以这里不兼容 v1——见 docs/06 与 ADR-0013。

import {
  BridgeError,
  BridgeLink,
  normalizeToolArgs,
  personaSignature,
  renderPersona,
  renderTurn,
} from "./acb-wire.mjs"
import { TOOLS } from "../../shared/tools.mjs"

const SERVICE = "acb"

export default {
  id: SERVICE,

  async setup(ctx) {
    const config = readConfig(ctx)
    const log = makeLogger(ctx)

    if (!config.token) {
      log("warn", "没有令牌（ctx.options.token 或 ACB_TOKEN），acb 不启用")
      return
    }

    const driver = new TurnDriver({ ctx, log, pinned: config.session })
    const link = new BridgeLink({
      endpoint: config.endpoint,
      token: config.token,
      harness: config.harness,
      log,
      onTurn: (delivery) => driver.run(delivery, link),
      onControl: (frame) => driver.handleControl(frame, link),
    })

    // 工具面先挂上：轮次来之前模型就该看得见它们。
    const registration = await ctx.tool.transform((editor) => {
      for (const spec of TOOL_SPECS) {
        editor.add({
          name: spec.name,
          description: spec.description,
          input: spec.input,
          execute: async (input) => ({
            content: await callBridge(link, spec.name, input ?? {}),
          }),
        })
      }
    })

    void link
      .connect()
      .then(async () => {
        log("info", `已接入桥，绑定会话组 ${link.sessions.join("、")}`)
        // 把 storage 里还活着的映射主动报给桥：控制台的「harness 侧映射」
        // 靠 report_map 落盘，不报的话页面上永远是空、也没法断开。
        await driver.reportStoredMaps(link)
      })
      .catch((error) => log("warn", `暂时连不上桥：${error.message}，会一直重试`))

    return async () => {
      link.close()
      log("warn", "插件被卸载：已断开桥，不再重连。若进程还活着但不再收消息，多半是这里")
      try {
        await registration?.dispose?.()
      } catch {
        // 卸载时 dispose 失败不值得让进程起不来。
      }
    }
  },
}

// ---------------------------------------------------------------------------
// 一轮：投递 → 提问 → 等它说完 → 收工
// ---------------------------------------------------------------------------

/**
 * opencode v2 里插件注册的工具**不在**模型的原生工具列表里，而是进「Code Mode」目录，
 * 模型得用 `execute` 工具跑一行 JS 才调得动：`await tools.send_message({ text: "..." })`。
 *
 * 实测（2026-10-29，nemotron-3-ultra）：模型看到投递里「回这个房间用 send_message」，
 * 第一反应是直接调 `send_message`，得到
 * `No tool named "send_message" is currently available`，第二次才想起来走 `execute`。
 * 每轮白失败一次很浪费，所以在投递末尾把这条规矩直接说清楚。
 * 这是 opencode 独有的，属于适配器的事，不往通用的 renderTurn 里塞。
 */
const CODE_MODE_HINT = [
  "",
  "---",
  "（opencode 提示：上列工具不在原生工具列表里，要用 execute 跑一行 JS：",
  '  发消息 `await tools.send_message({ text: "收到" })`；',
  "  确认 `await tools.ack_messages({ ids: [1, 2] })`；",
  "  先想清楚要发什么，再一次调用，别先用原生工具名硬调。",
  "",
  "  平台特有的能力（读合并转发正文、查群成员、发图、审批入群）从目录查，别自己猜：",
  '  `await tools.platform_methods({ query: "图片" })` 看有哪些方法、参数怎么填，',
  '  再 `await tools.platform_call({ method: "message.send_image", params: {...} })`。',
  "  别用 fetch 直接连桥或聊天平台的端口：那样绕过了订阅校验和水位，发出的消息",
  "  不进归档，下一轮你对这段对话就失忆了。）",
].join("\n")

class TurnDriver {
  constructor({ ctx, log, pinned }) {
    this.ctx = ctx
    this.log = log
    this.pinned = pinned || ""
    this.sessions = new Map()
    /** 已向桥 report_map 过的会话 → opencode 会话 id，防重复回报。 */
    this.reported = new Map()
    /** 会话 id → 已注入过的人格签名；人格只在首轮/变更时注入一次。 */
    this.personas = new Map()
    /** 串行链：同一会话本来就串行，这里再兜一层，免得两轮叠在一起。 */
    this.chain = Promise.resolve()
  }

  /**
   * 把 storage 里还活着的映射主动报给桥（report_map）。连接建立后调一次，
   * 让控制台立刻看得见 harness 侧映射——哪怕一个轮次都还没跑过。
   */
  async reportStoredMaps(link) {
    for (const session of link.sessions) {
      if (this.pinned) continue
      try {
        const stored = await this.#storageGet(`session:${session}`)
        if (!stored || !(await this.#alive(stored))) continue
        this.sessions.set(session, stored)
        this.#reportMap(session, stored, link)
      } catch (error) {
        this.log("warn", `回报会话 ${session} 的映射失败：${error?.message ?? error}`)
      }
    }
  }

  /** 向桥回报映射（桥侧落盘，控制台可见）。同一个 (会话, id) 只报一次。 */
  #reportMap(session, id, link) {
    if (this.reported.get(session) === id) return
    this.reported.set(session, id)
    try {
      link.controlResult({
        // proto 里 control_result 的 id 是必需字段（缺了整帧被拒、连接被掐）；
        // 主动回报没有原帧可配对，学 dsh 用合成 id。
        id: `map-${Date.now()}`,
        session,
        command: "report_map",
        result: { session_id: id, meta: { title: `acb:${session}` } },
      })
      this.log("info", `已向桥回报映射 ${session} → ${id}`)
    } catch (error) {
      this.log("warn", `回报映射失败：${error?.message ?? error}`)
    }
  }

  /**
   * 桥推来的控制指令。目前只认 `reset`：忘掉旧映射（storage 一起清），
   * 立刻新建一条干净的 opencode 会话并把新 id 报回桥——桥删旧映射、
   * 存新映射，控制台马上能看到换好的映射。
   */
  async handleControl(frame, link) {
    const { id, session, command } = frame
    try {
      if (command !== "reset") throw new Error(`不认识的指令 ${command}`)
      this.#forget(session)
      let newId = ""
      if (this.pinned) {
        this.log("warn", `会话 ${session} 钉死了 opencode 会话（${this.pinned}），reset 不换会话`)
      } else {
        // storage 刚清掉，#session 在这里必然走新建分支。
        newId = await this.#session(session)
      }
      link.controlResult({
        id,
        session,
        command,
        result: { ok: true, ...(newId ? { session_id: newId } : {}) },
      })
      this.log("info", `会话 ${session} 已重置${newId ? ` → 新会话 ${newId}` : ""}`)
    } catch (error) {
      const message = error?.message ?? String(error)
      this.log("error", `重置会话 ${session} 失败：${message}`)
      link.controlResult({ id, session, command, error: message })
    }
  }

  run(delivery, link) {
    const step = () => this.#one(delivery, link)
    this.chain = this.chain.then(step, step)
    return this.chain
  }

  async #one(delivery, link) {
    let failure = null
    try {
      const sessionID = await this.#session(delivery.session)
      // 映射（新建或接上的）报给桥，控制台才看得到 harness 侧映射。
      this.#reportMap(delivery.session, sessionID, link)
      let text = renderTurn(delivery) + CODE_MODE_HINT
      // 人格只在首轮或变更时注入：桥只给语义，opencode 这边把它放在本轮最前面。
      // 重复注入会白烧 token，所以按签名去重。
      const personaText = renderPersona(delivery.persona)
      const signature = personaSignature(delivery.persona)
      if (personaText && this.personas.get(delivery.session) !== signature) {
        const updated = this.personas.has(delivery.session) ? "[人格已更新]\n" : ""
        text = `${updated}${personaText}\n\n${text}`
        this.personas.set(delivery.session, signature)
        this.log("info", `会话 ${delivery.session} 注入人格 ${delivery.persona?.id ?? ""}`)
      }
      const info = delivery.session_info ?? {}
      const model = info.model ?? "(opencode 默认)"
      this.log(
        "debug",
        `开轮 ${delivery.ticket}：${delivery.room.connection}:${delivery.room.path.join("/")}，${delivery.records.length} 条，模型 ${model}`,
      )
      // prompt 把话排进会话；wait 等这一轮助手真正说完。
      await this.ctx.session.prompt({ sessionID, text })
      if (typeof this.ctx.session.wait === "function") {
        await this.ctx.session.wait({ sessionID })
      }
    } catch (error) {
      failure = error?.message ?? String(error)
      this.log("error", `这一轮失败了：${failure}`)
      // 会话可能被删了：下次重建，别一直往一个死会话里灌。
      this.#forget(delivery.session)
    } finally {
      try {
        link.endTurn(delivery.ticket, failure)
      } catch (error) {
        this.log("warn", `回 turn_end 失败：${error.message}`)
      }
    }
  }

  /**
   * 这个会话的 opencode 会话：钉死的 > 记住的 > 新建。
   *
   * 串行键是会话（ADR-0017），这里按会话 id 找/建 opencode 会话，保证同一会话
   * 的轮次都落在同一条 opencode 会话里。重开 opencode 要能接上上次那条——v2 没有
   * 「按标题列会话」，把会话 id 存进插件自己的 storage。
   */
  async #session(session) {
    if (this.pinned) return this.pinned

    // 缓存里的也要确认还活着：用户可能在 opencode 里把会话删了。
    const cached = this.sessions.get(session)
    if (cached && (await this.#alive(cached))) return cached
    if (cached) this.#forget(session)

    const stored = await this.#storageGet(`session:${session}`)
    if (stored && (await this.#alive(stored))) {
      this.sessions.set(session, stored)
      this.log("info", `接上会话 ${stored}（acb:${session}）`)
      return stored
    }

    const created = await this.ctx.session.create({ title: `acb:${session}` })
    const id =
      created?.id ??
      created?.sessionID ??
      created?.session?.id ??
      created?.data?.id
    if (!id) {
      throw new Error(
        `建会话没拿到 id，opencode 的返回形状可能变了：${JSON.stringify(created).slice(0, 200)}`,
      )
    }
    this.sessions.set(session, id)
    await this.#storageSet(`session:${session}`, id)
    this.log("info", `新会话 ${id}（acb:${session}）`)
    return id
  }

  #forget(session) {
    this.sessions.delete(session)
    this.reported.delete(session)
    // 新 opencode 会话的上下文是空的，人格要重新注入。
    this.personas.delete(session)
    void this.#storageSet(`session:${session}`, null)
  }

  async #alive(sessionID) {
    try {
      await this.ctx.session.get({ sessionID })
      return true
    } catch {
      return false
    }
  }

  async #storageGet(key) {
    try {
      const value = await this.ctx.storage?.get?.(key)
      return value ?? null
    } catch {
      return null
    }
  }

  async #storageSet(key, value) {
    try {
      if (value === null) await this.ctx.storage?.remove?.(key)
      else await this.ctx.storage?.set?.(key, value)
    } catch {
      // 记不住就退回内存里那份，不值得把这一轮弄挂。
    }
  }
}

// ---------------------------------------------------------------------------
// 工具面：名字和参数跟桥上的工具一一对应
// ---------------------------------------------------------------------------

async function callBridge(link, name, args) {
  let wire
  try {
    wire = normalizeToolArgs(name, args)
  } catch (error) {
    // 本地就能看出来的错，别浪费一个来回。
    throw new Error(error.message)
  }
  try {
    const result = await link.call(name, wire)
    if (result === null || result === undefined) return "好了"
    return typeof result === "string"
      ? result
      : JSON.stringify(result, null, 2)
  } catch (error) {
    const code = error instanceof BridgeError ? error.code : "internal"
    throw new Error(`${name} 没成（${code}）：${error.message}`)
  }
}


// 工具面只在 shared/tools.mjs 维护一份（dsh 也从那里拿），这边只把 `schema` 改名成
// opencode 认的 `input`。
//
// 以前这里是一份手抄的副本，实测已经漂移：共享版早就加了 fetch_records，这边没有，
// 而投递文本里却在提示模型「用 fetch_records 抓剩下的」——模型照着调只会撞一个
// 不存在的工具。别再抄第二份。
const TOOL_SPECS = TOOLS.map(({ name, description, schema }) => ({
  name,
  description,
  input: schema,
}))


function readConfig(ctx) {
  const options = ctx?.options ?? {}
  return {
    endpoint:
      (typeof options.endpoint === "string" && options.endpoint.trim()) ||
      process.env.ACB_ENDPOINT?.trim() ||
      "127.0.0.1:7700",
    token:
      (typeof options.token === "string" && options.token.trim()) ||
      process.env.ACB_TOKEN?.trim() ||
      "",
    harness:
      (typeof options.harness === "string" && options.harness.trim()) ||
      process.env.ACB_HARNESS?.trim() ||
      "opencode",
    session:
      (typeof options.session === "string" && options.session.trim()) || "",
  }
}

function makeLogger(ctx) {
  const line = `[${SERVICE}]`
  return (level, message) => {
    // 固定写一份到 stderr：实测 2.0.22 在 `serve --stdio` 下 `app.log` 不落盘
    // （opencode.log 里搜不到任何 service= 的行），只靠它等于把诊断信息扔掉。
    // stdout 一个字都不能碰——那是 RPC 通道，写进去会让 opencode 报 socket 意外关闭。
    try {
      console.error(`${line} ${level} ${message}`)
    } catch {
      // 连 stderr 都不可用就算了，日志绝不能把插件弄挂。
    }
    const app = ctx?.app
    if (typeof app?.log === "function") {
      Promise.resolve(
        app.log({ body: { service: SERVICE, level, message } }),
      ).catch(() => {})
    }
  }
}
