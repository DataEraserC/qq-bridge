// acb 桥的协议客户端：一行一帧 JSON，桥那一侧是 `crates/kernel/src/endpoint.rs`，
// 帧的形状见 `docs/04-protocol.md`。
//
// 这个文件只负责「把话按协议说对」：握手、接轮次、发工具调用、收结果、断线重连。
// 它不认识 opencode，也不认识任何聊天平台——所以能单独测（`test/wire.test.mjs`
// 用一个假的桥验它）。opencode 那一侧的粘合全在 `acb.mjs`。

import net from "node:net"

/// 协议版本。和 `acb_proto::frame::PROTOCOL_VERSION` 必须一致。
export const PROTOCOL_VERSION = 4

const MAX_FRAME_BYTES = 1024 * 1024
const HANDSHAKE_TIMEOUT_MS = 10_000
const CALL_TIMEOUT_MS = 60_000
const RECONNECT_MIN_MS = 1_000
const RECONNECT_MAX_MS = 10_000

/** 工具调用失败。`code` 跟协议里的错误码一致（bad_request / unauthorized / ...）。 */
export class BridgeError extends Error {
  constructor(code, message) {
    super(message)
    this.name = "BridgeError"
    this.code = code
  }
}

/** 把 `127.0.0.1:7700` 拆成 host 和 port。只支持 TCP，桥这边没有 unix socket。 */
export function parseEndpoint(text) {
  const at = text.lastIndexOf(":")
  if (at < 0) {
    throw new Error(`endpoint 要形如 127.0.0.1:7700，收到的是 ${text}`)
  }
  const host = text.slice(0, at).trim() || "127.0.0.1"
  const port = Number(text.slice(at + 1))
  if (!Number.isInteger(port) || port <= 0 || port > 65535) {
    throw new Error(`endpoint 里的端口不对：${text}`)
  }
  return { host, port }
}

/**
 * 一条到桥的长连接。
 *
 * 用法：`onTurn` 收到一轮就把它交给 harness 干活，干完调 `endTurn`；中途要发消息、
 * 要确认水位就 `call("send_message", {...})`。桥保证同一设备同时只有一轮，所以
 * `onTurn` 不会并发。
 */
export class BridgeLink {
  constructor({ endpoint, token, harness, onTurn, onControl, log }) {
    if (!token) {
      throw new Error("没有接入令牌：要和配置里会话的 token 一致")
    }
    if (!harness) {
      throw new Error("没有 harness 名：要和配置里会话的 harness 一致")
    }
    parseEndpoint(endpoint) // 早报错，别等连的时候才炸
    this.endpoint = endpoint
    this.token = token
    this.harness = harness
    this.onTurn = onTurn ?? (async () => {})
    this.onControl = onControl ?? (async () => {})
    this.log = log ?? (() => {})

    this.sessions = []
    /** 桥下发的会话映射（acb 会话 ↔ harness 内部会话），握手后填充。 */
    this.sessionMaps = []
    /** 当前这一轮的票；没有票就没有进行中的轮次。 */
    this.ticket = null
    this.socket = null
    this.buffer = ""
    this.pending = new Map()
    this.seq = 0
    this.closed = false
    this.handshake = null
    this.reconnectTimer = null
    this.backoff = RECONNECT_MIN_MS
  }

