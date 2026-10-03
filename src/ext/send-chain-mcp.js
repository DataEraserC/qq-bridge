// src/ext/send-chain-mcp.js — 下游扩展：qq_send_chain 的 MCP 工具声明。
//
// 桥接侧端点在 src/ext/send-chain.js（POST /api/socialV2/send-chain），
// 本文件只负责 stdio server.tool(...) 注册。所有闸门（token、模式、白名单、
// 限频、敏感词、段白名单、合计字数）以桥接为准，MCP 层不做重复校验——
// 与 qq_send_media / qq_send_sticker 的分工一致。
//
// 【下游维护约定】纯下游新增；mcp-snowluma-safe.js 里只保留三处「↓ down」
// 挂钩：顶部 import、TOOL_CONFIG_FLAGS 一行、registerSendChainTool 调用一行。
export function registerSendChainTool({
  server,
  z,
  agentApi,
  serializeModelData,
  cfg,
}) {
  // 就地判断（与 send-media 块同构）；wrapper 的 disabledTools 还会兜底过滤。
  if (cfg.socialV2?.tools?.sendChain === false) return;
  server.tool(
    "qq_send_chain",
    "按顺序发送图文交错的消息链（一条气泡，段序原样保留）。segments 是非空段数组，每段 {type, ...}，支持五种类型：{type:'text',text} 文字；{type:'image',url} 图片；{type:'video',url} 视频（mp4 ≤1 个约 60 秒）；{type:'face',id} QQ 官方表情（数字 id）；{type:'at',qq} 群里 @ 某人（可插在任意位置）。图文交错示例：[{type:'text',text:'看这个'},{type:'image',url:'https://...'},{type:'text',text:'第二张来了'},{type:'image',url:'https://...'}]。上限：≤20 段、图片 ≤9 张、全部 text 合计 ≤ 单条消息字数上限（超长/敏感会被拒绝，长文本仍走 qq_send_message 分条）。url 支持 http(s) URL / 纯 base64 / data: URL / base64://（大文件请传 URL，桥接内安全下载）。需要引用传 replyToMessageId（reply 必须在最前，不支持作为段）、需要头部点名传 atUserId。常规「一图一文」直接用 qq_send_media 更简单；自定义收藏表情包不进链（单独 qq_send_sticker）。",
    {
      key: z.string().describe("会话 key，格式 group:群号 或 private:QQ号"),
      token: z.string().describe("会话令牌（见唤醒提示中的【会话令牌】）"),
      segments: z
        .array(z.any())
        .min(1)
        .describe(
          "段数组（按顺序发出，支持文-图-文-图交错）：{type:'text',text} / {type:'image',url} / {type:'video',url} / {type:'face',id} / {type:'at',qq}；也兼容 OneBot 嵌套写法 {type,data:{...}}",
        ),
      replyToMessageId: z
        .union([z.number(), z.string()])
        .optional()
        .describe(
          "引用的消息 id（可为负数；reply 段必须在最前，故不入 segments）",
        ),
      atUserId: z
        .union([z.number(), z.string()])
        .optional()
        .describe("群聊里头部要 @ 的成员 QQ 号（与链内 at 段独立）"),
    },
    async ({ key, token, segments, replyToMessageId, atUserId }) => {
      try {
        const data = await agentApi("/api/socialV2/send-chain", {
          method: "POST",
          body: JSON.stringify({
            key,
            segments,
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
              text: `qq_send_chain 失败：${error?.message ?? error}`,
            },
          ],
          isError: true,
        };
      }
    },
  );
}
