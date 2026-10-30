// MCP 协议核心：把一条 JSON-RPC 消息换成一条响应，不碰任何 IO。
//
// 从 stdio 版 mcp-server.mjs 抽出来的纯逻辑层——stdio 和 streamable-http 两个
// transport 各自接它，这样「把 MCP 协议说对」的逻辑只写一份。
//
// 支持的方法（acb 只需要这些）：initialize / notifications/initialized /
// tools/list / tools/call / ping。不认识的方法回 -32601。
//
// 这个文件不认识 acb 桥、不认识 dsh、不认识任何聊天平台，也不认识任何 transport
// ——所以能单独测。

const PROTOCOL_VERSION = "2024-11-05"

/**
 * 处理一条 JSON-RPC 请求，返回要回给客户端的响应数组（大多数是一条，通知回空数组）。
 *
 * `tools` 是 [{ name, description, inputSchema }]；`onCall(name, args)` 收到
 * tools/call 时被调（异步），返回结果对象或抛错。
 */
export function dispatch(message, { name = "acb", version = "0.1.0", tools = [], onCall }) {
  // 通知（没有 id）不回复。
  if (message.method === "notifications/initialized") {
    return []
  }
  if (typeof message.id === "undefined") {
    return []
  }

  switch (message.method) {
    case "initialize":
      return [ok(message.id, {
        protocolVersion: PROTOCOL_VERSION,
        capabilities: { tools: {} },
        serverInfo: { name, version },
      })]
    case "ping":
      return [ok(message.id, {})]
    case "tools/list":
      return [ok(message.id, {
        tools: tools.map((t) => ({
          name: t.name,
          description: t.description ?? "",
          inputSchema: t.inputSchema ?? { type: "object", properties: {} },
        })),
      })]
    case "tools/call":
      return [callResult(message.id, message.params, onCall)]
    default:
      return [err(message.id, -32601, `不知道的 MCP 方法 ${message.method}`)]
  }
}

// tools/call 的结果要包一层 MCP 的 CallToolResult 结构，且工具失败用 isError=true
// 让模型看到失败原因（而不是整个 MCP 连接断掉）。这里返回一个「待 onCall 完成后再
// 序列化」的标记，由 transport 层 await 后写回。
function callResult(id, params, onCall) {
  const { name, arguments: args } = params ?? {}
  return {
    _async: true,
    resolve: async () => {
      try {
        const result = await onCall(name, args ?? {})
        return ok(id, {
          content: [
            { type: "text", text: typeof result === "string" ? result : JSON.stringify(result) },
          ],
          isError: false,
        })
      } catch (error) {
        return ok(id, {
          content: [{ type: "text", text: error?.message ?? String(error) }],
          isError: true,
        })
      }
    },
  }
}

function ok(id, result) {
  return { jsonrpc: "2.0", id, result }
}

function err(id, code, message) {
  return { jsonrpc: "2.0", id, error: { code: code ?? -32600, message: message ?? "error" } }
}