  /** 连上去并完成握手。返回后就能收发帧；断线会自己在后台重连。 */
  async connect() {
    const { host, port } = parseEndpoint(this.endpoint)
    return new Promise((resolve, reject) => {
      const socket = net.createConnection({ host, port })
      socket.setNoDelay(true)
      socket.setEncoding("utf8")
      this.socket = socket

      const timer = setTimeout(() => {
        socket.destroy()
        this.#failHandshake(new BridgeError("timeout", "握手超时"))
      }, HANDSHAKE_TIMEOUT_MS)

      this.handshake = {
        resolve: () => {
          clearTimeout(timer)
          this.handshake = null
          resolve()
        },
        reject: (error) => {
          clearTimeout(timer)
          this.handshake = null
          reject(error)
        },
      }

      socket.on("connect", () => {
        this.#write({ frame: "hello", version: PROTOCOL_VERSION, harness: this.harness, token: this.token })
      })
      socket.on("data", (chunk) => this.#onData(chunk))
      socket.on("error", (error) => {
        // 连接被拒之类：握手还没完就先告诉调用方，完事了就只当断线处理。
        this.#failHandshake(new BridgeError("internal", `连不上桥：${error.message}`))
      })
      socket.on("close", () => this.#onClose())
    })
  }

  /** 主动断开，不再重连。 */
  close() {
    this.closed = true
    if (this.reconnectTimer) {
      clearTimeout(this.reconnectTimer)
      this.reconnectTimer = null
    }
    this.socket?.destroy()
    this.socket = null
    this.#rejectPending(new BridgeError("internal", "连接已关闭"))
  }

  /**
   * 调一个桥上的工具，等结果。
   *
   * 参数形状和协议一致（`docs/04-protocol.md` 的工具表），失败抛 {@link BridgeError}。
   */
  async call(tool, args = {}, { timeout = CALL_TIMEOUT_MS } = {}) {
    if (!this.socket || this.closed) {
      throw new BridgeError("internal", "和桥的连接不在了，这轮发不出去")
    }
    if (!this.ticket) {
      throw new BridgeError("unauthorized", "现在没有进行中的轮次，工具调不动")
    }
    const id = `c-${++this.seq}`
    const promise = new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id)
        reject(new BridgeError("timeout", `工具 ${tool} 等不到结果`))
      }, timeout)
      this.pending.set(id, { resolve, reject, timer })
    })
    this.#write({ frame: "tool_call", id, ticket: this.ticket, tool, args })
    return promise
  }

  /** 本轮收工。`error` 非空表示这轮出错了（桥只记日志，不影响水位）。 */
  endTurn(ticket = this.ticket, error = null) {
    if (!ticket) return
    this.#write({ frame: "turn_end", ticket, ...(error ? { error } : {}) })
    this.ticket = null
  }

  /**
   * 回报一条控制指令的结果（ADR-0018）。
   * `id`/`session`/`command` 原样带回来配对；`result` 是成功回的结构，`error` 是失败原因。
   */
  controlResult({ id, session, command, result = null, error = null }) {
    this.#write({
      frame: "control_result",
      id,
      session,
      command,
      ...(result !== null ? { result } : {}),
      ...(error !== null ? { error } : {}),
    })
  }

  /** 保活（桥也会自己 ping）。 */
  ping() {
    this.#write({ frame: "ping" })
  }

  // -------------------------------------------------------------------------
  // 内部
  // -------------------------------------------------------------------------

  #write(frame) {
    if (!this.socket) return
    const text =
      typeof frame === "string" ? frame : JSON.stringify(frame)
    try {
      this.socket.write(text.endsWith("\n") ? text : `${text}\n`)
    } catch (error) {
      this.#failHandshake(new BridgeError("internal", `写不出去：${error.message}`))
    }
  }

  #onData(chunk) {
    this.buffer += chunk
    if (this.buffer.length > MAX_FRAME_BYTES && !this.buffer.includes("\n")) {
      this.log("error", "收到的帧超过 1 MiB 还没有换行，断开")
      this.socket?.destroy()
      return
    }
    for (;;) {
      const at = this.buffer.indexOf("\n")
      if (at < 0) return
      const line = this.buffer.slice(0, at)
      this.buffer = this.buffer.slice(at + 1)
      if (!line.trim()) continue
      let frame
      try {
        frame = JSON.parse(line)
      } catch {
        this.log("error", `桥发来的不是 JSON：${line.slice(0, 200)}`)
        this.socket?.destroy()
        return
      }
      this.#onFrame(frame)
    }
  }

  #onFrame(frame) {
    switch (frame.frame) {
      case "ready":
        this.sessions = Array.isArray(frame.sessions) ? frame.sessions : []
        this.sessionMaps = Array.isArray(frame.maps) ? frame.maps : []
        this.backoff = RECONNECT_MIN_MS
        this.handshake?.resolve()
        return
      case "turn_start":
        this.ticket = frame.delivery.ticket
        // 不 await：读循环得继续跑，否则这一轮里的工具结果全卡在这里。
        Promise.resolve()
          .then(() => this.onTurn(frame.delivery))
          .catch((error) => {
            this.log("error", `这一轮跑挂了：${error?.message ?? error}`)
            const ticket = this.ticket
            this.ticket = null
            if (ticket) this.endTurn(ticket, String(error?.message ?? error))
          })
        return
      case "tool_result": {
        const waiting = this.pending.get(frame.id)
        if (!waiting) return
        this.pending.delete(frame.id)
        clearTimeout(waiting.timer)
        if (frame.error) {
          waiting.reject(new BridgeError(frame.error.code, frame.error.message))
        } else {
          waiting.resolve(frame.result ?? null)
        }
        return
      }
      case "log":
        this.log(frame.level ?? "info", frame.message ?? "")
        return
      case "control": {
        // 桥推来的控制指令（如 reset）。交给 harness 的 onControl 处理，处理完它要
        // 用 controlResult 回报结果。不 await，让读循环继续跑。
        Promise.resolve()
          .then(() => this.onControl(frame))
          .catch((error) => {
            this.log("error", `控制指令 ${frame.command} 处理失败：${error?.message ?? error}`)
            try {
              this.controlResult({
                id: frame.id,
                session: frame.session,
                command: frame.command,
                error: String(error?.message ?? error),
              })
            } catch (reportError) {
              this.log("error", `回报控制指令结果失败：${reportError?.message ?? reportError}`)
            }
          })
        return
      }
      case "ping":
        this.#write({ frame: "pong" })
        return
      case "pong":
        return
      default:
        this.log("warn", `不认识的帧：${frame.frame}`)
    }
  }

  #onClose() {
    this.socket = null
    this.ticket = null
    this.buffer = ""
    this.#rejectPending(new BridgeError("internal", "和桥的连接断了"))
    if (this.closed) return
    this.#failHandshake(new BridgeError("internal", "连接在握手阶段就断了"))
    this.log("warn", `桥断了，${Math.round(this.backoff / 1000)} 秒后重连`)
    this.reconnectTimer = setTimeout(() => {
      this.reconnectTimer = null
      this.connect()
        .then(() => this.log("info", `重连成功，重新绑定会话组 ${this.sessions.join("、")}`))
        .catch((error) => this.log("error", `重连失败：${error.message}`))
    }, this.backoff)
    this.backoff = Math.min(this.backoff * 2, RECONNECT_MAX_MS)
  }

  #failHandshake(error) {
    this.handshake?.reject(error)
  }

  #rejectPending(error) {
    for (const [, waiting] of this.pending) {
      clearTimeout(waiting.timer)
      waiting.reject(error)
    }
    this.pending.clear()
  }
}

