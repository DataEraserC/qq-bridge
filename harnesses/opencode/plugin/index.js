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
  renderTurn,
} from "./acb-wire.mjs"

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
      .then(() => log("info", `已接入桥，绑定会话组 ${link.sessions.join("、")}`))
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
  "  先想清楚要发什么，再一次调用，别先用原生工具名硬调。）",
].join("\n")

class TurnDriver {
  constructor({ ctx, log, pinned }) {
    this.ctx = ctx
    this.log = log
    this.pinned = pinned || ""
    this.sessions = new Map()
    /** 串行链：同一会话本来就串行，这里再兜一层，免得两轮叠在一起。 */
    this.chain = Promise.resolve()
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
      const text = renderTurn(delivery) + CODE_MODE_HINT
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

const ELEMENTS_DOC = `平台中立的内容段数组，例如
  [{"type":"text","text":"在的"}]
  [{"type":"text","text":"看图"},{"type":"image","url":"https://..."}]
  [{"type":"at","target":"10001"},{"type":"text","text":" 你说啥"}]
能用的类型：text / at / quote / image / audio / video / file / face / opaque。
平台做不了的类型会被适配器降级或明确报错，不会被悄悄丢掉。`

const ELEMENTS_SCHEMA = {
  type: "array",
  description: "内容段数组",
  items: { type: "object", additionalProperties: true },
}

const TEXT_SCHEMA = {
  type: "string",
  description: "只发一段纯文本时的快捷写法",
}

const ROOM_SCHEMA = {
  type: "string",
  description: "房间，形如 qq:group/123；不填就是当前轮次的房间",
}

const TOOL_SPECS = [
  {
    name: "send_message",
    description: `在当前轮次的房间发一条消息（谁问你就回哪儿）。
${ELEMENTS_DOC}
只发一段纯文本时可以只给 text。`,
    input: {
      type: "object",
      properties: { elements: ELEMENTS_SCHEMA, text: TEXT_SCHEMA },
      additionalProperties: false,
    },
  },
  {
    name: "send_message_to",
    description: `发到另一个房间。参数和 send_message 一样，只多一个 to。
to 的写法是 连接:段/段，例如 qq:group/158296748、qq:private/323622442。
只能发到「当前会话订阅过的房间」——订阅表在桥的配置里，改不了也不该改。
跨会话发送已经砍掉：你不能替别的人格/会话发言。`,
    input: {
      type: "object",
      properties: {
        to: { type: "string", description: "目标房间，形如 qq:group/12345" },
        elements: ELEMENTS_SCHEMA,
        text: TEXT_SCHEMA,
      },
      required: ["to"],
      additionalProperties: false,
    },
  },
  {
    name: "ack_messages",
    description: `确认这批记录处理完了，水位才会往前推。
不确认就一直算未读，下一轮还会再投一次（投递里会写「第 N 次投递」）。
ids 就是投递记录前面的编号。`,
    input: {
      type: "object",
      properties: {
        ids: {
          type: "array",
          items: { type: "number" },
          description: "要确认的记录编号",
        },
      },
      required: ["ids"],
      additionalProperties: false,
    },
  },
  {
    name: "get_history",
    description: "翻这个房间过去的记录（含平台事件），用来补上下文。",
    input: {
      type: "object",
      properties: {
        room: ROOM_SCHEMA,
        limit: { type: "number", description: "最多几条，默认 20，上限 200" },
        before: {
          type: "number",
          description: "只看编号比它小的（往前翻）",
        },
      },
      additionalProperties: false,
    },
  },
  {
    name: "list_rooms",
    description:
      "这个会话订阅了哪些房间、各自未读多少。想发消息前先看能发到哪。",
    input: { type: "object", properties: {}, additionalProperties: false },
  },
  {
    name: "get_notes",
    description: "查这个房间的笔记（长期记住的词、人、话题）。",
    input: {
      type: "object",
      properties: {
        room: ROOM_SCHEMA,
        kind: {
          type: "string",
          description: "只看某种：term / member / topic",
        },
      },
      additionalProperties: false,
    },
  },
  {
    name: "set_note",
    description: `写一条笔记。kind=term 记词，member 记人，topic 记还没聊完的事
（topic 有状态：open / closed）。`,
    input: {
      type: "object",
      properties: {
        room: ROOM_SCHEMA,
        kind: { type: "string", description: "默认 term" },
        key: { type: "string", description: "笔记的键" },
        value: { type: "string", description: "笔记的内容" },
        label: { type: "string", description: "给人看的名字" },
        status: {
          type: "string",
          description: "只有 topic 用：open / closed",
        },
      },
      required: ["key", "value"],
      additionalProperties: false,
    },
  },
  {
    name: "del_note",
    description: "删一条笔记。",
    input: {
      type: "object",
      properties: {
        room: ROOM_SCHEMA,
        kind: { type: "string", description: "默认 term" },
        key: { type: "string", description: "笔记的键" },
      },
      required: ["key"],
      additionalProperties: false,
    },
  },
  {
    name: "get_platform_events",
    description: "查平台事件（进群、退群、拍一拍之类），消息之外的那些动静。",
    input: {
      type: "object",
      properties: {
        room: ROOM_SCHEMA,
        limit: { type: "number", description: "最多几条，默认 20" },
      },
      additionalProperties: false,
    },
  },
  {
    name: "memory_read",
    description: `读记忆仓库里的一个文件。memory 是记忆 id（投递里的 memories 字段列了
本会话能碰哪些），path 是 worktree 内的相对路径。`,
    input: {
      type: "object",
      properties: {
        memory: { type: "string", description: "记忆 id" },
        path: { type: "string", description: "相对 worktree 的路径" },
      },
      required: ["memory"],
      additionalProperties: false,
    },
  },
  {
    name: "memory_write",
    description: `写记忆仓库里的一个文件（覆盖）。memory 是记忆 id，path 是相对路径，
content 是内容。父目录不存在会自动建。`,
    input: {
      type: "object",
      properties: {
        memory: { type: "string", description: "记忆 id" },
        path: { type: "string", description: "相对 worktree 的路径" },
        content: { type: "string", description: "要写的内容" },
      },
      required: ["memory", "path", "content"],
      additionalProperties: false,
    },
  },
  {
    name: "memory_list",
    description: "列记忆仓库里某个目录下的条目。memory 是记忆 id，path 默认仓库根。",
    input: {
      type: "object",
      properties: {
        memory: { type: "string", description: "记忆 id" },
        path: { type: "string", description: "相对路径，默认根" },
      },
      required: ["memory"],
      additionalProperties: false,
    },
  },
  {
    name: "memory_search",
    description: "在记忆仓库里搜文本（grep）。返回命中行「文件:行号:内容」。",
    input: {
      type: "object",
      properties: {
        memory: { type: "string", description: "记忆 id" },
        path: { type: "string", description: "搜索起点，默认仓库根" },
        needle: { type: "string", description: "要搜的关键字" },
      },
      required: ["memory", "needle"],
      additionalProperties: false,
    },
  },
  {
    name: "memory_git_status",
    description: "看记忆仓库的 git 状态（哪些文件改过、新增、删除）。",
    input: {
      type: "object",
      properties: { memory: { type: "string", description: "记忆 id" } },
      required: ["memory"],
      additionalProperties: false,
    },
  },
  {
    name: "memory_git_log",
    description: "看记忆仓库最近的提交记录。",
    input: {
      type: "object",
      properties: {
        memory: { type: "string", description: "记忆 id" },
        limit: { type: "number", description: "看最近几条，默认 10" },
      },
      required: ["memory"],
      additionalProperties: false,
    },
  },
  {
    name: "memory_git_diff",
    description: "看记忆仓库工作区相对最近提交的改动。",
    input: {
      type: "object",
      properties: { memory: { type: "string", description: "记忆 id" } },
      required: ["memory"],
      additionalProperties: false,
    },
  },
  {
    name: "memory_git_blame",
    description: "查记忆仓库里某个文件每一行是谁改的。",
    input: {
      type: "object",
      properties: {
        memory: { type: "string", description: "记忆 id" },
        path: { type: "string", description: "相对路径" },
      },
      required: ["memory", "path"],
      additionalProperties: false,
    },
  },
  {
    name: "memory_git_add",
    description: "暂存记忆仓库的改动。path 留空表示暂存全部。",
    input: {
      type: "object",
      properties: {
        memory: { type: "string", description: "记忆 id" },
        path: { type: "string", description: "相对路径，留空=全部" },
      },
      required: ["memory"],
      additionalProperties: false,
    },
  },
  {
    name: "memory_git_commit",
    description: "提交记忆仓库已暂存的改动。",
    input: {
      type: "object",
      properties: {
        memory: { type: "string", description: "记忆 id" },
        message: { type: "string", description: "提交信息" },
      },
      required: ["memory", "message"],
      additionalProperties: false,
    },
  },
  {
    name: "memory_git_push",
    description: "推送记忆仓库到默认远端（origin）。",
    input: {
      type: "object",
      properties: { memory: { type: "string", description: "记忆 id" } },
      required: ["memory"],
      additionalProperties: false,
    },
  },
  {
    name: "memory_git_branch",
    description: "列出记忆仓库的本地分支。",
    input: {
      type: "object",
      properties: { memory: { type: "string", description: "记忆 id" } },
      required: ["memory"],
      additionalProperties: false,
    },
  },
]

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
