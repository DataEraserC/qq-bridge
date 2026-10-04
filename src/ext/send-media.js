// src/ext/send-media.js — 下游扩展：qq_send_media（图片 / 图文 / 视频统一发送）。
//
// 【下游维护约定】本文件（整个 src/ext/ 目录）是纯下游新增，上游没有这个目录。
// bridge.js 里只保留标注「↓ down」的挂钩：顶部 import、extDeps() 依赖注入、
// 一条路由分发行。同步上游时保留这三处即可，本文件的演进不与上游冲突。
// 详见 src/ext/DOWNSTREAM.md。
//
// 【为什么不用 sendToQQ】sendToQQ 只发纯文本（text() 段 + 分条/转义/CQ 转义）；
// 媒体要以原始 OneBot 段（base64:// 直传）走「网关 HTTP 直连 + sendChain 串行」
// 通道，语义与上游 sendStickerV2 完全一致（先图片下载、再入队、短暂停顿、
// 断言可发送、POST、status/retcode 校验）。
//
// 【安全边界】
//   - 外部 URL 一律经 deps.safeFetchBuffer 在桥接内下载（DNS 固定、逐跳校验、
//     大小限制），绝不把 URL 交给网关重新抓取（防网关侧 SSRF，与 sticker 同规）；
//   - 本地绝对路径（含 file://）由桥接直接读文件：桥接与媒体同机，调用方
//     （宿主上的 agent 会话）本就拥有文件读取权，不新增权限面；读取同样过
//     大小闸门，且发送前统一做字节魔数校验（见下一条）；
//   - 所有来源（URL / base64 / 本地文件）发送前统一魔数校验：不是可识别的
//     图片/视频就地拒绝并给出可操作中文提示——损坏内容会让网关在全量上传
//     后返回 opaque 的 error_code=921（实测：长 base64 经模型转手被抄坏、
//     或 URL 下到 HTML 错误页都撞过这个坑）；
//   - 图片 ≤9 张、≤4MB/张；视频 ≤16MB（协议侧 mp4/h64、约 60s）；
//   - text 是同气泡文案：长度按 socialV2.send.maxMessageChars（与文本消息同限），
//     敏感词走 SENSITIVE_RE 同款闸门，超限/敏感直接拒绝，长文本请用 qq_send_message；
//   - 所有依赖经 deps 注入（含 fetch），本文件不触碰任何全局，可独立单测。

import { readFile, stat } from "node:fs/promises";

/** 发送上限（集中一处，测试与文档同源引用）。 */
export const MEDIA_LIMITS = {
  maxImages: 9,
  maxImageBytes: 4 * 1024 * 1024, // 与上游接收侧 MAX_MEDIA_BYTES 一致
  maxVideoBytes: 16 * 1024 * 1024, // mp4/h64 协议上限偏保守（约 60s 短视频）
  requestTimeoutMs: 15000, // 与 sticker 的 AbortSignal.timeout 一致
};

/**
 * 分类一个媒体引用。返回：
 *   { kind: 'base64', data } — base64://、data: URL、纯 base64（启发式）
 *   { kind: 'url', url }     — http(s) URL（调用方必须走 safeFetchBuffer）
 *   { kind: 'path', path }   — file:// URL 或本地绝对路径（同机直接读文件）
 * 格式不被认识时抛中文 Error（路由层转 400）。
 */
