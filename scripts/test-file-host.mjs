// file-host 回归（三面）：
//   1. 纯函数 —— 能力路径解析（含大写/长度/尾段/编码变形全拒）、Content-Type 推导、
//      Content-Disposition 注入消解（CRLF/引号）与 RFC 5987 非 ASCII 编码；
//   2. 桥接端点 /files/<token> —— 真实 HTTP 处理器 + fixture：无任何令牌即 200（能力
//      路径本身是门禁）、安全头齐全、Host 校验仍在其前、POST 405、未知 404、
//      **其余路由的控制台令牌门禁一根手指都没松**；
//   3. 文件系统语义 —— 符号链接跳过、隐藏文件跳过、多文件取字典序首个（确定性）。
// 只用 fixture：不读生产配置、不连 QQ/DSH、不发送任何消息。
import assert from "node:assert/strict";
import fs from "node:fs";
import http from "node:http";
import path from "node:path";
import { PassThrough } from "node:stream";
import test from "node:test";
import { bridgeHarness } from "./audit-bridge-harness.mjs";
import {
  parseHostedPath,
  contentTypeFor,
  contentDispositionFor,
} from "../src/ext/file-host.js";

const TOKEN = "0123456789abcdef0123456789abcdef"; // 32 位小写 hex

// ── 1. 纯函数 ───────────────────────────────────────────────────────────────
test("parseHostedPath 只认 /files/<32位小写hex> 精确形态", () => {
  assert.deepEqual(parseHostedPath(`/files/${TOKEN}`), { token: TOKEN });
  for (const bad of [
    "/files/", // 无 token
    "/files", // 裸路径
    `/files/${TOKEN.toUpperCase()}`, // 大写不算
    `/files/${TOKEN.slice(0, 31)}`, // 短一段
    `/files/${TOKEN}0`, // 长一段
    `/files/${TOKEN}/`, // 多余段
    `/files/${TOKEN}/x.png`,
    `/files/${TOKEN}/..%2f..%2fconfig.json`, // 编码穿越（pathname 不解码也不匹配）
    `/files/${TOKEN.slice(0, 16)}%2f..%2f`, // token 段内编码
    "/api/status", // 无关路由
  ]) {
    assert.equal(parseHostedPath(bad), null, `应拒绝: ${bad}`);
  }
});

test("contentTypeFor 按扩展名推导、未知回 octet-stream、大小写不敏感", () => {
  assert.equal(contentTypeFor("a.png"), "image/png");
  assert.equal(contentTypeFor("a.tar.gz"), "application/gzip");
  assert.equal(contentTypeFor("NOTE.HTML"), "text/html; charset=utf-8");
  assert.equal(contentTypeFor("noext"), "application/octet-stream");
  assert.equal(contentTypeFor("weird.xyz123"), "application/octet-stream");
});

test("contentDispositionFor 消解头注入并做 RFC 5987 编码", () => {
  const evil = contentDispositionFor("evil\r\nX-Injected: 1.txt", false);
  assert.ok(!evil.includes("\r") && !evil.includes("\n"), "CRLF 必须被消解");
  assert.ok(evil.startsWith("attachment; "));
  const quoted = contentDispositionFor('a"b\\c.txt', false);
  assert.ok(!quoted.split(";")[1].includes('"') || !quoted.includes('b"c'), "引号/反斜杠被替换成 _");
  const cn = contentDispositionFor("报告 v1.pdf", true);
  assert.ok(cn.startsWith("inline; "));
  assert.ok(cn.includes("filename*=UTF-8''%E6%8A%A5%E5%91%8A%20v1.pdf"), "非 ASCII 走 filename*");
  assert.ok(/filename="[\x20-\x7e]*"/.test(cn), "ASCII 回退段只含可打印字符");
});

// ── 2/3. 桥接端点（真实 HTTP 处理器） ──────────────────────────────────────
function request(server, { method = "GET", path: reqPath, headers = {} } = {}) {
  return new Promise((resolve, reject) => {
    const req = http.request(
      {
        hostname: "127.0.0.1",
        port: server.address().port,
        path: reqPath,
        method,
        headers,
      },
      (res) => {
        const chunks = [];
        res.on("data", (c) => chunks.push(c));
        res.on("end", () =>
          resolve({
            status: res.statusCode,
            headers: res.headers,
            body: Buffer.concat(chunks),
          }),
        );
      },
    );
    req.on("error", reject);
    req.end();
  });
}

async function withHosted(seed, run) {
  const h = await bridgeHarness();
  const server = h.startConsoleServer();
  if (!server.listening) await new Promise((r) => server.once("listening", r));
  try {
    if (seed) seed(h);
    await run(h, server);
  } finally {
    await new Promise((r) => server.close(r));
    await h.close();
  }
}

