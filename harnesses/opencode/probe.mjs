// 本地验证用：不装 opencode 也能把「桥 ↔ harness」这一段跑通。
//
//   node probe.mjs --token <设备令牌> [--endpoint 127.0.0.1:7700] [--reply 收到] [--once]
//
// 它用和插件完全同一个协议客户端（`acb-wire.mjs`），只是把「交给 opencode 会话」
// 换成了「打印出来 + 回一句固定的话」。所以能看到这一段的，插件也一定能看到。

import { BridgeLink, renderTurn } from "./plugin/acb-wire.mjs"

const options = parseArgs(process.argv.slice(2))
if (!options.token) {
  console.error("用法：node probe.mjs --token <设备令牌> [--endpoint 127.0.0.1:7700] [--reply 收到] [--once]")
  process.exit(2)
}

const log = (level, message) => console.log(`[${level}] ${message}`)

const link = new BridgeLink({
  endpoint: options.endpoint,
  token: options.token,
  harness: "opencode",
  log,
  async onTurn(delivery) {
    console.log("")
    console.log(renderTurn(delivery))
    console.log("")

    let failure = null
    try {
      if (delivery.records.length > 0 && options.reply) {
        const sent = await link.call("send_message", {
          elements: [{ type: "text", text: options.reply }],
        })
        console.log(`  已发出，平台消息号 ${sent?.platform_id ?? "-"}`)
        const acked = await link.call("ack_messages", {
          ids: delivery.records.map((record) => record.id),
        })
        console.log(`  水位推到 #${acked?.watermark ?? "?"}`)
      }
    } catch (error) {
      failure = error.message
      console.error(`  这一轮出错：${error.message}（${error.code}）`)
    } finally {
      link.endTurn(delivery.ticket, failure)
    }

    if (options.once) {
      link.close()
      process.exit(0)
    }
  },
})

try {
  await link.connect()
} catch (error) {
  console.error(`连不上桥：${error.message}`)
  process.exit(1)
}

console.log(`已绑定到会话组 ${link.sessions.join("、")}，等着开轮（Ctrl-C 退出）`)

process.on("SIGINT", () => {
  link.close()
  process.exit(0)
})

function parseArgs(argv) {
  const options = {
    endpoint: "127.0.0.1:7700",
    token: "",
    reply: "收到",
    once: false,
  }
  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index]
    switch (arg) {
      case "--endpoint":
        options.endpoint = argv[++index] ?? options.endpoint
        break
      case "--token":
        options.token = argv[++index] ?? ""
        break
      case "--reply":
        options.reply = argv[++index] ?? ""
        break
      case "--no-reply":
        options.reply = ""
        break
      case "--once":
        options.once = true
        break
      case "--help":
      case "-h":
        console.log("用法：node probe.mjs --token <设备令牌> [--endpoint 127.0.0.1:7700] [--reply 收到] [--once]")
        process.exit(0)
        break
      default:
        console.error(`不认识的参数：${arg}`)
        process.exit(2)
    }
  }
  return options
}
