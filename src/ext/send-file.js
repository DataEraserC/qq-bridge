// src/ext/send-file.js — 下游扩展：qq_send_file（任意类型文件上传）。
//
// 【下游维护约定】本文件（整个 src/ext/ 目录）是纯下游新增，上游没有这个目录。
// bridge.js 里只保留标注「↓ down」的挂钩：顶部 import、extDeps() 依赖注入
// （含 wsRequest）、一条路由分发行。详见 src/ext/DOWNSTREAM.md。
//
// 【传输】与 send-media 的关键差别：文件走 OneBot `upload_private_file` /
// `upload_group_file`，**经桥接的 WebSocket 主链路**（deps.wsRequest → bot.request）
// 发送，payload 是 base64:// 直传。HTTP 直连有 2MiB 请求体硬上限（实测超限直接断连），
// WS 帧上限 100MiB（实测 64MiB 帧通过、105MiB 被 1009 拒绝），换 WS 才装得下大文件。
// base64 膨胀 4/3：64MiB 文件 ≈ 89MiB 帧，仍在 100MiB 帧上限内。
//
// 【安全边界】
//   - 引用分类复用 send-media 的 classifyMediaRef：本地绝对路径（含 file://，桥接
//     同机直读，不新增权限面）/ http(s) URL（deps.safeFetchBuffer 桥接内下载，DNS 固定、
//     逐跳校验，绝不把 URL 交给网关重抓，防网关侧 SSRF）/ base64。
//   - **不做内容魔数校验**：发的是任意文件（文档/压缩包/安装包…），字节是否可识别与
//     能否上传无关；只校验 base64 字母表与长度（转手损坏就地拒绝）。
//   - 体积上限 FILE_LIMITS.maxFileBytes（64MiB，与 WS 帧上限换算后留有余量）。
//   - 文件名过敏感词闸门（deps.isSensitive）——名字是这条消息里唯一可见的文本；
//     文件内容不扫描（二进制无法按文本规则扫），不承诺内容审查。
//   - 目标白名单走 captureSendGuard(key)（与文本/媒体同一条 allow.groups/allow.private
//     检查），闸门链与 send-media 路由同构。
//   - 所有依赖经 deps 注入（含 fetch/safeFetchBuffer/wsRequest），可脱离桥接单测。

import { readFile, stat } from "node:fs/promises";
import { basename as pathBasename } from "node:path";
import { classifyMediaRef } from "./send-media.js";

/** 发送上限（集中一处，测试与文档同源引用）。 */
export const FILE_LIMITS = {
  maxFileBytes: 64 * 1024 * 1024, // 64MiB：WS 帧 100MiB ÷ 4/3 base64 膨胀 ≈ 74MiB，留余量
  maxNameChars: 100,
  requestTimeoutMs: 300000, // 网关收完帧还要往 QQ 上传，大文件给 5 分钟预算
  uploadAttempts: 2, // F6：上传偶发 TCP connect timeout，瞬时错误重试一次
  retryDelayMs: 1500,
};

const fmtSize = (n) =>
  n >= 1024 * 1024
    ? `${Math.round(n / 1024 / 1024)}MiB`
    : `${Math.round(n / 1024)}KB`;

/**
 * 归一化 + 校验入参（纯函数，可单测）。抛中文 Error，由路由层转 400。
 * 返回 { classified, name, ref } —— name 已按「显式 > 路径 basename > URL 路径段」
 * 推导并过长度/分隔符校验；纯 base64 无从推断，必须显式提供。
 */
export function normalizeFilePayload(payload = {}, limits = FILE_LIMITS) {
  const ref = String(payload.file ?? "").trim();
  if (!ref)
    throw new Error(
      "file 不能为空（本地绝对路径 / file:// / http(s) URL / base64:// / 纯 base64）",
    );
  const classified = classifyMediaRef(ref);
  if (classified.kind === "base64") {
    const approx = (classified.data.length * 3) / 4;
    if (approx > limits.maxFileBytes) {
      throw new Error(
        `文件过大（约 ${fmtSize(approx)}，上限 ${fmtSize(limits.maxFileBytes)}）`,
      );
    }
    // 长 base64 经模型转手抄坏的典型症状：字母表外字符、长度 mod 4 == 1。
    if (!/^[A-Za-z0-9+/]+={0,2}$/.test(classified.data)) {
      throw new Error(
        "base64 含非 base64 字符（长串转手可能已损坏），请改用本地文件路径或 http(s) URL",
      );
    }
    if (classified.data.length % 4 === 1) {
      throw new Error(
        `base64 长度非法（${classified.data.length} 字符，mod 4 = 1，数据已损坏），请改用本地文件路径或 http(s) URL`,
      );
    }
  }
  const name = resolveFileName(payload.name, classified, limits);
  return { classified, name, ref };
}

