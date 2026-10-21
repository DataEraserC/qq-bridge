// src/ext/avatar.js — 下游扩展：qq_get_avatar（按 QQ 号取头像，群/好友/陌生人通用）。
//
// 【为什么需要它】上游没有任何取头像的能力：
//   · OneBot 的 get_stranger_info / get_group_member_info 返回体里**没有**头像字段
//     （实测网关 1.14.20：get_stranger_info 回 user_id/nickname/sex/age/qq_level/
//     vip 状态等 20 个键，零头像键），所以"问网关要头像"这条路不存在；
//   · 上游 qq_get_message_images 只回消息里**已经存在的**图片段；
//     qq_get_self_image 只回仓库里那张固定的 AI 形象图。
//   → 模型被问"这个人长什么样/头像是什么"时无路可走，只能编。
//
// 【数据来源】QQ 官方头像 CDN（qlogo），按 QQ 号直接取，无需登录态：
//   个人  https://q1.qlogo.cn/g?b=qq&nk=<QQ>&s=<size>
//   群    https://p.qlogo.cn/gh/<群号>/<群号>/<size>
// 实测 640 号段均 200 + image/jpeg。CDN 不回 404 而是回一张默认头像，
// 所以"没有自定义头像"与"有头像"无法区分 —— 这不是缺陷，是 QQ 的行为，
// 文档里如实写明，避免模型把默认头像当成真人照片去描述。
//
// 【安全边界】
//   - 只接受纯数字 QQ 号/群号（1~12 位），URL 由本模块拼装，**绝不**接受调用方
//     传入的任意 URL —— 否则这里就成了一个可被 prompt injection 驱动的 SSRF 跳板；
//   - 下载走 deps.safeFetchBuffer（SSRF 校验、逐跳重定向复验、大小上限、DNS 固定），
//     与 sticker/媒体发送同一条通道；
//   - 像素上限复用接收侧 MAX_MEDIA_PIXELS，避免超大图打爆内存；
//   - 归属校验：群头像/成员头像先确认该 QQ 号或群号确实在**当前会话**的可见范围内
//     （群成员列表），避免模型拿本工具去嗅探任意 QQ 号的头像；
//   - 返回 base64 给 MCP 层转成图像块进上下文（与 qq_get_sticker_image 同构）。
//
// 【下游维护约定】同 send-media.js：纯下游新增，bridge.js / mcp-snowluma-safe.js
// 里只保留标注「↓ down」的挂钩。详见 src/ext/DOWNSTREAM.md。

/** 允许的尺寸档位：CDN 支持 40/100/140/640，其余值会被静默取整到最近档。 */
export const AVATAR_SIZES = Object.freeze([40, 100, 140, 640]);
export const DEFAULT_AVATAR_SIZE = 640;

/**
 * 校验并归一 QQ 号/群号。
 * @returns {string|null} 纯数字串，或 null（非法）
 */
export function normalizeQqId(value) {
  const s = String(value ?? "").trim();
  if (!/^\d{1,12}$/.test(s)) return null;
  if (Number(s) <= 0) return null;
  return s;
}

/**
 * 归一尺寸：只接受 AVATAR_SIZES 里的档位，非法值回落默认。
 * 不做"就近取整"——静默改变调用方要的尺寸会让缓存与断言失真。
 */
export function normalizeAvatarSize(value) {
  const n = Number(value);
  return AVATAR_SIZES.includes(n) ? n : DEFAULT_AVATAR_SIZE;
}

/**
 * 拼装头像 CDN URL。target 决定个人/群两种形态。
 * @param {{ kind: 'user'|'group', id: string, size: number }} spec
 */
export function buildAvatarUrl({ kind, id, size }) {
  const qq = normalizeQqId(id);
  if (!qq) throw new Error("QQ 号/群号必须是纯数字（1~12 位）");
  const s = normalizeAvatarSize(size);
  if (kind === "group") {
    return `https://p.qlogo.cn/gh/${qq}/${qq}/${s}`;
  }
  if (kind !== "user") throw new Error(`未知的头像类型：${String(kind)}`);
  return `https://q1.qlogo.cn/g?b=qq&nk=${qq}&s=${s}`;
}

/**
 * 从会话 key 推出该会话的群号（私聊返回 null）。
 * 头像归属校验要用：群会话里只允许看本群成员与本群群头像。
 */
export function groupIdFromKey(key) {
  const m = /^group:(\d+)$/.exec(String(key ?? ""));
  return m ? m[1] : null;
}

/**
 * GET /api/socialV2/avatar 路由处理器。
 *
 * 依赖经 deps 注入：agentTokenOk / v2SessionAllowed / v2ToolEnabled /
 * safeFetchBuffer / mediaLimits / assertGroupMember（群成员校验）/ selfUserId /
 * maxPixels / getImageDimensions / mimeFromBuffer / log。
 *
 * @param {object} deps extDeps() 注入表
 * @param {{ req, sendJson }} io 当前请求与 JSON 回包函数
 */
