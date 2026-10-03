// src/ext/send-chain.js — 下游扩展：qq_send_chain（图文交错 / 通用消息链发送）。
//
// 【定位】qq_send_media 解决「一图一文一视频」的常规发送（段序固定
// text→image→video）；本工具解决**段序自由**的场景：文-图-文-图 交错、
// 任意位置插 QQ 官方表情(face)/@人(at)、以及未来更多白名单段类型。
// 两工具共用 send-media.js 的底层（媒体解析、reply/at 头段、网关直连）。
//
// 【下游维护约定】同 send-media.js：本文件是纯下游新增，bridge.js 里只保留
// 标注「↓ down」的挂钩。详见 src/ext/DOWNSTREAM.md。
//
// 【段白名单】text / image / video / face / at（顺序按入参原样发出）。
//   - reply 不作为段类型：用顶层 replyToMessageId（OneBot 要求 reply 排最前）；
//   - json / forward / node / flash 等一律拒绝——这些能构造合并转发或闪照，
//     超出「发普通图文」的授权面，要放开必须单独走一次安全评审；
//   - 自定义收藏表情（表情包）不在链里：仍走 qq_send_sticker（单发一张）。
//
// 【安全边界】与 send-media 同规：
//   - 图片经 deps.safeFetchBuffer 桥接内下载（DNS 固定、逐跳校验、大小限制），
//     绝不把 URL 交给网关重抓；
//   - **text 段合计**（不是单段）按 socialV2.send.maxMessageChars 校验——
//     防「拆成 10 段各 60 字」绕过单条字数上限；敏感词对合计文本同款拒绝；
//   - 段数 / 图片数 / 视频数上限见 CHAIN_LIMITS；
//   - 所有依赖经 deps 注入（含 fetch），本文件不触碰任何全局，可独立单测。
import {
  MEDIA_LIMITS,
  classifyMediaRef,
  resolveMediaBase64,
  buildReplyAtSegments,
  postSegmentsV2,
} from "./send-media.js";

/** 发送上限（集中一处，测试与文档同源引用）。 */
export const CHAIN_LIMITS = {
  maxSegments: 20,
  maxImages: 9, // 与 MEDIA_LIMITS.maxImages 同口径
  maxVideos: 1, // 与 send-media 的 video ≤1 同口径
};

/** 段类型白名单（顺序即 normalize 的校验顺序，也是文档顺序）。 */
export const CHAIN_SEGMENT_TYPES = ["text", "image", "video", "face", "at"];

// 段字段同时接受扁平写法（{type:'text', text}）与 OneBot 嵌套写法
// （{type:'text', data:{text}}），降低模型侧的入形状出错率。
function segField(seg) {
  return seg.data && typeof seg.data === "object" ? seg.data : seg;
}

/**
 * 归一化 + 校验入参（纯函数，可单测）。抛中文 Error，由路由层转 400。
 * 返回 { segments: [{type, ...}], replyToMessageId, atUserId }——
 * segments 保持入参顺序（图文交错的关键），字段收成内部最简形状。
 */
