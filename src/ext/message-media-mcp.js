// src/ext/message-media-mcp.js — 下游扩展：qq_get_message_media 的 MCP 工具声明。
//
// 桥接侧端点在 src/ext/message-media.js（GET /api/socialV2/message-media），
// 本文件只负责 stdio server.tool(...) 注册。所有闸门（token、会话准入、工具开关）
// 以桥接为准，MCP 层只做开关短路（与 sticker 块 / send-media-mcp 同构分工）。
//
// 【关键契约】刻意不用 serializeModelData：那个模型视图会把 media[].url/file 压成
// index 句柄（见 src/qq-model-view.js compactMedia），而本工具存在的全部意义就是把
// 原始 url 交回给模型。media 是顶层键、本来也不会被 messages/newMessages 的压缩碰到，
// 这里用裸 JSON.stringify 把「不压缩」的契约写死，防止将来有人把 media 纳入消息压缩。
//
// 【下游维护约定】纯下游新增；mcp-snowluma-safe.js 里只保留三处「↓ down」挂钩：
// 顶部 import、TOOL_CONFIG_FLAGS 一行、registerMessageMediaTool 调用一行。
export function registerMessageMediaTool({ server, z, agentApi, cfg }) {
  // 就地判断（与 send-media-mcp 同构）；wrapper 的 TOOL_CONFIG_FLAGS 还会兜底过滤。
  if (cfg.socialV2?.tools?.getMessageMedia === false) return;
  server.tool(
    "qq_get_message_media",
    "按 message_id / 本地 seq 获取该条消息的媒体**元数据**（只读，不下载图片本体）：kind、url、file、faceId、caption 等。需要图片 URL 时用它——例如要交给 modlens_read_image(path=URL) 做 OCR/描述、或交给 web_fetch 这类只能吃 URL 的工具；消息列表返回的 media 通常只带 {kind, index} 句柄，原始 url 只有这里能拿到。要看图片**内容本身**请用 qq_get_message_images（图像块直接进上下文）；两者都按 messageId 或本地 seq 查询。",
    {
      key: z
        .string()
        .describe("会话 key，格式 group:群号 或 private:QQ号"),
      token: z
        .string()
        .describe("会话令牌（见唤醒提示中的【会话令牌】）"),
      messageId: z
        .union([z.number(), z.string()])
        .describe(
          "要查看的消息 id（QQ 消息 id 可能为负数；二代也可用本地 seq）",
        ),
    },
    async ({ key, token, messageId }) => {
      try {
        const data = await agentApi(
          `/api/socialV2/message-media?key=${encodeURIComponent(key)}&messageId=${encodeURIComponent(String(messageId))}`,
          { headers: { "x-agent-token": token } },
        );
        return { content: [{ type: "text", text: JSON.stringify(data) }] };
      } catch (error) {
        return {
          content: [
            {
              type: "text",
              text: `获取消息媒体失败：${error?.message ?? error}`,
            },
          ],
          isError: true,
        };
      }
    },
  );
}
