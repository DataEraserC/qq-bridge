// qqab 的 opencode 插件：把「桥推来的一轮」变成「opencode 会话里的一次提问」，
// 同时把桥的九个工具挂进这个 opencode 实例，让模型直接看见它们。
//
// 装法：把这个文件连同 `qqab-wire.mjs` 一起放进 `.opencode/plugins/`（或
// `~/.config/opencode/plugins/`），两个文件必须挨着放——前者 import 后者。
//
// 配置全走环境变量（插件没法读桥的配置文件，那是桥自己的事）：
//   QQAB_ENDPOINT  桥的接入点，默认 127.0.0.1:7700
//   QQAB_TOKEN     设备令牌，必填；和 config.toml 里设备的 token 一致
//   QQAB_SESSION   钉死用哪个 opencode 会话；不填就按设备自动找/建
//
// 设计上的取舍见 `docs/06-harness-adapters.md`：opencode 有插件机制，所以工具面和
// 轮次驱动都能在进程内做完，不用外部驱动。

import { tool } from "@opencode-ai/plugin"

import {
  BridgeError,
  BridgeLink,
  normalizeToolArgs,
  renderTurn,
} from "./qqab-wire.mjs"

const z = tool.schema

const SERVICE = "qqab"

export const QQabPlugin = async ({ client, directory }) => {
  const config = readConfig()

  const log = (level, message) => {
    // 插件的日志走 opencode 自己的日志，别往 stdout 扔——那是 TUI 的地盘。
    void client.app
      .log({ body: { service: SERVICE, level, message, extra: { directory } } })
      .catch(() => {})
  }

  if (!config.token) {
    log("warn", "没有 QQAB_TOKEN，qqab 插件不启用（桥那边这个设备就不会有 harness）")
    return {}
  }

  const driver = new TurnDriver({ client, log, sessionId: config.session })
  const link = new BridgeLink({
    endpoint: config.endpoint,
    token: config.token,
    log,
    onTurn: (delivery) => driver.run(delivery, link),
  })

  void link
    .connect()
    .then(() => log("info", `已接入桥，绑定设备 ${link.device}`))
    // 连不上不算致命：BridgeLink 会自己退避重连，桥晚上线也能接上。
    .catch((error) => log("warn", `暂时连不上桥：${error.message}，会一直重试`))

  return { tool: buildTools(link) }
}

// ---------------------------------------------------------------------------
// 一轮：投递 → 提问 → 收工
// ---------------------------------------------------------------------------

class TurnDriver {
  constructor({ client, log, sessionId }) {
    this.client = client
    this.log = log
    this.sessionId = sessionId || null
    this.opening = null
    /** 串行链：同一设备本来就串行，这里再兜一层，免得两条连接把两个提问叠一起。 */
    this.chain = Promise.resolve()
  }

  /** 接一轮，跑完（或跑挂）都保证回 `turn_end`，不然这个设备会一直占着。 */
  run(delivery, link) {
    const step = () => this.#one(delivery, link)
    this.chain = this.chain.then(step, step)
    return this.chain
  }

  async #one(delivery, link) {
    let failure = null
    try {
      const sessionID = await this.#session(delivery)
      const text = renderTurn(delivery)
      this.log(
        "debug",
        `开轮 ${delivery.ticket}：${delivery.room.platform}:${delivery.room.room}，${delivery.records.length} 条`,
      )
      // 这一步会一直等到这一轮助手回复完——所以它就是「轮次结束」的信号，
      // 不用再去盯 session.idle 事件。
      await this.client.session.prompt({
        path: { id: sessionID },
        body: { parts: [{ type: "text", text }] },
      })
    } catch (error) {
      failure = error?.message ?? String(error)
      this.log("error", `这一轮失败了：${failure}`)
      // 会话可能被删了：下次重建，别一直往一个死会话里灌。
      this.sessionId = null
    } finally {
      try {
        link.endTurn(delivery.ticket, failure)
      } catch (error) {
        this.log("warn", `回 turn_end 失败：${error.message}`)
      }
    }
  }

  /**
   * 这个设备的会话：优先用环境变量钉死的，否则按标题找上次那个，找不到就新建。
   *
   * 会话就是设备的记忆（ADR-0002：记忆按设备一份），所以重开 opencode 要能接上
   * 上次那条，不能每启动一次就失忆。
   */
  async #session(delivery) {
    if (this.sessionId) return this.sessionId
    if (this.opening) return this.opening

    const title = `qqab:${delivery.device}`
    this.opening = (async () => {
      const found = await this.#findSession(title)
      if (found) {
        this.sessionId = found
        this.log("info", `接上会话 ${found}（${title}）`)
        return found
      }
      const created = await this.client.session.create({ body: { title } })
      const id = created?.data?.id ?? created?.id
      if (!id) throw new Error("建会话没拿到 id，opencode 的返回形状可能变了")
      this.sessionId = id
      this.log("info", `新会话 ${id}（${title}）`)
      return id
    })()

    try {
      return await this.opening
    } finally {
      this.opening = null
    }
  }

  async #findSession(title) {
    try {
      const response = await this.client.session.list()
      const sessions = response?.data ?? response ?? []
      if (!Array.isArray(sessions)) return null
      const matched = sessions.filter((session) => session?.title === title)
      if (matched.length === 0) return null
      // 拿最近更新的那条：列表顺序没保证，自己按时间挑一遍更稳。
      const stamp = (session) =>
        Number(session?.time?.updated ?? session?.time?.created ?? 0)
      matched.sort((a, b) => stamp(b) - stamp(a))
      return matched[0].id ?? null
    } catch (error) {
      this.log("warn", `列会话失败（就当没有）：${error.message}`)
      return null
    }
  }
}

