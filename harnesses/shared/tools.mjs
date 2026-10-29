// 桥上的九个工具，名字、给人看的话、JSON Schema 都在这里。opencode 和 dsh
// 两个 harness 都从这里拿，保证工具面不漂移。
//
// schema 是 JSON Schema，两个 harness 各自适配字段名：
//   opencode v2 用 `input`，dsh 的 MCP 用 `inputSchema`。
// 内容完全一样。

export const ELEMENTS_DOC = `平台中立的内容段数组，例如
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

/** 九个工具。`schema` 是 JSON Schema，`name`/`description` 是给模型看的。 */
export const TOOLS = [
  {
    name: "send_message",
    description: `在当前轮次的房间发一条消息（谁问你就回哪儿）。
${ELEMENTS_DOC}
只发一段纯文本时可以只给 text。`,
    schema: {
      type: "object",
      properties: { elements: ELEMENTS_SCHEMA, text: TEXT_SCHEMA },
      additionalProperties: false,
    },
  },
  {
    name: "send_message_to",
    description: `发到另一个房间（比如私聊汇报到群里）。参数和 send_message 一样，只多一个 to。
to 的写法是 连接:段/段，例如 qq:group/158296748、qq:private/323622442。
只能发到「你这组设备订阅过的房间」——订阅表在桥的配置里，改不了也不该改。
同一个房间被本组两台设备都订了时，要用 device 指明用谁的身份说。`,
    schema: {
      type: "object",
      properties: {
        to: { type: "string", description: "目标房间，形如 qq:group/12345" },
        device: {
          type: "string",
          description: "用哪台设备的身份说；只在目标房间被本组多台设备订阅时才需要",
        },
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
    schema: {
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
    schema: {
      type: "object",
      properties: {
        room: ROOM_SCHEMA,
        limit: { type: "number", description: "最多几条，默认 20，上限 200" },
        before: { type: "number", description: "只看编号比它小的（往前翻）" },
      },
      additionalProperties: false,
    },
  },
  {
    name: "list_rooms",
    description:
      "你这组设备订阅了哪些房间、各自未读多少。想发消息前先看能发到哪（一行一台设备的视角）。",
    schema: { type: "object", properties: {}, additionalProperties: false },
  },
  {
    name: "get_notes",
    description: "查这个房间的笔记（长期记住的词、人、话题）。",
    schema: {
      type: "object",
      properties: {
        room: ROOM_SCHEMA,
        kind: { type: "string", description: "只看某种：term / member / topic" },
      },
      additionalProperties: false,
    },
  },
  {
    name: "set_note",
    description: `写一条笔记。kind=term 记词，member 记人，topic 记还没聊完的事
（topic 有状态：open / closed）。`,
    schema: {
      type: "object",
      properties: {
        room: ROOM_SCHEMA,
        kind: { type: "string", description: "默认 term" },
        key: { type: "string", description: "笔记的键" },
        value: { type: "string", description: "笔记的内容" },
        label: { type: "string", description: "给人看的名字" },
        status: { type: "string", description: "只有 topic 用：open / closed" },
      },
      required: ["key", "value"],
      additionalProperties: false,
    },
  },
  {
    name: "del_note",
    description: "删一条笔记。",
    schema: {
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
    schema: {
      type: "object",
      properties: {
        room: ROOM_SCHEMA,
        limit: { type: "number", description: "最多几条，默认 20" },
      },
      additionalProperties: false,
    },
  },
]

/** 名字 → 工具，方便按名查。 */
export const TOOLS_BY_NAME = new Map(TOOLS.map((t) => [t.name, t]))
