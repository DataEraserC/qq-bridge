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

const COLUMNS_DOC = `只返回这些列，写成字符串数组。
能查的列：id（水位编号）、room（房间）、kind（消息/事件）、sender（谁发的）、at（时间）、
elements（内容）、platform_id（平台原生消息 id，quote 引用要用它）、at_self（是否冲我）、
reply_to_self（是否回我）。
不填默认返回 id/at/sender/elements。填了非法列名会报错并列出合法列名。`

const COLUMNS_SCHEMA = {
  type: "array",
  description: COLUMNS_DOC,
  items: { type: "string" },
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
        columns: COLUMNS_SCHEMA,
      },
      additionalProperties: false,
    },
  },
  {
    name: "fetch_records",
    description: `按水位编号精确查一批记录，只返回你要的列。
${COLUMNS_DOC}
返回里每条记录会带一个 columns 字段，用 true/false 标明这一列这次取了没有——
false 是「这次没查」，不是「这条没这个值」，要的话把它加进 columns 再查一次。`,
    schema: {
      type: "object",
      properties: {
        room: ROOM_SCHEMA,
        ids: {
          type: "array",
          items: { type: "number" },
          description: "要查的水位编号（投递记录前面的 #编号）",
        },
        columns: COLUMNS_SCHEMA,
      },
      required: ["ids"],
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
    name: "platform_methods",
    description: `查这个平台还能干哪些消息之外的事（读合并转发的正文、查群成员、发图、审批入群……）。
桥只提供这一个目录和下面那个通用调用口——平台特有的能力都从这里查，不在工具列表里。

不给条件：列出全部方法（名字 + 一句话 + 标签），不带参数表。
给 group / tag / query：命中的方法带参数表。三者可以叠加。

方法名是「归属.动作」，归属跟平台 API 的命名空间对齐（forward.* / message.* / group.*）。
tags 是用途，跨归属：想找「图片相关」，用 tag="image"，不必知道平台把它放在哪个归属下。
query 按词搜名字、说明、标签，中文和英文都认（「图片」能搜到 image）。

清单里有些方法带 scope="connection"（好友列表、账号资料、群列表、点赞这类）——那是
「这个号上的事」，跟哪个房间无关，调它不用给 room。其余默认是房间里的事。

看到消息里带「可调 platform_call(...)」的提示时，直接照着调就行，不用先查目录。`,
    schema: {
      type: "object",
      properties: {
        room: ROOM_SCHEMA,
        group: { type: "string", description: "只看某个归属，如 forward、message、group" },
        tag: { type: "string", description: "只看带这个用途标签的，如 image、send、member、file" },
        query: { type: "string", description: "按关键词找方法（中英都认）" },
      },
      additionalProperties: false,
    },
  },
  {
    name: "platform_call",
    description: `调一个平台特有方法。core 不认识任何方法名，原样转给适配器。

method 用 resource.method 的写法，如 message.forward_content、group.get_member_info。
params 是对象，形状照 platform_methods 里给的参数表填；群号/私聊对象能从房间推出来时可以不填。

room 怎么给，看方法的 scope：
- 房间里的事（默认）：不给就是当前轮次的房间；给了就必须是会话订阅过的房间。
- scope="connection" 的事（好友列表、账号资料、群列表、点赞、链接安全检测）：跟房间无关，
  不用给 room。只有你同时盯了多个号、要说清是哪个号时才给一个那个号下的房间。
只能调这个连接声明过的方法。踢人、禁言、改群名这类破坏性操作默认不开放（报 forbidden）；
连接配置里开了就会真的执行，这时候你就是在替这个人操作，想清楚再调。

报错会带上分类：unknown_method（名字错了，会给你近似的）、bad_params（参数不对，
会说哪个字段）、unsupported_platform（这平台没这能力）、not_implemented（平台有但我们
还没写）、forbidden（没权限或没开）。照着改就行，别重复调同一个错的。`,
    schema: {
      type: "object",
      properties: {
        room: ROOM_SCHEMA,
        method: { type: "string", description: "方法名，如 message.send_group、group.get_member_info" },
        params: {
          type: "object",
          description: "方法参数。照 platform_methods 给的参数表填",
          additionalProperties: true,
        },
      },
      required: ["method"],
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