// ---------------------------------------------------------------------------
// 把一轮投递渲染成给模型看的话
// ---------------------------------------------------------------------------

/**
 * 把 `delivery` 摊平成人能读、模型能直接吃的一段文本。
 *
 * 桥已经在重投的记录上写好了旁白（`note`：第 N 次投递 + 怎么止投），这里只做呈现，
 * 不再造信息；旁白永远单独一行，不拼进对方说的话里。
 */
/** 房间串：`连接:段/段`。核心和适配器都按这个写法互认。 */
export function roomText(room) {
  return `${room.connection}:${room.path.join("/")}`
}

export function renderTurn(delivery, { now = Date.now() } = {}) {
  const lines = []
  const room = roomText(delivery.room)
  const count = delivery.records.length
  const reason =
    delivery.reason === "clock" ? "定时例行检查" : "有新消息"

  if (count === 0) {
    lines.push(`[${room}] ${reason}，这一轮没有新记录。`)
    return lines.join("\n")
  }

  lines.push(`[${room}] ${reason}，${count} 条新记录：`)

  for (const record of delivery.records) {
    const when = formatTime(record.at, now)
    const who = record.sender.name || record.sender.id
    const marks = []
    // 身份标签：桥判得出来才标，判不出（role 为空）就什么都不加。
    const role = roleLabel(record.sender.role)
    if (role) marks.push(role)
    if (record.kind?.kind === "event") {
      marks.push(`事件 ${record.kind.event}`)
    }
    if (record.at_self) marks.push("冲你来的")
    // 引用我发的某条消息 = 在跟我说，但不是 @。分两个词，模型才知道对方是在追问哪件事。
    if (record.reply_to_self) marks.push("回你的话")
    // 桥一般会给一句 `note`（写在下面单独一行）。没有 note 但标了重投，就在标题里
    // 提一句，别让「这条重复了」这件事丢掉。
    if (record.redelivery && !record.note) {
      marks.push(`第 ${record.redelivery} 次投递`)
    }
    const tail = marks.length ? `（${marks.join("、")}）` : ""
    lines.push(`#${record.id} ${when} ${who}(${record.sender.id})${tail}`)
    lines.push(`  ${renderElements(record.elements ?? [])}`)
    // 桥的旁白不是对方说的话，单独一行，绝不拼进上面那行内容里。
    if (record.note) {
      lines.push(`  ※ ${record.note}`)
    }
  }

  lines.push("")
  // 消息太多被截断时，明确告诉模型：还有没投进来的，用 get_history 抓。
  if (delivery.more) {
    lines.push(
      `⚠️ 本轮还有 ${delivery.more} 条没投进来（超了投递上限）。用 get_history 抓这个房间最新的 ${delivery.more} 条再看。`
    )
    lines.push("")
  }
  lines.push("回这个房间用 send_message；确认处理完了用 ack_messages 带上这些编号。")
  return lines.join("\n")
}

