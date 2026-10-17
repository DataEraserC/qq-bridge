// src/ext/send-file-mcp.js — 下游扩展：qq_send_file 的 MCP 工具声明。
//
// 桥接侧端点在 src/ext/send-file.js（POST /api/socialV2/send-file），
// 本文件只负责 stdio server.tool(...) 注册。所有闸门（token、模式、白名单、
// 限频、敏感词、体积）以桥接为准，MCP 层不做重复校验——与 qq_send_media 同分工。
//
// 【下游维护约定】纯下游新增；mcp-snowluma-safe.js 里只保留三处「↓ down」
// 挂钩：顶部 import、TOOL_CONFIG_FLAGS 一行、registerSendFileTool 调用一行。
export function registerSendFileTool({
  server,
  z,
  agentApi,
  serializeModelData,
  cfg,
}) {
  // 就地判断（与 sticker 块同构）；wrapper 的 disabledTools 还会兜底过滤。
  if (cfg.socialV2?.tools?.sendFile === false) return;
  server.tool(
    "qq_send_file",
    "发送任意类型的文件到群或私聊（QQ 群文件/私聊文件，不是图片消息）。file 传本地绝对路径最稳（桥接同机直读，绕开 1MB 请求体限制），也支持 file:// 与 http(s) URL（桥接内安全下载）；纯 base64 / base64:// 受 1MB 请求体限制，大文件别用。上限 64MiB，发送前不做内容格式校验（任意文件类型都可以发）。name 是文件名（可省略：默认取路径/URL 文件名；纯 base64 时必填）。返回 file_id —— QQ 协议的文件上传没有 message_id，发完不能引用这条文件消息。与 qq_send_media 分工：图片/视频消息用它，其它一切文件用这个。",
    {
      key: z.string().describe("会话 key，格式 group:群号 或 private:QQ号"),
      token: z.string().describe("会话令牌（见唤醒提示中的【会话令牌】）"),
      file: z
        .string()
        .describe(
          "文件地址：本地绝对路径（推荐）/ file:// / http(s) URL / base64:// 或纯 base64（≤1MB 请求体）",
        ),
      name: z
        .string()
        .optional()
        .describe(
          "文件名（默认取路径/URL 文件名；纯 base64 时必填；不含路径分隔符，≤100 字符）",
        ),
    },
    async ({ key, token, file, name }) => {
      try {
        const data = await agentApi("/api/socialV2/send-file", {
          method: "POST",
          body: JSON.stringify({ key, file, name }),
          headers: { "x-agent-token": token },
          timeoutMs: 300000,
        });
        return { content: [{ type: "text", text: serializeModelData(data) }] };
      } catch (error) {
        return {
          content: [
            {
              type: "text",
              text: `qq_send_file 失败：${error?.message ?? error}`,
            },
          ],
          isError: true,
        };
      }
    },
  );
}