// ---------------------------------------------------------------------------
// 工具面：名字和参数跟桥上的 `TOOL_NAMES` 一一对应
// ---------------------------------------------------------------------------

function buildTools(link) {
  const tools = {}
  for (const spec of TOOL_SPECS) {
    tools[spec.name] = tool({
      description: spec.description,
      args: spec.args,
      async execute(args) {
        return callBridge(link, spec.name, args)
      },
    })
  }
  return tools
}

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
    return result === null || result === undefined
      ? "好了"
      : typeof result === "string"
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

const TOOL_SPECS = [
  {
    name: "send_message",
    description: `在当前轮次的房间发一条消息（谁问你就回哪儿）。
${ELEMENTS_DOC}
只发一段纯文本时可以只给 text。`,
    args: {
      elements: z.array(z.any()).optional().describe("内容段数组"),
      text: z.string().optional().describe("只发一段纯文本时的快捷写法"),
    },
  },
  {
    name: "send_message_to",
    description: `发到另一个房间（比如私聊汇报到群里）。参数和 send_message 一样，只多一个 to。
to 的写法是 平台:房间，例如 qq:group/158296748、qq:private/323622442。
只能发到这个设备订阅过的房间——订阅表在桥的配置里，改不了也不该改。`,
    args: {
      to: z.string().describe("目标房间，形如 qq:group/12345"),
      elements: z.array(z.any()).optional().describe("内容段数组"),
      text: z.string().optional().describe("只发一段纯文本时的快捷写法"),
    },
  },
  {
    name: "ack_messages",
    description: `确认这批记录处理完了，水位才会往前推。
不确认就一直算未读，下一轮还会再投一次（投递里会写「第 N 次投递」）。
ids 就是投递记录前面的编号。`,
    args: { ids: z.array(z.number()).describe("要确认的记录编号") },
  },
  {
    name: "get_history",
    description: "翻这个房间过去的记录（含平台事件），用来补上下文。",
    args: {
      room: z.string().optional().describe("房间；不填就是当前轮次的房间"),
      limit: z.number().optional().describe("最多几条，默认 20，上限 200"),
      before: z.number().optional().describe("只看编号比它小的（往前翻）"),
    },
  },
  {
    name: "list_rooms",
    description: "这个设备订阅了哪些房间，以及每个房间的未读情况。想发消息前先看能发到哪。",
    args: {},
  },
  {
    name: "get_notes",
    description: "查这个房间的笔记（长期记住的词、人、话题）。",
    args: {
      room: z.string().optional().describe("房间；不填就是当前轮次的房间"),
      kind: z.string().optional().describe("只看某种：term / member / topic"),
    },
  },
  {
    name: "set_note",
    description: `写一条笔记。kind=term 记词，member 记人，topic 记还没聊完的事
（topic 有状态：open / closed）。`,
    args: {
      room: z.string().optional().describe("房间；不填就是当前轮次的房间"),
      kind: z.string().optional().describe("默认 term"),
      key: z.string().describe("笔记的键"),
      value: z.string().describe("笔记的内容"),
      label: z.string().optional().describe("给人看的名字"),
      status: z.string().optional().describe("只有 topic 用：open / closed"),
    },
  },
  {
    name: "del_note",
    description: "删一条笔记。",
    args: {
      room: z.string().optional().describe("房间；不填就是当前轮次的房间"),
      kind: z.string().optional().describe("默认 term"),
      key: z.string().describe("笔记的键"),
    },
  },
  {
    name: "get_platform_events",
    description: "查平台事件（进群、退群、拍一拍之类），消息之外的那些动静。",
    args: {
      room: z.string().optional().describe("房间；不填就是当前轮次的房间"),
      limit: z.number().optional().describe("最多几条，默认 20"),
    },
  },
]

function readConfig() {
  return {
    endpoint: process.env.QQAB_ENDPOINT?.trim() || "127.0.0.1:7700",
    token: process.env.QQAB_TOKEN?.trim() || "",
    session: process.env.QQAB_SESSION?.trim() || "",
  }
}
