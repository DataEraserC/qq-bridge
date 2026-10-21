// src/ext/avatar-mcp.js — 下游扩展：qq_get_avatar 的 MCP 工具声明。
//
// 桥接侧端点在 src/ext/avatar.js（GET /api/socialV2/avatar），
// 本文件只负责 stdio server.tool(...) 注册。所有闸门（token、会话准入、工具开关、
// 归属校验）以桥接为准，MCP 层只做开关短路（与 send-media-mcp 同构分工）。
//
// 【下游维护约定】纯下游新增；mcp-snowluma-safe.js 里只保留三处「↓ down」挂钩：
// 顶部 import、TOOL_CONFIG_FLAGS 一行、registerAvatarTool 调用一行。
export function registerAvatarTool({ server, z, agentApi, cfg }) {
  if (cfg.socialV2?.tools?.getAvatar === false) return;
  server.tool(
    "qq_get_avatar",
    "按 QQ 号取头像图片（群成员/好友/自己/本群群头像），图片直接进你的视觉上下文。OneBot 的 get_stranger_info / get_group_member_info **不返回头像**，需要「这个人长什么样」时用本工具。归属受限：群会话里只能取本群成员与本群群头像，私聊里只能取对方或自己。注意 QQ 对没设自定义头像的账号也返回默认头像，看不出对方是否换过头像——别把默认图当真人照片去描述。要头像 URL（而非图片本体）传 urlOnly=true。",
    {
      key: z.string().describe("会话 key，格式 group:群号 或 private:QQ号"),
      token: z.string().describe("会话令牌（见唤醒提示中的【会话令牌】）"),
      target: z
        .union([z.number(), z.string()])
        .describe("要取头像的 QQ 号；kind=group 时传群号"),
      kind: z
        .enum(["user", "group"])
        .optional()
        .describe("默认 user（个人头像）；group 取群头像（只能是当前群）"),
      size: z
        .number()
        .optional()
        .describe("头像尺寸档位：40 / 100 / 140 / 640，默认 640"),
      urlOnly: z
        .boolean()
        .optional()
        .describe("true 时只回 CDN URL 不回图片字节（省上下文，交给别的工具下载）")
    },
    async ({ key, token, target, kind, size, urlOnly }) => {
      try {
        const q = new URLSearchParams({ key, target: String(target) });
        if (kind) q.set("kind", kind);
        if (size) q.set("size", String(size));
        if (urlOnly) q.set("urlOnly", "1");
        const data = await agentApi(`/api/socialV2/avatar?${q.toString()}`, {
          headers: { "x-agent-token": token }
        });
        const summary = {
          ok: data?.ok,
          kind: data?.avatar?.kind,
          id: data?.avatar?.id,
          size: data?.avatar?.size,
          url: data?.avatar?.url,
          width: data?.avatar?.width,
          height: data?.avatar?.height,
          bytes: data?.avatar?.bytes,
          note: data?.avatar?.note
        };
        if (urlOnly || !data?.image?.data) {
          return { content: [{ type: "text", text: JSON.stringify(summary) }] };
        }
        return {
          content: [
            { type: "text", text: JSON.stringify(summary) },
            { type: "image", mimeType: data.image.mimeType, data: data.image.data }
          ]
        };
      } catch (error) {
        return {
          content: [{ type: "text", text: `获取头像失败：${error?.message ?? error}` }],
          isError: true
        };
      }
    }
  );
}
