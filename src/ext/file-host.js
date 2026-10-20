// src/ext/file-host.js — 只读文件托管：给本机文件一个 http(s) 能力 URL。
//
// 【职责边界】桥接侧**只读**：GET/HEAD /files/<能力token> 把 state/hosted/<token>/
// 里的单个文件流式送出。把文件放进目录是**文件系统操作**（本机、同 UID），不走
// HTTP——因此没有上传端点：天然绕开 HTTP 2MiB 请求体上限，也不存在需要 CSRF /
// 控制台令牌闸门的写攻击面。
//
// 【为什么不要求控制台令牌】若要求 ?token=<consoleToken>，管理端全权令牌会进 URL
// ——浏览器历史、Referer、聊天记录、shell history 任一处泄露即控制台沦陷。这里用
// **每次托管一段 128-bit 随机能力路径**替代：不可猜、与管理端令牌零耦合，泄露半径
// 只及该文件本身。挂点在 Host 头校验之后（bridge.js 注释标记 ↓ down: file-host），
// DNS rebinding 拿不到同源读取；控制台其余全部路由的令牌门禁不受影响。
//
// 【安全取舍】
//   - token 段严格 [0-9a-f]{32}、整路径精确匹配，decodeURIComponent 从不调用
//     ——没有解码就没有穿越面；文件查找永远落在 hosted/<token>/ 之内；
//   - 用 dirent.isFile()（lstat 语义）选文件，hosted/ 里的符号链接直接跳过
//     （能往该目录写符号链接的进程本就与桥接同 UID，这层是纵深防御）；
//   - 每个响应带 nosniff + `Content-Security-Policy: sandbox`（渲染出的文档进唯一
//     源、脚本全禁）+ Referrer-Policy: no-referrer；html/svg/xml/js 之类**可执行
//     文档**额外强制 Content-Disposition: attachment —— 即便托管了恶意页面，在
//     控制台同源（localStorage 里存着 consoleToken）也只会触发下载、永不执行。
//   - CORS 按能力语义放行（ACAO:* + CORP cross-origin）：知道 URL 即有权取用，
//     便于本机其它 origin（如 DSH GUI）以 <img>/fetch 消费；URL 不可猜 + no-referrer
//     保证不会被未授权方"顺路"拿到。
//
// 【一 token 一文件】hosted/<token>/ 内放**恰好一个**文件（取非点号常规文件的字典
// 序首个，同名替换即换内容，no-store 保证立即生效）。多文件/无文件都按既有规则
// 取首个/404——目录不是 API，形态由投放方自律。
//
// 【投放靠本模块导出，不靠 HTTP】`publishHostedFile()` / `listHostedFiles()` /
// `removeHostedFile()` 是**文件系统操作**（与桥接同 UID），由 `scripts/file-host.mjs`
// 调用；桥接进程不暴露任何上传端点，写攻击面为零。publish 先写 `.part` 再 rename，
// 半截文件不会出现在托管目录里。
//
// 不支持 Range（首版全量 200，浏览器可整段缓冲播放）；无过期时间（token 即门禁，
// 清理 = 删目录）。
import { createReadStream } from "node:fs";
import {
  copyFile, mkdir, readdir, rename, rm, stat, unlink,
} from "node:fs/promises";
import { randomBytes } from "node:crypto";
import path from "node:path";

/** 精确匹配 /files/<32位小写hex>。多余段、大小写混用、编码绕过一律不认。 */
const HOSTED_PATH_RE = /^\/files\/([0-9a-f]{32})$/;

// 零新依赖：内置常用扩展 → Content-Type（不引 mime-types 等包）。
const MIME_BY_EXT = {
  png: "image/png", jpg: "image/jpeg", jpeg: "image/jpeg", gif: "image/gif",
  webp: "image/webp", avif: "image/avif", bmp: "image/bmp", ico: "image/x-icon",
  pdf: "application/pdf",
  txt: "text/plain; charset=utf-8", md: "text/markdown; charset=utf-8",
  json: "application/json; charset=utf-8", csv: "text/csv; charset=utf-8",
  html: "text/html; charset=utf-8", htm: "text/html; charset=utf-8",
  css: "text/css; charset=utf-8", js: "text/javascript; charset=utf-8",
  mjs: "text/javascript; charset=utf-8", xml: "application/xml",
  mp4: "video/mp4", webm: "video/webm", mov: "video/quicktime",
  mp3: "audio/mpeg", wav: "audio/wav", ogg: "audio/ogg",
  m4a: "audio/mp4", flac: "audio/flac",
  zip: "application/zip", gz: "application/gzip", tar: "application/x-tar",
};