export async function handleAvatarRoute(deps, io) {
  const { req, sendJson } = io;
  const url = new URL(req.url ?? "/", "http://127.0.0.1");
  const key = String(url.searchParams.get("key") ?? "").trim();
  const target = String(url.searchParams.get("target") ?? "").trim();
  const kindRaw = String(url.searchParams.get("kind") ?? "user").trim();
  const size = normalizeAvatarSize(url.searchParams.get("size"));

  if (!/^(group|private):(\d+)$/.test(key)) {
    sendJson({ ok: false, error: "key 格式应为 group:群号 或 private:QQ号" }, 400);
    return;
  }
  const targetId = normalizeQqId(target);
  if (!targetId) {
    sendJson({ ok: false, error: "target 必须是纯数字 QQ 号/群号（1~12 位）" }, 400);
    return;
  }
  if (kindRaw !== "user" && kindRaw !== "group") {
    sendJson({ ok: false, error: "kind 只能是 user 或 group" }, 400);
    return;
  }

  if (req.headers["x-agent-token"] && !deps.agentTokenOk(key, req.headers["x-agent-token"])) {
    sendJson({ ok: false, error: "agent token 无效" }, 403);
    return;
  }
  if (req.headers["x-agent-token"] && !deps.v2SessionAllowed(key)) {
    sendJson({ ok: false, error: "目标不在当前模式允许范围内" }, 403);
    return;
  }
  if (req.headers["x-agent-token"] && !deps.v2ToolEnabled("getAvatar")) {
    sendJson({ ok: false, error: "工具未启用：qq_get_avatar" }, 403);
    return;
  }

  // 归属校验：只允许取「本会话看得见」的头像。
  //   · 群会话：群头像必须是本群；成员头像必须是本群成员。
  //   · 私聊：只允许取对方与自己（避免把私聊通道当成任意 QQ 号探测口）。
  const sessionGroup = groupIdFromKey(key);
  const sessionPeer = /^private:(\d+)$/.exec(key)?.[1] ?? null;
  if (kindRaw === "group") {
    if (!sessionGroup) {
      sendJson({ ok: false, error: "只有群会话能取群头像" }, 403);
      return;
    }
    if (targetId !== sessionGroup) {
      sendJson({ ok: false, error: `只能取当前群（${sessionGroup}）的群头像` }, 403);
      return;
    }
  } else if (sessionGroup) {
    const memberError = await deps.assertGroupMember(sessionGroup, targetId);
    if (memberError) {
      sendJson({ ok: false, error: memberError }, 403);
      return;
    }
  } else if (sessionPeer && targetId !== sessionPeer && targetId !== deps.selfUserId?.()) {
    sendJson({ ok: false, error: "私聊里只能取对方或自己的头像" }, 403);
    return;
  }

  let avatarUrl;
  try {
    avatarUrl = buildAvatarUrl({ kind: kindRaw, id: targetId, size });
  } catch (error) {
    sendJson({ ok: false, error: `构造头像地址失败：${error?.message ?? error}` }, 400);
    return;
  }

  // urlOnly：只要 CDN 地址就不要字节。省一次下载与一份 base64（640 号段约 48KB → 64KB base64），
  // 交给 modlens_read_image / web_fetch 这类只吃 URL 的下游工具时尤其划算。
  const urlOnly = url.searchParams.get("urlOnly") === "1";
  if (urlOnly) {
    sendJson({
      ok: true,
      key,
      avatar: {
        kind: kindRaw,
        id: targetId,
        size,
        url: avatarUrl,
        width: null,
        height: null,
        bytes: null,
        note: "QQ 头像 CDN 对无自定义头像的账号同样返回默认头像，无法据此判断对方是否换过头像",
      },
    });
    return;
  }

  try {
    const maxBytes = deps.mediaLimits?.().maxImageBytes ?? 8 * 1024 * 1024;
    const fetched = await deps.safeFetchBuffer(avatarUrl, maxBytes);
    const dims = deps.getImageDimensions(fetched.buffer);
    const maxPixels = deps.maxPixels?.() ?? 0;
    if (dims && maxPixels > 0 && dims.width * dims.height > maxPixels) {
      sendJson({ ok: false, error: `头像像素超限（${dims.width}x${dims.height}），已拒绝` }, 400);
      return;
    }
    const mimeType = deps.mimeFromBuffer(fetched.buffer) || "image/jpeg";
    sendJson({
      ok: true,
      key,
      avatar: {
        kind: kindRaw,
        id: targetId,
        size,
        url: avatarUrl,
        width: dims?.width ?? null,
        height: dims?.height ?? null,
        bytes: fetched.buffer.length,
        // QQ CDN 对不存在的号也回默认头像，无法区分 —— 如实告知，防模型把默认图当真人照片。
        note: "QQ 头像 CDN 对无自定义头像的账号同样返回默认头像，无法据此判断对方是否换过头像",
      },
      image: { mimeType, data: fetched.buffer.toString("base64") },
    });
  } catch (error) {
    deps.log?.(`[avatar] 取头像失败 ${key} ${kindRaw}:${targetId} —— ${error?.message ?? error}`);
    sendJson({ ok: false, error: `获取头像失败：${error?.message ?? error}` }, 500);
  }
}