export function normalizeChainPayload(payload = {}, limits = CHAIN_LIMITS) {
  const raw = payload.segments;
  if (!Array.isArray(raw) || raw.length === 0) {
    throw new Error("segments 必须是非空数组；纯文本请用 qq_send_message 发送");
  }
  if (raw.length > limits.maxSegments) {
    throw new Error(
      `segments 最多 ${limits.maxSegments} 段（收到 ${raw.length} 段）`,
    );
  }
  const segments = [];
  let images = 0;
  let videos = 0;
  for (let i = 0; i < raw.length; i++) {
    const seg = raw[i];
    if (!seg || typeof seg !== "object" || Array.isArray(seg)) {
      throw new Error(`segments[${i}] 必须是对象（{type, ...}）`);
    }
    const data = segField(seg);
    const type = String(seg.type ?? data.type ?? "").trim();
    switch (type) {
      case "text": {
        const text = String(data.text ?? "").trim();
        if (!text) throw new Error(`segments[${i}].text 不能为空`);
        segments.push({ type: "text", text });
        break;
      }
      case "image": {
        const ref = String(data.url ?? data.file ?? "").trim();
        classifyMediaRef(ref); // 只做格式校验，取数在 buildChainSegments
        if (++images > limits.maxImages) {
          throw new Error(`image 段最多 ${limits.maxImages} 个`);
        }
        segments.push({ type: "image", ref });
        break;
      }
      case "video": {
        const ref = String(data.url ?? data.file ?? "").trim();
        classifyMediaRef(ref);
        if (++videos > limits.maxVideos) {
          throw new Error(`video 段最多 ${limits.maxVideos} 个`);
        }
        segments.push({ type: "video", ref });
        break;
      }
      case "face": {
        const id = String(data.id ?? "").trim();
        if (!/^\d{1,4}$/.test(id)) {
          throw new Error(`segments[${i}].id 必须是表情数字 id`);
        }
        segments.push({ type: "face", id });
        break;
      }
      case "at": {
        const qq = String(data.qq ?? "").trim();
        if (!/^\d+$/.test(qq)) {
          throw new Error(`segments[${i}].qq 必须是正整数 QQ 号，且不能为 all`);
        }
        segments.push({ type: "at", qq });
        break;
      }
      case "reply":
        throw new Error(
          "reply 不支持作为段类型，请用顶层 replyToMessageId（reply 必须排在消息最前）",
        );
      default:
        throw new Error(
          `segments[${i}].type「${type}」不在白名单（支持 ${CHAIN_SEGMENT_TYPES.join("/")}）`,
        );
    }
  }
  return {
    segments,
    replyToMessageId: payload.replyToMessageId,
    atUserId: payload.atUserId ?? null,
  };
}

/** 汇总全部 text 段（字数/敏感词闸门按这个合计值算，防拆段绕过）。 */
export function chainText(norm) {
  return norm.segments
    .filter((s) => s.type === "text")
    .map((s) => s.text)
    .join("");
}

/**
 * 归一化 payload → OneBot 消息段数组（不含 reply/at 头段）。
 * 顺序与入参完全一致——这就是「图文交错」的实现：段序不重排。
 */
export async function buildChainSegments(deps, norm, limits = MEDIA_LIMITS) {
  const out = [];
  for (const seg of norm.segments) {
    if (seg.type === "text") {
      out.push({ type: "text", data: { text: seg.text } });
    } else if (seg.type === "face") {
      out.push({ type: "face", data: { id: Number(seg.id) } });
    } else if (seg.type === "at") {
      out.push({ type: "at", data: { qq: seg.qq } });
    } else {
      const b64 = await resolveMediaBase64(deps, seg.ref, seg.type, limits);
      out.push({ type: seg.type, data: { file: "base64://" + b64 } });
    }
  }
  return out;
}

/** 本地 recentMessages 摘要标签，如 `[消息链 文x2+图x1+表情x1]`。 */
export function chainSummaryLabel(norm) {
  const count = (type) => norm.segments.filter((s) => s.type === type).length;
  const parts = [];
  const texts = count("text");
  if (texts) parts.push(`文x${texts}`);
  const images = count("image");
  if (images) parts.push(`图x${images}`);
  if (count("video")) parts.push("视频");
  const faces = count("face");
  if (faces) parts.push(`表情x${faces}`);
  const ats = count("at");
  if (ats) parts.push(`@x${ats}`);
  return `[消息链 ${parts.join("+")}]`;
}

/**
 * 发送本体：guard → reply/at 头段 → 按序构建段 → 网关直连。
 * 下载与串行语义全部复用 send-media.js 的共享底层。
 */
export async function sendChainV2(deps, key, norm) {
  const assertSendAllowed = deps.captureSendGuard(key);
  // 顶层 replyToMessageId/atUserId 作头段（与 media 同构，atUserId=紧跟 reply
  // 的头部 @）；链内 at 段在任意位置——二者独立，调用方自己决定用哪种。
  const segments = buildReplyAtSegments(norm.replyToMessageId, norm.atUserId);
  segments.push(...(await buildChainSegments(deps, norm)));
  const data = await postSegmentsV2(deps, key, segments, assertSendAllowed);
  return { messageId: data?.message_id ?? null };
}

/**
 * HTTP 路由：POST /api/socialV2/send-chain。
 * 闸门顺序与 send-media 路由同构（归一化/字数/敏感词先挡廉价滥用 →
 * token → 模式 → key → 白名单 → 静默 → 引用 → 限频额度 → 发送 → 记录），
 * 差异点：字数按 **text 段合计** 校验；私聊拒绝顶层 atUserId 与链内 at 段。
 */