/** 文件名推导与校验（纯函数）。显式 name 优先；分隔符/控制字符一律拒绝。 */
export function resolveFileName(rawName, classified, limits = FILE_LIMITS) {
  const explicit = String(rawName ?? "").trim();
  let name = explicit;
  if (name) {
    if (/[\\/\u0000-\u001f]/.test(name)) {
      throw new Error("name 不能包含路径分隔符或控制字符");
    }
  } else if (classified.kind === "path") {
    name = pathBasename(classified.path);
  } else if (classified.kind === "url") {
    try {
      const pathname = new URL(classified.url).pathname;
      name = pathBasename(decodeURIComponent(pathname));
    } catch {
      name = "";
    }
    if (!name || name === "/" || name === ".") {
      throw new Error(
        "无法从 URL 推断文件名（路径末端为空），请显式提供 name 参数",
      );
    }
  } else {
    throw new Error("纯 base64 内容没有文件名，请显式提供 name 参数");
  }
  if (!name.trim()) throw new Error("文件名不能为空，请提供 name 参数");
  if (name.length > limits.maxNameChars) {
    throw new Error(
      `name 过长（${name.length} 字符，上限 ${limits.maxNameChars}），请显式传更短的 name`,
    );
  }
  return name;
}

/**
 * 引用 → base64（可单测）。路径同机直读、URL 走 deps.safeFetchBuffer、内联直接复用。
 * 路径与体积的 IO 校验在这里：错误中文可操作，由路由层转 500 并回滚预占额度
 * （与 send-media 的 resolveMediaBase64 同口径：发不出去就不算发送）。
 */
export async function resolveFileBase64(deps, classified, limits = FILE_LIMITS) {
  if (classified.kind === "base64") return classified.data;
  if (classified.kind === "path") {
    const p = classified.path;
    let info;
    try {
      info = await stat(p);
    } catch (error) {
      throw new Error(
        `本地文件不可读（${error?.code === "ENOENT" ? "不存在" : error?.code || "未知错误"}）：${p}`,
      );
    }
    if (!info.isFile()) throw new Error(`本地路径不是普通文件：${p}`);
    if (info.size > limits.maxFileBytes) {
      throw new Error(
        `文件过大（本地文件 ${fmtSize(info.size)}，上限 ${fmtSize(limits.maxFileBytes)}）：${p}`,
      );
    }
    const buf = await readFile(p);
    return buf.toString("base64");
  }
  let fetched;
  try {
    // safeFetchBytes 而非 safeFetchBuffer：发的是任意文件，后者会以"不是有效图片"
    // 拒掉 zip/pdf/exe 等一切非图片。SSRF/重定向/大小上限与前者同源。
    const fetchBytes = deps.safeFetchBytes ?? deps.safeFetchBuffer;
    fetched = await fetchBytes(classified.url, limits.maxFileBytes);
  } catch (error) {
    throw new Error(`文件下载失败，已拒绝发送：${error?.message ?? error}`);
  }
  return Buffer.from(fetched.buffer).toString("base64");
}

/** key → { action, params }（纯函数）：私聊 upload_private_file、群 upload_group_file。 */
export function buildUploadParams(key, b64, name) {
  const m = /^(group|private):(\d+)$/.exec(String(key ?? ""));
  if (!m) throw new Error("key 格式应为 group:群号 或 private:QQ号");
  const file = "base64://" + b64;
  return m[1] === "private"
    ? { action: "upload_private_file", params: { user_id: Number(m[2]), file, name } }
    : { action: "upload_group_file", params: { group_id: Number(m[2]), file, name } };
}

/** 瞬时传输错误才重试（F6 实测：偶发 TCP connect timeout/ETIMEDOUT）；OneBot 业务失败不重试。 */
export function isTransientUploadError(error) {
  const msg = String(error?.message ?? error ?? "");
  return /timeout|timedout|timed out|ECONN|EPIPE|EHOSTUNREACH|ENETUNREACH|ENOTFOUND|EAI_AGAIN|socket|1006|closed|aborted|network/i.test(
    msg,
  );
}

/**
 * 上传本体：sendChain 串行（与文本/表情/媒体同队列）→ 闸门断言 → WS 上传
 * （status/retcode 校验 + 瞬时错误重试一次）。deps 由 extDeps() 注入。
 */
