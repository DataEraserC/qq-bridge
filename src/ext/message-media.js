// src/ext/message-media.js — 下游扩展：qq_get_message_media（按需取回媒体元数据）。
//
// 【定位】模型视图（src/qq-model-view.js）会把 messages/newMessages 里的
// media[].file/url 压成 {kind, index} 句柄（拿本地 seq 换图片本体用），于是模型侧
// 再也看不到原始 URL——但 modlens_read_image / web_fetch 这类下游只吃 URL，
// 不声明图像输入的模型更是连图片本体都看不到。本端点就是「按需取回原始媒体元数据」
// 的正门：只回元数据（url/file/faceId/caption…），不下载图片字节
// （那是 /api/images/message / qq_get_message_images 的活）。
//
// 【只读授权面】与发送类工具不同，本域不进 RULES.md 发送白名单（不产生出站消息），
// 但仍过同一套闸门链：key 格式 → agent token → 会话准入（v2SessionAllowed）→
// 工具开关（v2ToolEnabled）。MCP 声明在 message-media-mcp.js。
//
// 【下游维护约定】同 send-media.js：本文件是纯下游新增，bridge.js / mcp-snowluma-safe.js
// 里只保留标注「↓ down」的挂钩。详见 src/ext/DOWNSTREAM.md。
//
// 【数据来源】存储层一直保留原始 file/url（只有模型视图压缩它们），
// findMessageMedia 同时覆盖 recentMessages 与 unread，按 messageId 或本地 seq 都能命中。

/**
 * GET /api/socialV2/message-media 路由处理器。
 * 依赖全部经 deps 注入（闸门 + 存储层），可脱离桥接单测。
 *
 * @param {object} deps extDeps() 注入表：agentTokenOk / v2SessionAllowed /
 *   v2ToolEnabled / findMessageMedia
 * @param {{ req, sendJson }} io 当前请求与 JSON 回包函数
 */
export async function handleMessageMediaRoute(deps, io) {
  const { req, sendJson } = io;
  const url = new URL(req.url ?? "/", "http://127.0.0.1");
  const key = String(url.searchParams.get("key") ?? "").trim();
  const messageId = String(url.searchParams.get("messageId") ?? "").trim();
  if (!/^(group|private):(\d+)$/.test(key)) {
    sendJson({ ok: false, error: "key 格式应为 group:群号 或 private:QQ号" }, 400);
    return;
  }
  if (!messageId) {
    sendJson({ ok: false, error: "messageId 不能为空" }, 400);
    return;
  }
  if (
    req.headers["x-agent-token"] &&
    !deps.agentTokenOk(key, req.headers["x-agent-token"])
  ) {
    sendJson({ ok: false, error: "agent token 无效" }, 403);
    return;
  }
  if (req.headers["x-agent-token"] && !deps.v2SessionAllowed(key)) {
    sendJson({ ok: false, error: "目标不在当前模式允许范围内" }, 403);
    return;
  }
  if (
    req.headers["x-agent-token"] &&
    !deps.v2ToolEnabled("getMessageMedia")
  ) {
    sendJson(
      { ok: false, error: "工具未启用：qq_get_message_media" },
      403,
    );
    return;
  }
  try {
    const media = deps.findMessageMedia(key, messageId);
    sendJson({
      ok: true,
      key,
      messageId,
      media,
      ...(media.length
        ? {}
        : {
            note: "该消息没有可用的媒体元数据（可能已滑出未读/最近窗口，或本就不是图片/表情消息）",
          }),
    });
  } catch (error) {
    sendJson(
      { ok: false, error: `获取消息媒体失败：${error?.message ?? error}` },
      500,
    );
  }
}