export async function handleSendChainRoute(deps, io) {
  const { req, readBody, sendJson } = io;
  // 共享 readBody 的请求体上限是 1MB（防 DoS）。纯 base64 大图/视频塞进 JSON
  // 必然超限，这里把 413 转成可操作的中文 400（指引改用 URL，桥接内安全下载）。
  let body;
  try {
    body = await readBody();
  } catch (error) {
    if (error?.statusCode === 413) {
      sendJson(
        {
          ok: false,
          error:
            "请求体过大：纯 base64 图片/视频受 1MB 请求体上限限制，大文件请传 http(s) URL（桥接内会安全下载后发送）",
        },
        400,
      );
      return;
    }
    sendJson(
      { ok: false, error: `读取请求体失败：${error?.message ?? error}` },
      error?.statusCode || 500,
    );
    return;
  }
  const key = String(body.key ?? "").trim();
  const replyToMessageId = body.replyToMessageId;
  const atUserId = body.atUserId ?? null;
  const sendCfg = deps.cfg?.socialV2?.send ?? {};
  if (!key || !Array.isArray(body.segments) || body.segments.length === 0) {
    sendJson(
      {
        ok: false,
        error:
          "key 和 segments 不能为空（segments 为非空段数组）；纯文本请用 qq_send_message",
      },
      400,
    );
    return;
  }
  // 归一化先做：纯函数零副作用，坏入参不进后面的闸门链。
  let norm;
  try {
    norm = normalizeChainPayload(body);
  } catch (error) {
    sendJson({ ok: false, error: error?.message ?? String(error) }, 400);
    return;
  }
  // 字数与敏感词闸门与 send-message 同口径，按 text 段**合计**（防拆段绕过），
  // 且在 token 之前挡廉价滥用。
  const maxChars = Math.max(1, Number(sendCfg.maxMessageChars) || 500);
  const allText = chainText(norm);
  if (allText.length > maxChars) {
    sendJson(
      {
        ok: false,
        error: `单条消息不能超过 ${maxChars} 字（text 段合计 ${allText.length} 字）`,
      },
      400,
    );
    return;
  }
  if (deps.isSensitive(allText)) {
    sendJson({ ok: false, error: "消息含敏感信息，已阻止发送" }, 403);
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
  if (req.headers["x-agent-token"] && !deps.v2ToolEnabled("sendChain")) {
    sendJson({ ok: false, error: "工具未启用：qq_send_chain" }, 403);
    return;
  }
  if (deps.currentMode() !== "reserved2") {
    sendJson({ ok: false, error: "该接口仅 reserved2 模式可用" }, 403);
    return;
  }
  if (!req.headers["x-agent-token"]) {
    sendJson(
      { ok: false, error: "reserved2 模式发送必须携带 agent token" },
      403,
    );
    return;
  }
  const keyMatch = /^(group|private):(\d+)$/.exec(key);
  if (!keyMatch) {
    sendJson(
      { ok: false, error: "key 格式应为 group:群号 或 private:QQ号" },
      400,
    );
    return;
  }
  const kind = keyMatch[1];
  const id = Number(keyMatch[2]);
  if (
    !Number.isFinite(id) ||
    id <= 0 ||
    !deps.modeAllowed(key, kind, id, deps.cfg, deps.currentMode())
  ) {
    sendJson({ ok: false, error: "目标不在当前模式允许范围内" }, 403);
    return;
  }
  // 私聊既不许顶层 @，也不许链内任何位置插 at 段。
  const hasAtSegment = norm.segments.some((s) => s.type === "at");
  if (kind === "private" && (atUserId || hasAtSegment)) {
    sendJson({ ok: false, error: "私聊不需要 @" }, 400);
    return;
  }
  if (deps.shouldBlockSilentReply(key)) {
    sendJson({ ok: false, error: "静默模式已开启，当前不允许发送" }, 403);
    return;
  }
  if (
    replyToMessageId !== undefined &&
    replyToMessageId !== null &&
    String(replyToMessageId).trim() !== "" &&
    !/^-?[1-9]\d*$/.test(String(replyToMessageId).trim())
  ) {
    sendJson(
      {
        ok: false,
        error: "replyToMessageId 必须是非零整数（消息 id 可能为负数）",
      },
      400,
    );
    return;
  }
  let quotedInfo = null;
  let actualReplyToMessageId = replyToMessageId;
  if (
    replyToMessageId !== undefined &&
    replyToMessageId !== null &&
    String(replyToMessageId).trim() !== ""
  ) {
    const stForReply = deps.getSocialV2State(key);
    const resolved = await deps.resolveReplyTargetV2(
      stForReply,
      kind,
      id,
      String(replyToMessageId).trim(),
    );
    if (!resolved) {
      sendJson(
        {
          ok: false,
          error:
            "无法解析被引用消息，请确认 message id 正确且属于当前会话（可用 qq_get_message_detail 查看）",
        },
        400,
      );
      return;
    }
    quotedInfo = resolved.info;
    actualReplyToMessageId = resolved.messageId;
  }
  const now = Date.now();
  try {
    const st = deps.getSocialV2State(key);
    const maxPerMinute = Number(sendCfg.maxSendPerMinute) || 0;
    const maxPerHour = Number(sendCfg.maxSendPerHour) || 0;
    const recentMinute = (st.sendTimes || []).filter(
      (t) => now - t < 60000,
    ).length;
    const recentHour = (st.sendTimes || []).filter(
      (t) => now - t < 3600000,
    ).length;
    if (
      (maxPerMinute > 0 && recentMinute + 1 > maxPerMinute) ||
      (maxPerHour > 0 && recentHour + 1 > maxPerHour)
    ) {
      sendJson({ ok: false, error: "发送频率超限，请稍后再试" }, 429);
      return;
    }
    st.sendTimes.push(now);
    if (st.sendTimes.length > 500) st.sendTimes = st.sendTimes.slice(-500);
    // 文本段与文本消息同规：逐段过已知 token 脱敏（sendToQQ 对文本做的那步）。
    for (const seg of norm.segments) {
      if (seg.type === "text") seg.text = deps.redact(seg.text);
    }
    const sent = await sendChainV2(deps, key, {
      ...norm,
      replyToMessageId: actualReplyToMessageId,
    });
    // 记录到二代会话的 recentMessages：媒体元数据不入库，落地后按 messageId
    // 用 qq_get_message_media 读回（与 send-media 同构）。
    const label = chainSummaryLabel(norm);
    const safeJoined = deps.redact(allText);
    const recordText = safeJoined ? `${label} ${safeJoined}` : label;
    st.recentMessages.push({
      messageId: sent.messageId ? String(sent.messageId) : null,
      sender: "我",
      text: deps.truncateText(recordText, 200),
      plain: deps.truncateText(recordText, 200),
      quoteTargetIsSelf: false,
      isOwner: true,
      ownerLabel: "我",
      isSelf: true,
      media: [],
      hasMedia: false,
      forwardIds: [],
      hasForward: false,
      time: Date.now(),
    });
    const recentLimit = Number(deps.cfg?.socialV2?.context?.recentLimit) || 100;
    if (st.recentMessages.length > recentLimit)
      st.recentMessages.splice(0, st.recentMessages.length - recentLimit);
    st.lastAiReplyAt = now;
    st.lastActionAt = now;
    st.wakeConfig.noActionCount = 0;
    st.preSleepWaitSatisfiedAt = 0;
    st.preSleepWaitObservedAt = 0;
    st.preSleepWaitAccumMs = 0;
    deps.saveSocialV2State();
    deps.scheduleReplyCheckV2(key);
    deps.log(`[chain] 工具发送消息链 ${key}: ${label}`);
    deps.appendActivity(`${key} [chain] 工具发送消息链：${label}`);
    sendJson({
      ok: true,
      key,
      messageId: sent.messageId,
      summary: label,
      sent: 1,
      failed: 0,
      quoted: quotedInfo,
    });
  } catch (error) {
    const st = deps.getSocialV2State(key);
    const idx = st.sendTimes.indexOf(now);
    if (idx >= 0) st.sendTimes.splice(idx, 1);
    if (st.sendTimes.length > 500) st.sendTimes = st.sendTimes.slice(-500);
    deps.saveSocialV2State();
    deps.log(`[chain] 工具发送消息链失败 ${key}: ${error?.message ?? error}`);
    sendJson(
      { ok: false, error: `发送消息链失败：${error?.message ?? error}` },
      500,
    );
  }
}
