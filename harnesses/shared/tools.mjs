// 桥上的工具（消息工具 + 记忆工具），名字、给人看的话、JSON Schema 都在这里。
// opencode 和 dsh 两个 harness 都从这里拿，保证工具面不漂移。
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

/** 全部工具。`schema` 是 JSON Schema，`name`/`description` 是给模型看的。 */
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
    description: `发到另一个房间。参数和 send_message 一样，只多一个 to。
to 的写法是 连接:段/段，例如 qq:group/158296748、qq:private/323622442。
只能发到「当前会话订阅过的房间」——订阅表在桥的配置里，改不了也不该改。
跨会话发送已经砍掉：你不能替别的人格/会话发言。`,
    schema: {
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
  {
    name: "memory_read",
    description: `读记忆仓库里的一个文件。memory 是记忆 id（投递里的 memories 字段列了
本会话能碰哪些），path 是 worktree 内的相对路径。`,
    schema: {
      type: "object",
      properties: {
        memory: { type: "string", description: "记忆 id" },
        path: { type: "string", description: "相对 worktree 的路径，如 shared.md 或 notes/foo.md" },
      },
      required: ["memory"],
      additionalProperties: false,
    },
  },
  {
    name: "memory_write",
    description: `写记忆仓库里的一个文件（覆盖）。memory 是记忆 id，path 是相对路径，
content 是内容。父目录不存在会自动建。`,
    schema: {
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
    schema: {
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
    schema: {
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
    schema: {
      type: "object",
      properties: {
        memory: { type: "string", description: "记忆 id" },
      },
      required: ["memory"],
      additionalProperties: false,
    },
  },
  {
    name: "memory_git_log",
    description: "看记忆仓库最近的提交记录。",
    schema: {
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
    schema: {
      type: "object",
      properties: {
        memory: { type: "string", description: "记忆 id" },
      },
      required: ["memory"],
      additionalProperties: false,
    },
  },
  {
    name: "memory_git_blame",
    description: "查记忆仓库里某个文件每一行是谁改的。",
    schema: {
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
    schema: {
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
    schema: {
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
    schema: {
      type: "object",
      properties: {
        memory: { type: "string", description: "记忆 id" },
      },
      required: ["memory"],
      additionalProperties: false,
    },
  },
  {
    name: "memory_git_branch",
    description: "列出记忆仓库的本地分支。",
    schema: {
      type: "object",
      properties: {
        memory: { type: "string", description: "记忆 id" },
      },
      required: ["memory"],
      additionalProperties: false,
    },
  },
]

/** 名字 → 工具，方便按名查。 */
export const TOOLS_BY_NAME = new Map(TOOLS.map((t) => [t.name, t]))