test("GET 托管文件：无令牌 200、字节一致、安全头齐全", async () => {
  const payload = Buffer.from("hosted payload for file-host e2e\n");
  await withHosted(
    (h) => {
      const dir = path.join(h.temp, "state", "hosted", TOKEN);
      fs.mkdirSync(dir, { recursive: true });
      fs.writeFileSync(path.join(dir, "report.pdf"), payload);
    },
    async (h, server) => {
      const res = await request(server, { path: `/files/${TOKEN}` });
      assert.equal(res.status, 200);
      assert.deepEqual(res.body, payload, "字节逐一致");
      assert.equal(res.headers["content-type"], "application/pdf");
      assert.equal(res.headers["content-length"], String(payload.length));
      assert.ok(res.headers["content-disposition"].startsWith("inline; "), "pdf 可内联");
      assert.equal(res.headers["x-content-type-options"], "nosniff");
      assert.equal(res.headers["content-security-policy"], "sandbox");
      assert.equal(res.headers["referrer-policy"], "no-referrer");
      assert.equal(res.headers["cache-control"], "no-store");
      assert.equal(res.headers["access-control-allow-origin"], "*");
      assert.equal(res.headers["cross-origin-resource-policy"], "cross-origin");
      assert.equal(res.headers["set-cookie"], undefined, "不下发任何 cookie");
    },
  );
});

test("HEAD 同头不带 body；POST 405；未知 token / 裸路径 404", async () => {
  await withHosted(
    (h) => {
      const dir = path.join(h.temp, "state", "hosted", TOKEN);
      fs.mkdirSync(dir, { recursive: true });
      fs.writeFileSync(path.join(dir, "a.txt"), "hello");
    },
    async (h, server) => {
      const head = await request(server, { method: "HEAD", path: `/files/${TOKEN}` });
      assert.equal(head.status, 200);
      assert.equal(head.body.length, 0, "HEAD 无 body");
      assert.equal(head.headers["content-length"], "5");

      const post = await request(server, { method: "POST", path: `/files/${TOKEN}` });
      assert.equal(post.status, 405);
      assert.equal(post.headers.allow, "GET, HEAD");

      const miss = await request(server, { path: "/files/ffffffffffffffffffffffffffffffff" });
      assert.equal(miss.status, 404);
      assert.equal(JSON.parse(miss.body).ok, false);

      const bare = await request(server, { path: "/files" });
      assert.equal(bare.status, 404, "裸 /files 不列目录");
      const slash = await request(server, { path: "/files/" });
      assert.equal(slash.status, 404);
    },
  );
});

test("Host 校验卡在托管分支之前；控制台令牌门禁对其余路由原样", async () => {
  await withHosted(
    (h) => {
      const dir = path.join(h.temp, "state", "hosted", TOKEN);
      fs.mkdirSync(dir, { recursive: true });
      fs.writeFileSync(path.join(dir, "a.txt"), "hi");
    },
    async (h, server) => {
      // evil Host → 即便路径合法也在文件分支之前被 403
      const rebound = await request(server, {
        path: `/files/${TOKEN}`,
        headers: { host: "evil.example:1" },
      });
      assert.equal(rebound.status, 403);

      // 令牌门禁未松动：无 token 401，带 fixture token 200
      const noTok = await request(server, { path: "/api/status" });
      assert.equal(noTok.status, 401);
      const withTok = await request(server, {
        path: "/api/status",
        headers: { "x-console-token": "fixture-console-token" },
      });
      assert.equal(withTok.status, 200);
    },
  );
});

test("符号链接跳过、隐藏文件跳过、多文件取字典序首个", async () => {
  await withHosted(
    (h) => {
      const base = path.join(h.temp, "state", "hosted");
      // 只有符号链接的 token → 404
      const linkTok = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
      fs.mkdirSync(path.join(base, linkTok), { recursive: true });
      fs.symlinkSync(h.temp + "-outside-secret", path.join(base, linkTok, "link.txt"));
      // 隐藏文件的 token → 404
      const dotTok = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
      fs.mkdirSync(path.join(base, dotTok), { recursive: true });
      fs.writeFileSync(path.join(base, dotTok, ".hidden"), "secret");
      // 多文件 → 取 a.txt（字典序首个）
      const multiTok = "cccccccccccccccccccccccccccccccc";
      fs.mkdirSync(path.join(base, multiTok), { recursive: true });
      fs.writeFileSync(path.join(base, multiTok, "b.txt"), "B");
      fs.writeFileSync(path.join(base, multiTok, "a.txt"), "A");
    },
    async (h, server) => {
      const link = await request(server, { path: "/files/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" });
      assert.equal(link.status, 404, "符号链接不外读");
      const dot = await request(server, { path: "/files/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb" });
      assert.equal(dot.status, 404, "隐藏文件不下发");
      const multi = await request(server, { path: "/files/cccccccccccccccccccccccccccccccc" });
      assert.equal(multi.status, 200);
      assert.equal(multi.body.toString(), "A", "确定性取首个");
      assert.ok(multi.headers["content-disposition"].includes('filename="a.txt"'));
    },
  );
});