/** 桥给的发送者身份 → 中文标签。判不出（空/不认识）返回空串。 */
function roleLabel(role) {
  switch (role) {
    case "system_admin":
      return "系统管理员"
    case "owner":
      return "群主"
    case "admin":
      return "群管理员"
    default:
      return ""
  }
}

function formatTime(at, now) {
  const date = new Date(at)
  if (Number.isNaN(date.getTime())) return "?"
  const sameDay = new Date(now).toDateString() === date.toDateString()
  const time = date.toTimeString().slice(0, 8)
  return sameDay ? time : `${date.getMonth() + 1}/${date.getDate()} ${time}`
}

/** 元素数组 → 一行文本。平台特有内容原样摊给模型看，不替它下结论。 */
export function renderElements(elements) {
  const parts = []
  for (const element of elements) {
    switch (element.type) {
      case "text":
        parts.push(element.text ?? "")
        break
      case "at":
        parts.push(element.all ? "@全体成员" : `@${element.target ?? "?"}`)
        break
      case "quote":
        parts.push(`[引用 #${element.message_id ?? "?"}]`)
        break
      case "image":
        parts.push(`[图片 ${element.url ?? ""}]`.trim())
        break
      case "audio":
        parts.push(`[语音 ${element.url ?? ""}]`.trim())
        break
      case "video":
        parts.push(`[视频 ${element.url ?? ""}]`.trim())
        break
      case "file":
        parts.push(`[文件 ${element.name ?? ""} ${element.url ?? ""}]`.trim())
        break
      case "face":
        parts.push(`[表情 ${element.id ?? "?"}]`)
        break
      case "opaque": {
        // 平台特有内容（QQ 合并转发、KOOK 卡片、Discord embed……）。
        // 桥只负责原样透传、不解释语义（ADR-0004 规则 3）；但「桥不解释」
        // 不等于「模型看不见」——这里是唯一送进对话的通道，必须把原始载荷摊开，
        // 否则模型只收到一句空占位，这段内容在对话里等于凭空消失。
        //
        // 另外要带上「可以怎么取」（ADR-0020）：只给一个 id，模型根本不知道自己能查，
        // 多半会回一句「你转了条消息我看不到」。
        const hint = affordanceOf(element)
        parts.push(
          `[${element.platform ?? "平台"} 平台特有内容${hint ? `（${hint}）` : ""}，原样如下：${shortJson(element.payload)}]`,
        )
        break
      }
      default:
        parts.push(`[不认识的内容 ${element.type ?? "?"}：${shortJson(element)}]`)
    }
  }
  return parts.join("")
}

