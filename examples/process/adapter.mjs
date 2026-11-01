#!/usr/bin/env node
// 最小外置进程适配器（示例）。
//
// 协议：stdin/stdout 一行一条 JSON。第一行必须是 hello，之后 core 发 send / call /
// shutdown，适配器按 id 回 reply。完整说明见 docs/05-adapters.md。
//
// 用法见 examples/README.md：连接的 kind = "process"、transport = "process"，
// options.command = ["node", "examples/process/adapter.mjs"]。

import readline from "node:readline"

const AT = 1_700_000_000_000

function write(message) {
  process.stdout.write(`${JSON.stringify(message)}\n`)
}

function catalog() {
  const text = { name: "text", ty: "string", required: true, desc: "正文", values: [] }
  return [
    {
      name: "example.echo",
      summary: "回显调用参数",
      tags: ["read"],
      risk: "read",
      scope: "room",
      params: [text],
      targets: [],
      effect: "read",
      record: "none",
    },
    {
      name: "example.send",
      summary: "假装发一条消息并写流水",
      tags: ["send", "message"],
      risk: "write",
      scope: "room",
      params: [text],
      targets: [],
      effect: "send",
      record: "message",
    },
  ]
}

function reply(id, result) {
  write({ type: "reply", id, result })
}

function error(id, kind, detail) {
  reply(id, { outcome: "error", error: { kind, detail } })
}

// 第一条必须是 hello。
write({ type: "hello", name: "example-process", catalog: catalog() })

// 启动后主动发一条入站事件，方便看出桥真的收到了。
write({
  type: "event",
  record: {
    room: { connection: "example", path: ["group", "1"] },
    kind: { kind: "message" },
    sender: { id: "10001", name: "示例用户", role: null },
    at: AT,
    elements: [{ type: "text", text: "来自外置进程示例的问候" }],
    at_self: false,
    platform_id: "ex-event-1",
    reply_to_self: false,
  },
})

const lines = readline.createInterface({ input: process.stdin })
lines.on("line", (line) => {
  if (!line.trim()) return
  let message
  try {
    message = JSON.parse(line)
  } catch (parseError) {
    console.error(`示例适配器：读不懂 core 消息：${parseError.message}`)
    return
  }

  switch (message.type) {
    case "describe":
      reply(message.id, { outcome: "catalog", catalog: catalog() })
      break
    case "send":
      reply(message.id, {
        outcome: "sent",
        platform_id: `ex-${message.id}`,
        at: AT,
      })
      break
    case "call":
      if (message.method === "example.echo") {
        reply(message.id, { outcome: "call", value: message.params ?? {} })
      } else if (message.method === "example.send") {
        const text = String(message.params?.text ?? "")
        reply(message.id, {
          outcome: "call",
          value: { message_id: `ex-${message.id}` },
          record: {
            kind: { kind: "message" },
            elements: [{ type: "text", text }],
            platform_id: `ex-${message.id}`,
            at: AT,
          },
        })
      } else {
        reply(message.id, {
          outcome: "error",
          error: {
            kind: "unknown_method",
            method: message.method,
            suggestion: "example.echo",
          },
        })
      }
      break
    case "decide":
      error(message.id, "unsupported", "这个示例只做平台适配器，不做策略 provider")
      break
    case "shutdown":
      process.exit(0)
    default:
      error(message.id ?? 0, "unsupported", `不认识的动作：${message.type}`)
  }
})