export function classifyMediaRef(ref) {
  const s = String(ref ?? "").trim();
  if (!s) throw new Error("媒体地址不能为空");
  if (s.startsWith("base64://"))
    return {
      kind: "base64",
      data: s.slice("base64://".length).replace(/\s/g, ""),
    };
  if (s.startsWith("data:")) {
    const idx = s.indexOf(",");
    if (idx < 0) throw new Error("data: URL 缺少逗号分隔的 base64 载荷");
    return { kind: "base64", data: s.slice(idx + 1).replace(/\s/g, "") };
  }
  if (/^https?:\/\//i.test(s)) return { kind: "url", url: s };
  if (s.startsWith("file://")) {
    let p = s.slice("file://".length);
    if (!p.startsWith("/")) p = `/${p}`; // file://localhost/… 形式也收敛成绝对路径
    try {
      p = decodeURIComponent(p); // file:///a%20b.png → /a/b.png
    } catch {
      throw new Error("file:// 路径含无法解码的百分号编码");
    }
    return { kind: "path", path: p };
  }
  // 本地绝对路径 vs 纯 base64 的消歧（base64 字母表不含「.」，路径几乎都带扩展名）：
  //   - 以 / 开头且含字母表外字符（. _ - 等）→ 一定是路径（/home/…/a.png）；
  //   - 以 / 开头、全在字母表内：≥64 字符按 base64（/9j/4AA… 正是 JPEG 头），
  //     <64 字符按路径（短 base64 本就不接受，短路径 /tmp/x 应被支持）。
  if (s.startsWith("/")) {
    const bare = s.replace(/\s/g, "");
    if (!/^[A-Za-z0-9+/=]+$/.test(bare)) return { kind: "path", path: s };
    if (bare.length < 64) return { kind: "path", path: s };
    return { kind: "base64", data: bare };
  }
  // 纯 base64 启发式：与上游 base64FromMaybe 同口径，要求最小长度避免把
  // 无协议的杂散字符串误判成图片数据。
  if (/^[A-Za-z0-9+/=\s]+$/.test(s) && s.replace(/\s/g, "").length >= 64) {
    return { kind: "base64", data: s.replace(/\s/g, "") };
  }
  throw new Error(
    "媒体地址格式不支持（需 http(s) URL、data: URL、base64://、纯 base64 或本地绝对路径）",
  );
}

/**
 * 归一化 + 校验入参（纯函数，可单测）。抛中文 Error，由路由层转 400。
 * 返回 { images, video, text, replyToMessageId, atUserId }。
 */
export function normalizeMediaPayload(payload = {}, limits = MEDIA_LIMITS) {
  const rawImages = payload.images ?? [];
  const images = (typeof rawImages === "string" ? [rawImages] : rawImages)
    .map((v) => String(v ?? "").trim())
    .filter(Boolean);
  if (images.length > limits.maxImages) {
    throw new Error(
      `images 最多 ${limits.maxImages} 张（收到 ${images.length} 张）`,
    );
  }
  const video = String(payload.video ?? "").trim();
  if (!images.length && !video) {
    throw new Error(
      "请至少提供 images 或 video 之一；纯文本请用 qq_send_message 发送",
    );
  }
  if (video) classifyMediaRef(video); // 只做格式校验，取数在 buildMediaSegments
  for (const ref of images) classifyMediaRef(ref);
  return {
    images,
    video: video || null,
    text: String(payload.text ?? "").trim(),
    replyToMessageId: payload.replyToMessageId,
    atUserId: payload.atUserId ?? null,
  };
}

// ── 发送前字节校验（魔数 + 关键结构）────────────────────────────────────────
// 损坏内容让网关在全量上传后返回 opaque 的 error_code=921，排查成本极高
// （实测三例：长 base64 经模型转手被抄坏/截断、URL 下到的非媒体本体）。
// 这里在桥接内就地拦下，给出「哪里坏了、该怎么发」的可操作中文提示。
const JPEG_EOI = Buffer.from([0xff, 0xd9]); // JPEG EOI 标记
const PNG_IEND = Buffer.from([0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82]);

/** 图片字节是否可识别：JPEG/PNG/GIF/WebP/BMP；JPEG 查尾部 EOI、PNG 查尾部 IEND（拦截断）。 */
function looksLikeImage(buf) {
  if (buf.length < 16) return false;
  if (buf[0] === 0xff && buf[1] === 0xd8 && buf[2] === 0xff) {
    // EOI 必须出现在尾部 16 字节内（EXIF 等元数据里偶发的 FF D9 不在尾部）
    return buf.subarray(-18).indexOf(JPEG_EOI) >= 0;
  }
  const isPng =
    buf[0] === 0x89 &&
    buf[1] === 0x50 &&
    buf[2] === 0x4e &&
    buf[3] === 0x47 &&
    buf[4] === 0x0d &&
    buf[5] === 0x0a &&
    buf[6] === 0x1a &&
    buf[7] === 0x0a;
  if (isPng) return buf.subarray(-12).indexOf(PNG_IEND) >= 0;
  const head = buf.subarray(0, 12).toString("latin1");
  if (head.startsWith("GIF87a") || head.startsWith("GIF89a")) return true;
  if (head.startsWith("RIFF") && head.slice(8) === "WEBP") return true;
  if (head.startsWith("BM")) return true;
  return false;
}

/** 视频字节是否可识别：MP4/MOV（ftyp）、AVI、MKV/WebM、裸 H264（Annex-B 起始码）。 */
function looksLikeVideo(buf) {
  if (buf.length < 12) return false;
  const head = buf.subarray(0, 12).toString("latin1");
  if (head.slice(4, 8) === "ftyp") return true;
  if (head.startsWith("RIFF") && head.slice(8) === "AVI ") return true;
  if (buf[0] === 0x1a && buf[1] === 0x45 && buf[2] === 0xdf && buf[3] === 0xa3)
    return true;
  if (
    (buf[0] === 0 && buf[1] === 0 && buf[2] === 0 && buf[3] === 0x01) ||
    (buf[0] === 0 && buf[1] === 0 && buf[2] === 0x01)
  )
    return true;
  return false;
}

/** 不可识别时抛中文 Error；hint 说明来源（base64 转手 / 下载地址 / 本地路径）。 */
function assertMediaBytes(buf, kind, hint) {
  const ok = kind === "video" ? looksLikeVideo(buf) : looksLikeImage(buf);
  if (ok) return;
  const label = kind === "video" ? "视频" : "图片";
  const formats =
    kind === "video" ? "MP4/MOV/AVI/MKV/WebM/H264" : "JPEG/PNG/GIF/WebP/BMP";
  throw new Error(`${label}内容无法识别（支持 ${formats}），已拒绝发送${hint}`);
}

export async function resolveMediaBase64(
  deps,
  ref,
  kind,
  limits = MEDIA_LIMITS,
) {
  const maxBytes =
    kind === "video" ? limits.maxVideoBytes : limits.maxImageBytes;
  const label = kind === "video" ? "视频" : "图片";
  const classified = classifyMediaRef(ref);
  if (classified.kind === "base64") {
    // 粗略估计 base64 解码后大小，超限直接拒绝，避免超大字符串进内存
    const approx = (classified.data.length * 3) / 4;
    if (approx > maxBytes) {
      throw new Error(
        `${label}过大（约 ${Math.round(approx / 1024)}KB，上限 ${Math.round(maxBytes / 1024)}KB）`,
      );
    }
    const data = classified.data;
    // 字母表/长度校验：长 base64 经模型等转手被抄坏时，常见症状是混入
    // 字母表外字符或长度 %4 == 1（缺失/多余字符），这里直接点破。
    if (!/^[A-Za-z0-9+/]+={0,2}$/.test(data)) {
      throw new Error(
        "base64 含非 base64 字符（长串转手可能已损坏），已拒绝发送；请改用本地文件路径或 http(s) URL",
      );
    }
    if (data.length % 4 === 1) {
      throw new Error(
        `base64 长度非法（${data.length} 字符，长度 mod 4 = 1，数据已被损坏），已拒绝发送；请改用本地文件路径或 http(s) URL`,
      );
    }
    assertMediaBytes(
      Buffer.from(data, "base64"),
      kind,
      "：base64 数据可能已在转手中损坏/截断，请改用本地文件路径或 http(s) URL",
    );
    return data;
  }
  if (classified.kind === "path") {
    // 本地绝对路径：同机直读，不经过请求体/网络（同时绕开 1MB 请求体限制）。
    const p = classified.path;
    let info;
    try {
      info = await stat(p);
    } catch (error) {
      const maybeBase64 =
        p.replace(/\s/g, "").length >= 64 &&
        /^[A-Za-z0-9+/=]+$/.test(p.replace(/\s/g, ""));
      throw new Error(
        `本地媒体文件不可读（${error?.code === "ENOENT" ? "不存在" : error?.code || "未知错误"}）：${p}` +
          (maybeBase64
            ? "；若这其实是 base64 内容，纯 base64 需 ≥64 字符且不带路径特征"
            : ""),
      );
    }
    if (!info.isFile()) throw new Error(`本地媒体路径不是普通文件：${p}`);
    if (info.size > maxBytes) {
      throw new Error(
        `${label}过大（本地文件 ${Math.round(info.size / 1024)}KB，上限 ${Math.round(maxBytes / 1024)}KB）：${p}`,
      );
    }
    const buf = await readFile(p);
    assertMediaBytes(buf, kind, `：${p}`);
    return buf.toString("base64");
  }
  // 桥接内完成带 DNS 固定、逐跳校验和大小限制的下载；不能把 URL 交给网关重抓。
  let fetched;
  try {
    fetched = await deps.safeFetchBuffer(classified.url, maxBytes);
  } catch (error) {
    throw new Error(`媒体地址下载失败，已拒绝发送：${error?.message ?? error}`);
  }
  const buf = Buffer.from(fetched.buffer);
  assertMediaBytes(
    buf,
    kind,
    `：${classified.url}（下载到的可能不是媒体本体，如 HTML 错误页）`,
  );
  return buf.toString("base64");
}

/**
 * 归一化 payload → OneBot 消息段数组（不含 reply/at，由 sendMediaV2 头部追加）。
 * 顺序：text（文案在前）→ images → video。
 */
export async function buildMediaSegments(deps, norm, limits = MEDIA_LIMITS) {
  const segments = [];
  if (norm.text) segments.push({ type: "text", data: { text: norm.text } });
  for (const ref of norm.images) {
    const b64 = await resolveMediaBase64(deps, ref, "image", limits);
    segments.push({ type: "image", data: { file: "base64://" + b64 } });
  }
  if (norm.video) {
    const b64 = await resolveMediaBase64(deps, norm.video, "video", limits);
    segments.push({ type: "video", data: { file: "base64://" + b64 } });
  }
  return segments;
}

/**
 * reply / at 头段（OneBot 要求 reply 排最前）。qq_send_media 与 qq_send_chain
 * 共用，校验口径一致：reply 为非零整数（消息 id 可能为负数），at 为正整数 QQ 号。
 */
export function buildReplyAtSegments(replyToMessageId, atUserId) {
  const segments = [];
  if (
    replyToMessageId !== undefined &&
    replyToMessageId !== null &&
    String(replyToMessageId).trim() !== ""
  ) {
    const rid = String(replyToMessageId).trim();
    if (!/^-?[1-9]\d*$/.test(rid))
      throw new Error("replyToMessageId 必须是非零整数（消息 id 可能为负数）");
    segments.push({ type: "reply", data: { id: rid } });
  }
  if (
    atUserId !== undefined &&
    atUserId !== null &&
    String(atUserId).trim() !== ""
  ) {
    const at = String(atUserId).trim();
    if (!/^\d+$/.test(at))
      throw new Error("atUserId 必须是正整数 QQ 号，且不能为 all");
    segments.push({ type: "at", data: { qq: at } });
  }
  return segments;
}

/**
 * 网关直连发送段数组：构造 action/params、走 sendChain 串行队列、
 * status/retcode 校验——与上游 sendStickerV2 的语义完全一致。
 * assertSendAllowed 由调用方传入（captureSendGuard(key) 的产物），在真正 POST
 * 前断言。deps 由 bridge.js 的 extDeps() 注入（含 fetch，保证在桥接 realm 内
 * 执行——测试夹具通过覆盖 fetch 全局就能观测到真实发出的段）。
 */
export async function postSegmentsV2(deps, key, segments, assertSendAllowed) {
  const [kind, id] = String(key).split(":");
  const action = kind === "private" ? "send_private_msg" : "send_group_msg";
  const params =
    kind === "private"
      ? { user_id: Number(id), message: segments }
      : { group_id: Number(id), message: segments };
  const cfg = deps.cfg;
  const httpUrl = String(
    cfg?.snowluma?.httpUrl || "http://127.0.0.1:3000",
  ).replace(/\/+$/, "");
  // 与文本/表情共用 sendChain，保证「先文字后媒体」的真人顺序不被并发工具调用打乱。
  const data = await deps.enqueueSend(async () => {
    // 真人发图前通常会有短暂停顿，避免「文字刚发完图立刻跟上」的机械感。
    await deps.sleep(deps.randInt(800, 2000));
    assertSendAllowed();
    const res = await deps.fetch(`${httpUrl}/${action}`, {
      method: "POST",
      headers: {
        "content-type": "application/json",
        ...(cfg?.snowluma?.accessToken
          ? { authorization: `Bearer ${cfg.snowluma.accessToken}` }
          : {}),
      },
      body: JSON.stringify(params),
      signal: AbortSignal.timeout(MEDIA_LIMITS.requestTimeoutMs),
    });
    const body = await res.json().catch(() => ({}));
    if (!res.ok || body.status !== "ok" || body.retcode !== 0) {
      const hint =
        res.status === 426
          ? "（HTTP 426：snowluma.httpUrl 可能指向了 WebSocket 端口，请检查 config.json 的 snowluma.httpUrl 是否为 OneBot HTTP API 地址）"
          : "";
      throw new Error(
        `OneBot ${action} 失败: ${body.wording || body.retcode || res.status}${hint}`,
      );
    }
    return body.data;
  });
  return data;
}

/**
 * 发送本体：guard → 段构建（download 含在 buildMediaSegments 内）→ 网关直连。
 */
export async function sendMediaV2(deps, key, payload, options = {}) {
  const norm = normalizeMediaPayload(payload);
  const assertSendAllowed = deps.captureSendGuard(key);
  const replyToMessageId = options.replyToMessageId ?? norm.replyToMessageId;
  const atUserId = options.atUserId ?? norm.atUserId;
  const segments = buildReplyAtSegments(replyToMessageId, atUserId);
  segments.push(...(await buildMediaSegments(deps, norm)));
  const data = await postSegmentsV2(deps, key, segments, assertSendAllowed);
  return { messageId: data?.message_id ?? null, norm };
}

/** 本地 recentMessages 摘要标签，如 `[图片x2]` / `[视频]` / `[图片x1+视频]`。 */
export function mediaSummaryLabel(norm) {
  const parts = [];
  if (norm.images.length) parts.push(`图片x${norm.images.length}`);
  if (norm.video) parts.push("视频");
  return `[${parts.join("+")}]`;
}

/**
 * HTTP 路由：POST /api/socialV2/send-media。
 * 闸门顺序与上游 send-sticker 路由同构（token → 模式 → key → 白名单 →
 * 静默 → 引用 → 归一化 → 限频额度 → 发送 → recentMessages 记录），
 * 敏感词/字数闸门与 send-message 路由同口径。
 */
export async function handleSendMediaRoute(deps, io) {
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
            "请求体过大：纯 base64 图片/视频受 1MB 请求体上限限制，大文件请传本地绝对路径（桥接同机直读）或 http(s) URL（桥接内会安全下载后发送）",
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
  const hasMediaInput =
    (body.images !== undefined && body.images !== null) ||
    (body.video !== undefined && body.video !== null);
  const caption = String(body.text ?? "").trim();
  const replyToMessageId = body.replyToMessageId;
  const atUserId = body.atUserId ?? null;
  const sendCfg = deps.cfg?.socialV2?.send ?? {};
  if (!key || !hasMediaInput) {
    sendJson({ ok: false, error: "key 和 images/video 至少其一不能为空" }, 400);
    return;
  }
  // 文案闸门与 send-message 同口径：字数上限 + 敏感词拒绝（在 token 之前挡廉价滥用）。
  const maxChars = Math.max(1, Number(sendCfg.maxMessageChars) || 500);
  if (caption.length > maxChars) {
    sendJson({ ok: false, error: `单条消息不能超过 ${maxChars} 字` }, 400);
    return;
  }
  if (deps.isSensitive(caption)) {
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
  if (req.headers["x-agent-token"] && !deps.v2ToolEnabled("sendMedia")) {
    sendJson({ ok: false, error: "工具未启用：qq_send_media" }, 403);
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
  if (kind === "private" && atUserId) {
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
  let norm;
  try {
    norm = normalizeMediaPayload(body);
  } catch (error) {
    sendJson({ ok: false, error: error?.message ?? String(error) }, 400);
    return;
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
    // 文案与文本消息同规：再过一次已知 token 脱敏（sendToQQ 对文本做的那步）。
    const safeCaption = norm.text ? deps.redact(norm.text) : "";
    const sent = await sendMediaV2(
      deps,
      key,
      { ...norm, text: safeCaption },
      {
        replyToMessageId: actualReplyToMessageId,
        atUserId,
      },
    );
    // 记录到二代会话的 recentMessages，让 AI 知道自己发了什么媒体。
    // 媒体元数据本身不入库：发送落地后有 messageId，原始 url/file 可用
    // qq_get_message_media 从网关按 id 读回（与 sticker 只存 sticker 字段同构）。
    const label = mediaSummaryLabel(norm);
    const recordText = safeCaption ? `${label} ${safeCaption}` : label;
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
    deps.log(`[media] 工具发送媒体 ${key}: ${label}`);
    deps.appendActivity(`${key} [media] 工具发送媒体：${label}`);
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
    deps.log(`[media] 工具发送媒体失败 ${key}: ${error?.message ?? error}`);
    sendJson(
      { ok: false, error: `发送媒体失败：${error?.message ?? error}` },
      500,
    );
  }
}