// 可内联渲染的类型；其余（含一切可执行文档）强制 attachment 下载。
const INLINE_EXTS = new Set([
  "png", "jpg", "jpeg", "gif", "webp", "avif", "bmp", "ico", "pdf",
  "txt", "md", "json", "csv", "mp4", "webm", "mov", "mp3", "wav", "ogg",
  "m4a", "flac", "zip", "gz", "tar",
]);

/** 路径 → { token } | null。纯函数，可单测。 */
export function parseHostedPath(pathname) {
  const m = HOSTED_PATH_RE.exec(String(pathname ?? ""));
  return m ? { token: m[1] } : null;
}

/** 文件名 → Content-Type。未知扩展回 octet-stream。纯函数。 */
export function contentTypeFor(fileName) {
  const ext = extOf(fileName);
  return MIME_BY_EXT[ext] ?? "application/octet-stream";
}

/**
 * 文件名 → Content-Disposition 值。纯函数。
 * - 消息头注入面收口：CRLF / 引号 / 反斜杠 → `_`，再按 ASCII 折叠出 filename="…"；
 * - 非 ASCII 原名走 RFC 5987 filename*=UTF-8''…（' ( ) * 额外百分号编码）。
 */
export function contentDispositionFor(fileName, inline) {
  const base = String(fileName ?? "").split(/[/\\]/).pop()
    .replace(/[\r\n"\\]/g, "_").slice(0, 120) || "download";
  const ascii = base.replace(/[^\x20-\x7e]/g, "_");
  const encoded = encodeURIComponent(base).replace(
    /['()*]/g,
    (c) => "%" + c.charCodeAt(0).toString(16).toUpperCase(),
  );
  return `${inline ? "inline" : "attachment"}; filename="${ascii}"; filename*=UTF-8''${encoded}`;
}

function extOf(fileName) {
  const base = String(fileName ?? "").split(/[/\\]/).pop();
  const i = base.lastIndexOf(".");
  return i > 0 ? base.slice(i + 1).toLowerCase() : "";
}

/** hosted/<token>/ 内首个非点号常规文件（isFile 为 lstat 语义，跳过符号链接）。 */
async function resolveHostedFile(hostedDir, token) {
  const dir = path.join(hostedDir, token);
  let entries;
  try {
    entries = await readdir(dir, { withFileTypes: true });
  } catch {
    return null;
  }
  const names = entries
    .filter((e) => e.isFile() && !e.name.startsWith("."))
    .map((e) => e.name)
    .sort();
  if (names.length === 0) return null;
  const filePath = path.join(dir, names[0]);
  try {
    const s = await stat(filePath);
    if (!s.isFile()) return null;
    return { filePath, fileName: names[0], size: s.size };
  } catch {
    return null;
  }
}

/** 托管文件响应头（与控制台 SECURITY_HEADERS 同族，外加能力 URL 专属项）。 */
function fileHeaders(contentType, disposition, size) {
  return {
    "content-type": contentType,
    "content-length": String(size),
    "content-disposition": disposition,
    "x-content-type-options": "nosniff",
    "content-security-policy": "sandbox",
    "referrer-policy": "no-referrer",
    "cache-control": "no-store",
    "access-control-allow-origin": "*",
    "cross-origin-resource-policy": "cross-origin",
  };
}

/** 新 token：128-bit 随机小写 hex。 */
export function newHostedToken() {
  return randomBytes(16).toString("hex");
}

/** 文件名 → 安全基名：去目录段、消解分隔符/控制字符、限长。 */
export function safeHostedName(name) {
  const base = String(name ?? "").split(/[/\\]/).pop() ?? "";
  const cleaned = base.replace(/[\r\n\t"\\:*?<>|]/g, "_").replace(/^\.+/, "").trim();
  return (cleaned || "file").slice(0, 120);
}

/**
 * 把本机文件投放为能力 URL（**文件系统操作**，不走 HTTP）。
 *
 * 与只读端点共用同一份目录约定：hosted/<token>/ 内恰好一个文件。先写
 * `.<name>.part` 再 rename —— 半截文件不会出现在托管目录里被读出去。
 * `token` 省略时新生成；重复投放同一 token 即换内容（同名替换语义）。
 *
 * @returns {Promise<{token: string, fileName: string, size: number}>}
 */
export async function publishHostedFile(hostedDir, sourcePath, { token, name } = {}) {
  const from = path.resolve(String(sourcePath ?? ""));
  let info;
  try {
    info = await stat(from);
  } catch (error) {
    throw new Error(
      `源文件不可读（${error?.code === "ENOENT" ? "不存在" : error?.code || "未知错误"}）：${from}`,
    );
  }
  if (!info.isFile()) throw new Error(`源路径不是普通文件：${from}`);
  const fileName = safeHostedName(name ?? path.basename(from));
  const finalToken = token === undefined ? newHostedToken() : String(token);
  if (!/^[0-9a-f]{32}$/.test(finalToken)) {
    throw new Error("token 必须是 32 位小写十六进制（省略则自动生成）");
  }
  const dir = path.join(hostedDir, finalToken);
  await mkdir(dir, { recursive: true, mode: 0o700 });
  const target = path.join(dir, fileName);
  const staging = path.join(dir, `.${fileName}.${randomBytes(4).toString("hex")}.part`);
  try {
    await copyFile(from, staging);
    await rename(staging, target);
  } catch (error) {
    await unlink(staging).catch(() => {});
    throw new Error(`投放失败：${error?.message ?? error}`);
  }
  // 同名替换即换内容；其余旧文件（换名重投）会被"取字典序首个"读到旧的那份，
  // 因此投放成功后清掉除目标之外的所有条目——一 token 一文件是硬约定。
  for (const entry of await readdir(dir).catch(() => [])) {
    if (entry !== fileName) await rm(path.join(dir, entry), { force: true, recursive: true }).catch(() => {});
  }
  return { token: finalToken, fileName, size: info.size };
}

/** 列出已投放的 token（按名字排序），供 CLI/排查用。 */
export async function listHostedFiles(hostedDir) {
  let entries;
  try {
    entries = await readdir(hostedDir, { withFileTypes: true });
  } catch {
    return [];
  }
  const out = [];
  for (const entry of entries) {
    if (!entry.isDirectory() || !/^[0-9a-f]{32}$/.test(entry.name)) continue;
    const hit = await resolveHostedFile(hostedDir, entry.name);
    if (hit) out.push({ token: entry.name, fileName: hit.fileName, size: hit.size });
  }
  return out.sort((a, b) => a.token.localeCompare(b.token));
}

/** 下线一个 token（删目录）；返回是否真的删掉了。 */
export async function removeHostedFile(hostedDir, token) {
  const t = String(token ?? "");
  if (!/^[0-9a-f]{32}$/.test(t)) throw new Error("token 必须是 32 位小写十六进制");
  const dir = path.join(hostedDir, t);
  const exists = await stat(dir).then((s) => s.isDirectory()).catch(() => false);
  if (!exists) return false;
  await rm(dir, { recursive: true, force: true });
  return true;
}

/**
 * 路由处理器：GET/HEAD 送文件，其余方法 405，形态不符 404。
 * io = { req, res }；deps = { hostedDir }（桥接传 state/hosted）。
 * 故意不依赖 sendJson/SECURITY_HEADERS——挂点在控制台令牌门禁**之前**，
 * 自带全套头与 JSON 错误体，夹具里也能脱离开箱单测。
 */
export async function handleHostedFileRoute({ hostedDir }, { req, res, url }) {
  const send = (status, obj) => {
    const body = JSON.stringify(obj);
    res.writeHead(status, {
      "content-type": "application/json; charset=utf-8",
      "cache-control": "no-store",
      "x-content-type-options": "nosniff",
    });
    res.end(req.method === "HEAD" ? undefined : body);
  };

  // 复用控制台主流程已解析的 url（坏 URL 在进入本分支前就已 400）；
  // 仅在夹具直调缺省时兜底解析一次，且包住 try——挂点在主流程 try 之外，裸解析会悬挂。
  let pathname;
  try {
    pathname = (url ?? new URL(req.url ?? "/", "http://127.0.0.1")).pathname;
  } catch {
    return send(400, { ok: false, error: "无效的请求地址" });
  }
  const parsed = parseHostedPath(pathname);
  if (!parsed) return send(404, { ok: false, error: "文件不存在" });
  if (req.method !== "GET" && req.method !== "HEAD") {
    res.setHeader?.("allow", "GET, HEAD");
    return send(405, { ok: false, error: "只支持 GET/HEAD" });
  }

  const hit = await resolveHostedFile(hostedDir, parsed.token);
  if (!hit) return send(404, { ok: false, error: "文件不存在" });

  const ext = extOf(hit.fileName);
  const inline = INLINE_EXTS.has(ext);
  res.writeHead(
    200,
    fileHeaders(contentTypeFor(hit.fileName), contentDispositionFor(hit.fileName, inline), hit.size),
  );
  if (req.method === "HEAD") return res.end();

  await new Promise((resolve) => {
    const stream = createReadStream(hit.filePath);
    stream.on("error", () => { res.destroy(); resolve(); });
    res.on("close", () => stream.destroy());
    stream.on("end", resolve);
    stream.pipe(res);
  });
}
