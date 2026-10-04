// src/ext/send-media-mcp.js — 下游扩展：qq_send_media 的 MCP 工具声明。
//
// 桥接侧端点在 src/ext/send-media.js（POST /api/socialV2/send-media），
// 本文件只负责 stdio server.tool(...) 注册。所有闸门（token、模式、白名单、
// 限频、敏感词、媒体校验）以桥接为准，MCP 层不做重复校验——与上游
// qq_send_sticker 的分工一致。
//
// 【下游维护约定】纯下游新增；mcp-snowluma-safe.js 里只保留三处「↓ down」
// 挂钩：顶部 import、TOOL_CONFIG_FLAGS 一行、registerSendMediaTool 调用一行。
export function registerSendMediaTool({
  server,
  z,
  agentApi,
  serializeModelData,
  cfg,
}) {
  // 就地判断（与 sticker 块同构）；wrapper 的 disabledTools 还会兜底过滤。
  if (cfg.socialV2?.tools?.sendMedia === false) return;
  server.tool(
    "qq_send_media",
    "发送图片 / 图文 / 视频消息（一条消息内：可选文字 + 多张图片 + 一个视频）。images 传地址字符串或数组（本地绝对路径 / file:// / http(s) URL / 纯 base64 / data: URL / base64://，最多 9 张），video 传 mp4 地址（最多 1 个，约 60 秒），text 是同气泡文案（按单条消息字数上限校验，长文本请改用 qq_send_message 分条发）。大文件请传本地绝对路径（桥接同机直读，推荐）或 http(s) URL（桥接内安全下载；纯 base64 受 1MB 请求体限制，超长 base64 转手还易损坏）。需要引用传 replyToMessageId、需要点名传 atUserId。表情与媒体不能挤同一条：先发媒体，再单独 qq_send_sticker。",
    {
      key: z.string().describe("会话 key，格式 group:群号 或 private:QQ号"),
      token: z.string().describe("会话令牌（见唤醒提示中的【会话令牌】）"),
      images: z
        .union([z.string(), z.array(z.string())])
        .optional()
        .describe(
          "图片地址（字符串或数组，≤9 张；同机文件用本地绝对路径最稳）",
        ),
      video: z
        .string()
        .optional()
        .describe("视频地址（mp4，≤1 个，约 60 秒；支持本地绝对路径 / URL）"),
      text: z
        .string()
        .optional()
        .describe("同气泡文案（可选，超长/含敏感信息会被拒绝）"),
      replyToMessageId: z
        .union([z.number(), z.string()])
        .optional()
        .describe("引用的消息 id（可为负数）"),
      atUserId: z
        .union([z.number(), z.string()])
        .optional()
        .describe("群聊里要 @ 的成员 QQ 号"),
    },
    async ({ key, token, images, video, text, replyToMessageId, atUserId }) => {
      try {
        const data = await agentApi("/api/socialV2/send-media", {
          method: "POST",
          body: JSON.stringify({
            key,
            images,
            video,
            text,
            replyToMessageId,
            atUserId,
          }),
          headers: { "x-agent-token": token },
          timeoutMs: 300000,
        });
        return { content: [{ type: "text", text: serializeModelData(data) }] };
      } catch (error) {
        return {
          content: [
            {
              type: "text",
              text: `qq_send_media 失败：${error?.message ?? error}`,
            },
          ],
          isError: true,
        };
      }
    },
  );
}