/**
 * 平台特有内容 → 「可以怎么取」的一句话提示。
 *
 * 不主动展开正文（投递体积不可控、会撞平台限流、可能把别人的私聊带进不该看的群），
 * 也不指望模型自己去翻能力目录（它常常跳过这一步）。把动作写在内容旁边最省事。
 */
function affordanceOf(element) {
  const payload = element.payload ?? {}
  const kind = typeof payload.type === "string" ? payload.type : ""
  const id = payload.data?.id

  // 合并转发：OneBot 系都是这个形状。
  if (kind === "forward" && typeof id === "string" && id) {
    return `未展开，可调 platform_call(method="message.forward_content", params=${JSON.stringify({ id })}) 读取正文`
  }
  // 其它不认识的：至少告诉它去哪查，别让它瞎猜方法名。
  return "可调 platform_methods 看这个平台能做什么"
}

/** 平台载荷能有多长就多长地给模型看，但别让一条消息撑爆上下文。 */
const MAX_PLATFORM_JSON = 600

function shortJson(value) {
  let raw
  try {
    raw = JSON.stringify(value ?? null)
  } catch {
    raw = String(value) // 循环引用之类，退化成字符串，绝不因为序列化失败就把内容吞了
  }
  if (typeof raw !== "string") raw = String(raw)
  return raw.length > MAX_PLATFORM_JSON ? `${raw.slice(0, MAX_PLATFORM_JSON)}…（已截断）` : raw
}

// ---------------------------------------------------------------------------
// 工具参数：模型给的东西 → 桥要的东西
// ---------------------------------------------------------------------------

/** 标准元素表的类型名，和 `acb_proto::Element` 一一对应。 */
export const ELEMENT_TYPES = [
  "text",
  "at",
  "quote",
  "image",
  "audio",
  "video",
  "file",
  "face",
  "opaque",
]

/**
 * 把工具参数收拾成桥要的形状。
 *
 * 只做两件事：`send_message` / `send_message_to` 收 `elements`（`text` 是单段文本的糖），
 * 其余工具原样透传认识的字段。**不做语义校验**——房间对不对、票对不对，桥说了算，
 * 这里抢着判会把错误信息说不清楚。
 */
export function normalizeToolArgs(tool, args = {}) {
  const given = args && typeof args === "object" ? args : {}

  switch (tool) {
    case "send_message":
      return { elements: elementsOf(given) }
    case "send_message_to": {
      if (!given.to || typeof given.to !== "string") {
        throw new BridgeError(
          "bad_request",
          "send_message_to 要写 to，形如 qq:group/12345；只能是当前会话订阅过的房间",
        )
      }
      // 跨会话发送已砍：to 只落在当前会话订阅的房间，没有 device 身份一说。
      return { elements: elementsOf(given), to: given.to }
    }
    case "ack_messages": {
      const ids = Array.isArray(given.ids) ? given.ids : []
      if (ids.length === 0) {
        throw new BridgeError("bad_request", "ack_messages 要带 ids，就是投递记录里的编号")
      }
      const bad = ids.find((id) => !Number.isInteger(id))
      if (bad !== undefined) {
        throw new BridgeError("bad_request", `ids 只能是整数，收到 ${JSON.stringify(bad)}`)
      }
      return { ids }
    }
    default:
      return stripUndefined(given)
  }
}

function elementsOf(args) {
  if (Array.isArray(args.elements) && args.elements.length > 0) {
    for (const element of args.elements) {
      if (!element || typeof element !== "object") {
        throw new BridgeError("bad_request", "elements 里每一项都得是对象")
      }
      if (!ELEMENT_TYPES.includes(element.type)) {
        throw new BridgeError(
          "bad_request",
          `不认识的元素类型 ${JSON.stringify(element.type)}；能用的有：${ELEMENT_TYPES.join("、")}`,
        )
      }
    }
    return args.elements
  }
  if (typeof args.text === "string" && args.text.length > 0) {
    return [{ type: "text", text: args.text }]
  }
  throw new BridgeError("bad_request", "发消息至少要有 text，或者 elements 里的一段内容")
}

function stripUndefined(object) {
  const out = {}
  for (const [key, value] of Object.entries(object)) {
    if (value !== undefined && value !== null) out[key] = value
  }
  return out
}