test("html 强制 attachment（防同源执行）；png 内联", async () => {
  await withHosted(
    (h) => {
      const base = path.join(h.temp, "state", "hosted");
      const htmlTok = "dddddddddddddddddddddddddddddddd";
      fs.mkdirSync(path.join(base, htmlTok), { recursive: true });
      fs.writeFileSync(path.join(base, htmlTok, "page.html"), "<script>alert(1)</script>");
      const pngTok = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";
      fs.mkdirSync(path.join(base, pngTok), { recursive: true });
      fs.writeFileSync(path.join(base, pngTok, "x.png"), Buffer.from([0x89, 0x50, 0x4e, 0x47]));
    },
    async (h, server) => {
      const html = await request(server, { path: "/files/dddddddddddddddddddddddddddddddd" });
      assert.equal(html.status, 200);
      assert.ok(html.headers["content-disposition"].startsWith("attachment; "), "html 必须强制下载");
      assert.equal(html.headers["content-type"], "text/html; charset=utf-8");
      const png = await request(server, { path: "/files/eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee" });
      assert.ok(png.headers["content-disposition"].startsWith("inline; "));
      assert.equal(png.headers["content-type"], "image/png");
    },
  );
});

// ── 挂钩一致性 ──────────────────────────────────────────────────────────────
test("直调缺省 url 时兜底解析：合法走通、坏 URL 回 400 不抛", async () => {
  const { handleHostedFileRoute } = await import("../src/ext/file-host.js");
  const h = await bridgeHarness();
  try {
    const dir = path.join(h.temp, "state", "hosted", TOKEN);
    fs.mkdirSync(dir, { recursive: true });
    fs.writeFileSync(path.join(dir, "a.txt"), "direct");

    // res 要能被 createReadStream().pipe()，用真流挂头部方法
    const mkRes = () => {
      const res = new PassThrough();
      res.statusCode = 0;
      res.headers = {};
      res.writeHead = (status, headers) => { res.statusCode = status; res.headers = headers; };
      res.setHeader = (k, v) => { res.headers[k.toLowerCase()] = v; };
      const chunks = [];
      res.on("data", (c) => chunks.push(c));
      res.ended = new Promise((r) => res.once("end", r));
      Object.defineProperty(res, "body", { get: () => Buffer.concat(chunks) });
      return res;
    };

    // 缺 url → 兜底解析成功，200
    const okRes = mkRes();
    await handleHostedFileRoute(
      { hostedDir: path.join(h.temp, "state", "hosted") },
      { req: { url: `/files/${TOKEN}`, method: "GET" }, res: okRes },
    );
    await okRes.ended;
    assert.equal(okRes.statusCode, 200);
    assert.equal(okRes.body.toString(), "direct");

    // 缺 url 且 req.url 非法 → 400，不向上传播异常（挂点在主流程 try 之外）
    const badRes = mkRes();
    await handleHostedFileRoute(
      { hostedDir: path.join(h.temp, "state", "hosted") },
      { req: { url: "http://[", method: "GET" }, res: badRes },
    );
    await badRes.ended;
    assert.equal(badRes.statusCode, 400);
    assert.equal(JSON.parse(badRes.body).ok, false);
  } finally {
    await h.close();
  }
});

test("bridge.js 挂钩与夹具注入齐全（import 被剥离后靠 context 解析）", async () => {
  const bridgeSrc = fs.readFileSync(
    new URL("../src/bridge.js", import.meta.url),
    "utf8",
  );
  assert.ok(
    bridgeSrc.includes("handleHostedFileRoute"),
    "bridge.js 要有 file-host 分发行",
  );
  assert.ok(
    bridgeSrc.includes("↓ down: file-host"),
    "分发行要带 ↓ down 标记供上游同步盘点",
  );
  const harnessSrc = fs.readFileSync(
    new URL("./audit-bridge-harness.mjs", import.meta.url),
    "utf8",
  );
  assert.ok(harnessSrc.includes("fileHostExt"), "夹具要注入 ext 模块");
});