export async function uploadFileV2(deps, key, b64, name) {
  const assertSendAllowed = deps.captureSendGuard(key);
  const { action, params } = buildUploadParams(key, b64, name);
  return deps.enqueueSend(async () => {
    // 与媒体一致：真人发东西前有短暂停顿，且不与文字发送并发。
    await deps.sleep(deps.randInt(800, 2000));
    assertSendAllowed();
    let lastError;
    for (let attempt = 1; attempt <= FILE_LIMITS.uploadAttempts; attempt++) {
      try {
        const resp = await deps.wsRequest(action, params, {
          timeoutMs: FILE_LIMITS.requestTimeoutMs,
        });
        if (!resp || resp.status !== "ok" || resp.retcode !== 0) {
          const why =
            resp?.wording ||
            (resp?.retcode !== undefined ? `retcode ${resp.retcode}` : "无响应");
          throw new Error(`OneBot ${action} 失败: ${why}`);
        }
        return resp.data;
      } catch (error) {
        lastError = error;
        if (
          !isTransientUploadError(error) ||
          attempt >= FILE_LIMITS.uploadAttempts
        ) {
          break;
        }
        deps.log(
          `[file] 上传瞬时失败，${FILE_LIMITS.retryDelayMs}ms 后重试（第 ${attempt} 次）: ${error?.message ?? error}`,
        );
        await deps.sleep(FILE_LIMITS.retryDelayMs);
      }
    }
    throw lastError;
  });
}

/** recentMessages 摘要标签，如 `[文件] report.pdf`。 */
export function fileSummaryLabel(name) {
  return `[文件] ${name}`;
}

/**
 * HTTP 路由：POST /api/socialV2/send-file。
 * 闸门顺序与 send-media 路由同构（归一化/敏感 → token → 模式 → key → 白名单 →
 * 静默 → 限频额度 → 读取/下载 → 上传 → recentMessages 记账）。
 * 没有 reply/at：upload_* 返回 file_id 而非 message_id，QQ 协议无法引用文件消息。
 */
export async function handleSendFileRoute(deps, io) {
  const { req, readBody, sendJson } = io;
  // 共享 readBody 的请求体上限是 1MB（防 DoS）。纯 base64 大文件塞 JSON 必然超限，
  // 这里把 413 转成可操作的中文 400（指引改用本地路径或 URL）。
  let body;
  try {
    body = await readBody();
  } catch (error) {
    if (error?.statusCode === 413) {
      sendJson(
        {
          ok: false,
          error:
            "请求体过大：纯 base64 受 1MB 请求体上限限制，大文件请传本地绝对路径（桥接同机直读）或 http(s) URL（桥接内会安全下载后发送）",
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
  if (!key || body.file === undefined || body.file === null || String(body.file).trim() === "") {
    sendJson({ ok: false, error: "key 和 file 都不能为空" }, 400);
    return;
  }
  let norm;
  try {
    norm = normalizeFilePayload(body);
  } catch (error) {
    sendJson({ ok: false, error: error?.message ?? String(error) }, 400);
    return;
  }
  // 文件名是这条消息唯一可见的文本：与 caption 同规，在 token 之前挡廉价滥用。
  if (deps.isSensitive(norm.name)) {
    sendJson({ ok: false, error: "文件名含敏感信息，已阻止发送" }, 403);
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
  if (req.headers["x-agent-token"] && !deps.v2ToolEnabled("sendFile")) {
    sendJson({ ok: false, error: "工具未启用：qq_send_file" }, 403);
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
  if (deps.shouldBlockSilentReply(key)) {
    sendJson({ ok: false, error: "静默模式已开启，当前不允许发送" }, 403);
    return;
  }
  const sendCfg = deps.cfg?.socialV2?.send ?? {};
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
    const b64 = await resolveFileBase64(deps, norm.classified);
    const data = await uploadFileV2(deps, key, b64, norm.name);
    // 记录到二代会话的 recentMessages，让 AI 知道自己发了什么文件。
    // upload_* 只回 file_id（没有 message_id），引用/读回都不可用，messageId 记 null。
    const label = fileSummaryLabel(norm.name);
    st.recentMessages.push({
      messageId: null,
      sender: "我",
      text: deps.truncateText(label, 200),
      plain: deps.truncateText(label, 200),
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
    deps.log(`[file] 工具发送文件 ${key}: ${norm.name}`);
    deps.appendActivity(`${key} [file] 工具发送文件：${norm.name}`);
    sendJson({
      ok: true,
      key,
      fileId: data?.file_id ?? null,
      name: norm.name,
      sent: 1,
      failed: 0,
    });
  } catch (error) {
    const st = deps.getSocialV2State(key);
    const idx = st.sendTimes.indexOf(now);
    if (idx >= 0) st.sendTimes.splice(idx, 1);
    if (st.sendTimes.length > 500) st.sendTimes = st.sendTimes.slice(-500);
    deps.saveSocialV2State();
    deps.log(`[file] 工具发送文件失败 ${key}: ${error?.message ?? error}`);
    sendJson(
      { ok: false, error: `发送文件失败：${error?.message ?? error}` },
      500,
    );
  }
}
