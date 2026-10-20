// qq_send_file 回归（三面）：
//   1. 纯函数单测 —— 引用分类复用（本地绝对路径 / file:// / URL / base64）、
//      文件名推导与校验（显式 / basename / URL 段、分隔符与长度）、base64 损坏
//      与体积拒绝、upload_* 参数构造、瞬时错误重试与业务失败不重试；
//   2. 桥接端点 /api/socialV2/send-file —— 真实 HTTP 处理器 + fixture，
//      用夹具 FakeBot.request（deps.wsRequest 的落点）观测 upload_group_file 的
//      action/params/base64 载荷，守住 flag/token/mode/key/白名单/静默/文件名敏感词/
//      限频闸门、recentMessages 记账与失败回滚；
//   3. 开关一致性（bridge 两份默认表 / toolFlags / toolMap / console.html /
//      config.example.json / TOOL_CONFIG_FLAGS / preset 补丁 / 夹具注入）与
//      MCP 工具注册转发。
// 只用 fixture：不读生产配置、不连 QQ/DSH、不发送任何消息；URL 下载在夹具里
// 用 globals 覆盖 safeFetchBuffer，绝不发起真实网络请求。
import assert from "node:assert/strict";
import fs from "node:fs/promises";
import http from "node:http";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";
import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { StdioClientTransport } from "@modelcontextprotocol/sdk/client/stdio.js";
import { bridgeHarness } from "./audit-bridge-harness.mjs";
import { classifyMediaRef } from "../src/ext/send-media.js";
import {
  FILE_LIMITS,
  normalizeFilePayload,
  resolveFileName,
  resolveFileBase64,
  buildUploadParams,
  uploadFileV2,
  isTransientUploadError,
  fileSummaryLabel,
} from "../src/ext/send-file.js";

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const KEY = "group:456";
// 不做魔数校验：发的是任意文件，夹具字节随便什么形状都行。
const FILE_BYTES = Buffer.from("fixture file bytes for qq_send_file\n");

// ── 1. 纯函数 ───────────────────────────────────────────────────────────────
test("normalizeFilePayload 分类四种合法引用并按来源推导文件名", async () => {
  const fromPath = normalizeFilePayload({ file: "/tmp/dir/report final.pdf" });
  assert.equal(fromPath.classified.kind, "path");
  assert.equal(fromPath.name, "report final.pdf");

  const fromFileUrl = normalizeFilePayload({ file: "file:///tmp/a%20b.txt" });
  assert.equal(fromFileUrl.classified.kind, "path");
  assert.equal(fromFileUrl.name, "a b.txt");

  const fromUrl = normalizeFilePayload({ file: "https://cdn.fixture/pkg.zip" });
  assert.equal(fromUrl.classified.kind, "url");
  assert.equal(fromUrl.name, "pkg.zip");

  const inline = Buffer.from("x".repeat(96)).toString("base64");
  const fromB64 = normalizeFilePayload({ file: `base64://${inline}`, name: "x.bin" });
  assert.equal(fromB64.classified.kind, "base64");
  assert.equal(fromB64.name, "x.bin");

  // 复用 send-media 的分类口径（长纯 base64 与相对路径的消歧由它保证）
  assert.equal(classifyMediaRef("/9j/" + "A".repeat(80)).kind, "base64");
  assert.throws(() => classifyMediaRef("relative/a.png"), /格式不支持/);

  assert.throws(() => normalizeFilePayload({}), /不能为空/);
  assert.throws(() => normalizeFilePayload({ file: "junk! not media" }), /格式不支持/);
});

test("resolveFileName：显式优先、base64 必须显式、非法名拒绝", () => {
  assert.equal(
    resolveFileName("  报告.pdf  ", { kind: "path", path: "/tmp/other.bin" }),
    "报告.pdf",
    "显式 name 覆盖推导",
  );
  assert.throws(
    () => resolveFileName("dir/evil.txt", { kind: "path", path: "/tmp/x" }),
    /路径分隔符/,
  );
  assert.throws(
    () => resolveFileName("a\u0000b", { kind: "path", path: "/tmp/x" }),
    /控制字符/,
  );
  assert.throws(
    () => resolveFileName(undefined, { kind: "base64", data: "AAAA" }),
    /纯 base64/,
  );
  assert.throws(
    () => resolveFileName(undefined, { kind: "url", url: "https://cdn.fixture/" }),
    /无法从 URL 推断文件名/,
  );
  assert.throws(
    () => resolveFileName("n".repeat(FILE_LIMITS.maxNameChars + 1), { kind: "path", path: "/tmp/x" }),
    /过长/,
  );
});

test("体积与 base64 损坏在归一化阶段就地拒绝", () => {
  const tiny = { ...FILE_LIMITS, maxFileBytes: 8 };
  const bigB64 = Buffer.from("x".repeat(64)).toString("base64");
  assert.throws(
    () => normalizeFilePayload({ file: bigB64, name: "x.bin" }, tiny),
    /过大/,
  );
  // 长 base64 转手抄坏的两种典型症状：字母表外字符 / 长度 mod 4 == 1
  assert.throws(
    () => normalizeFilePayload({ file: "data:application/octet-stream,AAAA-BBBB" }),
    /非 base64 字符/,
  );
  assert.throws(
    () => normalizeFilePayload({ file: "base64://" + "A".repeat(65) }),
    /长度非法/,
  );
});

test("buildUploadParams 按 key 选 action 与目标、坏 key 抛错", () => {
  const g = buildUploadParams("group:456", "QUJD", "a.txt");
  assert.equal(g.action, "upload_group_file");
  assert.equal(g.params.group_id, 456);
  assert.equal(g.params.file, "base64://QUJD");
  assert.equal(g.params.name, "a.txt");
  const p = buildUploadParams("private:123", "QUJD", "a.txt");
  assert.equal(p.action, "upload_private_file");
  assert.equal(p.params.user_id, 123);
  assert.throws(() => buildUploadParams("nonsense", "QUJD", "a.txt"), /key 格式/);
});

test("resolveFileBase64：本地直读 / URL 下载 / 内联直通，错误中文可操作", async () => {
  const dir = await fs.mkdtemp(path.join(os.tmpdir(), "qqf-"));
  try {
    const file = path.join(dir, "数据 data.bin");
    await fs.writeFile(file, FILE_BYTES);
    const deps = {
      safeFetchBuffer: async () => {
        throw new Error("本地文件不该走网络");
      },
    };
    const local = await resolveFileBase64(
      deps,
      classifyMediaRef(file),
    );
    assert.equal(local, FILE_BYTES.toString("base64"));

    const inline = Buffer.from("y".repeat(32)).toString("base64");
    assert.equal(
      await resolveFileBase64(deps, classifyMediaRef(`base64://${inline}`)),
      inline,
    );

    const downloads = [];
    const urlB64 = await resolveFileBase64(
      {
        safeFetchBuffer: async (url, maxBytes) => {
          downloads.push({ url, maxBytes });
          return { buffer: FILE_BYTES };
        },
      },
      classifyMediaRef("https://cdn.fixture/pkg.zip"),
    );
    assert.equal(urlB64, FILE_BYTES.toString("base64"));
    assert.equal(downloads[0].maxBytes, FILE_LIMITS.maxFileBytes);

    await assert.rejects(
      resolveFileBase64(deps, classifyMediaRef("/nonexistent-qq-test/x.bin")),
      /不存在/,
    );
    await assert.rejects(resolveFileBase64(deps, classifyMediaRef(dir)), /不是普通文件/);
    await assert.rejects(
      resolveFileBase64(
        { safeFetchBuffer: async () => { throw new Error("DNS 拒绝"); } },
        classifyMediaRef("https://cdn.fixture/x.bin"),
      ),
      /下载失败.*DNS 拒绝/,
    );
  } finally {
    await fs.rm(dir, { recursive: true, force: true });
  }
});

test("uploadFileV2 瞬时传输错误重试一次，OneBot 业务失败不重试", async () => {
  const mkDeps = (wsRequest) => ({
    captureSendGuard: () => () => {},
    enqueueSend: (fn) => fn(),
    sleep: async () => {},
    randInt: () => 0,
    log: () => {},
    wsRequest,
  });
  let attempts = 0;
  const ok = await uploadFileV2(
    mkDeps(async () => {
      attempts += 1;
      if (attempts === 1) throw new Error("connect ETIMEDOUT 127.0.0.1:53001");
      return { status: "ok", retcode: 0, data: { file_id: "fid-1" } };
    }),
    KEY,
    "QUJD",
    "a.txt",
  );
  assert.equal(ok.file_id, "fid-1");
  assert.equal(attempts, 2, "瞬时错误要重试一次");

  attempts = 0;
  await assert.rejects(
    uploadFileV2(
      mkDeps(async () => {
        attempts += 1;
        throw new Error("OneBot upload_group_file 失败: retcode 1404");
      }),
      KEY,
      "QUJD",
      "a.txt",
    ),
    /1404/,
  );
  assert.equal(attempts, 1, "业务失败不该重试");

  assert.ok(isTransientUploadError(new Error("request timed out after 300000ms")));
  assert.ok(!isTransientUploadError(new Error("OneBot upload_group_file 失败: file too large")));
  assert.equal(fileSummaryLabel("report.pdf"), "[文件] report.pdf");
});

// ── 2. 桥接端点 ─────────────────────────────────────────────────────────────
async function harnessRequest(
  config,
  run,
  { mode = "reserved2", globals = {} } = {},
) {
  const h = await bridgeHarness({ config, globals });
  h.setMode(mode);
  const st = h.getSocialV2State(KEY);
  const server = h.startConsoleServer();
  if (!server.listening)
    await new Promise((resolve) => server.once("listening", resolve));
  const post = (payload, { token = st.agentToken, key = KEY } = {}) =>
    new Promise((resolve, reject) => {
      const headers = {
        "x-console-token": "fixture-console-token",
        "content-type": "application/json",
      };
      if (token !== null) headers["x-agent-token"] = token;
      const req = http.request(
        {
          hostname: "127.0.0.1",
          port: server.address().port,
          path: "/api/socialV2/send-file",
          method: "POST",
          headers,
        },
        (res) => {
          let data = "";
          res.setEncoding("utf8");
          res.on("data", (chunk) => {
            data += chunk;
          });
          res.on("end", () => {
            try {
              resolve({ status: res.statusCode, data: JSON.parse(data) });
            } catch (error) {
              reject(error);
            }
          });
        },
      );
      req.on("error", reject);
      req.setTimeout(4000, () =>
        req.destroy(new Error("fixture HTTP request timed out")),
      );
      req.end(JSON.stringify({ key, ...payload }));
    });
  try {
    await run({ h, st, post });
  } finally {
    await new Promise((resolve) => server.close(resolve));
    await h.close();
  }
}

test("send-file 本地文件直读成 base64 经 WS 发 upload_group_file 并记账", async () => {
  const dir = await fs.mkdtemp(path.join(os.tmpdir(), "qqf-e2e-"));
  try {
    const file = path.join(dir, "report final.pdf");
    await fs.writeFile(file, FILE_BYTES);
    await harnessRequest(
      {},
      async ({ h, st, post }) => {
        const res = await post({ file });
        assert.equal(res.status, 200, JSON.stringify(res.data));
        assert.equal(res.data.ok, true);
        assert.equal(res.data.fileId, "fixture-file-1", "网关回的 file_id 要透传");
        assert.equal(res.data.name, "report final.pdf");
        assert.equal(res.data.sent, 1);

        const call = h.calls.ws.at(-1);
        assert.equal(call.action, "upload_group_file");
        assert.equal(call.params.group_id, 456);
        assert.equal(call.params.name, "report final.pdf");
        assert.equal(call.params.file, "base64://" + FILE_BYTES.toString("base64"));
        assert.equal(h.calls.http.length, 0, "文件上传不走 HTTP 通道");

        // recentMessages 记账：无 message_id（协议如此），AI 仍能看到自己发了文件
        const last = st.recentMessages.at(-1);
        assert.equal(last.sender, "我");
        assert.equal(last.isSelf, true);
        assert.equal(last.messageId, null);
        assert.equal(last.text, "[文件] report final.pdf");
        assert.ok(st.lastAiReplyAt > 0, "发送要刷新 lastAiReplyAt");
      },
      {
        globals: {
          safeFetchBuffer: async () => {
            throw new Error("本地文件不该走网络");
          },
        },
      },
    );
  } finally {
    await fs.rm(dir, { recursive: true, force: true });
  }
});

test("send-file URL 在桥接内下载（不交给网关抓取）", async () => {
  const downloads = [];
  await harnessRequest(
    {},
    async ({ h, post }) => {
      const res = await post({ file: "https://cdn.fixture/pkg.zip" });
      assert.equal(res.status, 200, JSON.stringify(res.data));
      assert.equal(res.data.ok, true);
      assert.equal(res.data.name, "pkg.zip", "URL 文件名从路径段推导");
      assert.deepEqual(downloads.map((d) => d.url), ["https://cdn.fixture/pkg.zip"]);
      const call = h.calls.ws.at(-1);
      assert.equal(call.action, "upload_group_file");
      assert.equal(call.params.file, "base64://" + FILE_BYTES.toString("base64"));
    },
    {
      globals: {
        safeFetchBuffer: async (url, maxBytes) => {
          downloads.push({ url, maxBytes });
          return { buffer: FILE_BYTES };
        },
      },
    },
  );
});

test("send-file URL 下载走 safeFetchBytes（不校验图片签名，zip 才发得出去）", async () => {
  const downloads = [];
  const zip = Buffer.from("PK\x03\x04 zip bytes are not an image", "latin1");
  await harnessRequest(
    {},
    async ({ h, post }) => {
      const res = await post({ file: "https://cdn.fixture/pkg.zip" });
      assert.equal(res.status, 200, JSON.stringify(res.data));
      assert.equal(res.data.ok, true);
      assert.deepEqual(downloads.map((d) => d.url), ["https://cdn.fixture/pkg.zip"]);
      const call = h.calls.ws.at(-1);
      assert.equal(call.params.file, "base64://" + zip.toString("base64"));
    },
    {
      // 只桩 safeFetchBytes：证明 ext 走的是**不做图片校验**的那条路。
      // 若哪天退回 safeFetchBuffer，这个用例会因为真实网络解析失败而红。
      globals: {
        safeFetchBytes: async (url, maxBytes) => {
          downloads.push({ url, maxBytes });
          return { buffer: zip };
        },
        safeFetchBuffer: async () => {
          throw new Error("抓取内容不是有效图片（PNG/JPEG/GIF/WebP）");
        },
      },
    },
  );
});

test("send-file 守住 flag/token/mode/key/白名单/文件名/载荷闸门", async () => {
  await harnessRequest(
    { socialV2: { tools: { sendFile: false } } },
    async ({ h, post }) => {
      const res = await post({ file: "/tmp/x.bin" });
      assert.equal(res.status, 403);
      assert.match(String(res.data.error), /工具未启用：qq_send_file/);
      assert.equal(h.calls.ws.length, 0);
      assert.equal(h.calls.http.length, 0);
    },
  );
  await harnessRequest({}, async ({ h, post }) => {
    assert.equal(
      (await post({ file: "/tmp/x.bin" }, { token: "wrong-token" })).status,
      403,
      "错 token",
    );
    const noToken = await post({ file: "/tmp/x.bin" }, { token: null });
    assert.equal(noToken.status, 403);
    assert.match(String(noToken.data.error), /agent token/);
    // token 校验在 key 格式之前（与 send-media 同构）：格式坏 key 报 403
    const badKey = await post({ file: "/tmp/x.bin" }, { key: "nonsense" });
    assert.equal(badKey.status, 403);
    assert.match(String(badKey.data.error), /agent token|允许范围/);
    assert.equal(
      (await post({ file: "/tmp/x.bin" }, { key: "group:999" })).status,
      403,
      "白名单外",
    );
    assert.equal((await post({})).status, 400, "缺 file");
    assert.equal(
      (await post({ file: "junk!" })).status,
      400,
      "非法引用",
    );
    assert.equal(
      (await post({ file: "base64://" + "A".repeat(65) })).status,
      400,
      "损坏 base64",
    );
    assert.equal(
      (await post({ file: "/tmp/x.bin", name: "dir/evil.txt" })).status,
      400,
      "文件名带分隔符",
    );
    const sensitive = await post({
      file: "/tmp/x.bin",
      name: "password: hunter2xyz",
    });
    assert.equal(sensitive.status, 403);
    assert.match(String(sensitive.data.error), /敏感信息/);
    assert.equal(h.calls.ws.length, 0, "任何闸门失败都不该真的发文件");
    assert.equal(h.calls.http.length, 0, "同上：HTTP 通道也不该有动作");
  });
});

test("send-file 非 reserved2 模式直接拒绝", async () => {
  await harnessRequest(
    {},
    async ({ post }) => {
      const res = await post({ file: "/tmp/x.bin" });
      assert.equal(res.status, 403);
      assert.match(String(res.data.error), /reserved2/);
    },
    { mode: "chat" },
  );
});

test("send-file 限频 429：被拒的请求不记账", async () => {
  await harnessRequest(
    { socialV2: { send: { maxSendPerMinute: 1 } } },
    async ({ st, post }) => {
      const file = path.join(os.tmpdir(), "qqf-rate.bin");
      await fs.writeFile(file, FILE_BYTES);
      try {
        assert.equal((await post({ file })).status, 200);
        const second = await post({ file });
        assert.equal(second.status, 429);
        assert.match(String(second.data.error), /频率超限/);
        assert.equal(st.sendTimes.length, 1, "被拒的那次不该记账");
      } finally {
        await fs.rm(file, { force: true });
      }
    },
  );
});

test("send-file 发送失败时 500 并回滚预占额度", async () => {
  await harnessRequest(
    {},
    async ({ h, st, post }) => {
      // 网关 WS 通道失败（夹具注入的 globals 覆盖 wsRequest 不可行——extDeps 闭包持有
      // bot；改用不存在的本地路径在读取阶段失败，同样发生在额度预占之后）。
      const res = await post({ file: "/nonexistent-qq-test/y.bin" });
      assert.equal(res.status, 500);
      assert.match(String(res.data.error), /发送文件失败.*不存在/);
      assert.equal(st.sendTimes.length, 0, "失败后预占额度必须回滚");
      assert.equal(h.calls.ws.length, 0, "读取失败不该打到网关");
    },
  );
});

// ── 3. 开关一致性 + MCP ────────────────────────────────────────────────────
test("sendFile 开关在四份清单 + MCP + preset + 夹具保持同步", async () => {
  const bridgeSrc = await fs.readFile(path.join(ROOT, "src/bridge.js"), "utf8");
  const defaults = bridgeSrc.match(/^\s*sendFile: true,$/gm) || [];
  assert.ok(
    defaults.length >= 2,
    `loadConfig 两份默认表都要有 sendFile（只找到 ${defaults.length} 处）`,
  );
  assert.ok(
    bridgeSrc.includes("'sendFile'"),
    "console config API 的 toolFlags 要登记",
  );
  assert.ok(
    bridgeSrc.includes("sendFile: 'qq_send_file'"),
    "console 名称映射 toolMap 要登记",
  );
  assert.ok(bridgeSrc.includes("'/api/socialV2/send-file'"), "路由分发行存在");
  assert.ok(
    bridgeSrc.includes("from './ext/send-file.js'"),
    "ext import 挂钩存在",
  );
  assert.ok(bridgeSrc.includes("wsRequest: (action, params, options)"), "extDeps 的 wsRequest 挂钩");

  const example = JSON.parse(
    await fs.readFile(path.join(ROOT, "config.example.json"), "utf8"),
  );
  assert.equal(example.socialV2.tools.sendFile, true);

  const consoleHtml = await fs.readFile(
    path.join(ROOT, "public/console.html"),
    "utf8",
  );
  assert.ok(consoleHtml.includes('data-v2-tool="sendFile"'), "控制台开关行");
  assert.ok(
    consoleHtml.includes("<b>qq_send_file</b>"),
    "控制台开关展示工具名",
  );

  const mcpSrc = await fs.readFile(
    path.join(ROOT, "src/mcp-snowluma-safe.js"),
    "utf8",
  );
  assert.ok(
    mcpSrc.includes("qq_send_file: 'sendFile'"),
    "TOOL_CONFIG_FLAGS 登记（漏登记=默认关却仍可见）",
  );
  assert.ok(
    mcpSrc.includes(
      "registerSendFileTool({ server, z, agentApi, serializeModelData, cfg })",
    ),
    "注册调用挂钩",
  );
  assert.ok(
    mcpSrc.includes("from './ext/send-file-mcp.js'"),
    "MCP ext import 挂钩",
  );

  const preset = await fs.readFile(
    path.join(ROOT, "dsh/agent-presets/qq-chat-v2/agent.cordis.yml"),
    "utf8",
  );
  assert.ok(
    preset.includes("【发文件（任意类型）】qq_send_file"),
    "preset 源能力行",
  );
  const patch = await fs.readFile(
    path.join(ROOT, "plugins/qq-agent-presets/presets/qq-chat-v2.patch.yml"),
    "utf8",
  );
  assert.ok(
    patch.includes("【发文件（任意类型）】qq_send_file"),
    "生成的补丁要同步（跑 build-agent-preset-patches）",
  );

  const harnessSrc = await fs.readFile(
    path.join(ROOT, "scripts/audit-bridge-harness.mjs"),
    "utf8",
  );
  assert.ok(harnessSrc.includes("sendFileExt"), "夹具要注入 ext 模块");

  const rules = await fs.readFile(path.join(ROOT, "RULES.md"), "utf8");
  assert.ok(
    rules.includes("qq_send_chain` / `qq_send_file`"),
    "RULES.md 发送白名单要登记 qq_send_file",
  );

  await fs.access(path.join(ROOT, "src/ext/DOWNSTREAM.md"));
  const auditSrc = await fs.readFile(
    path.join(ROOT, "scripts/test-audit.mjs"),
    "utf8",
  );
  assert.ok(
    auditSrc.includes("'test-send-file.mjs'"),
    "本测试要登记进 test-audit 白名单",
  );
});

async function mcpFixture(config, run) {
  const requests = [];
  const api = http.createServer((req, res) => {
    const chunks = [];
    req.on("data", (chunk) => chunks.push(chunk));
    req.on("end", () => {
      requests.push({
        url: req.url,
        token: req.headers["x-agent-token"],
        body: Buffer.concat(chunks).toString("utf8"),
      });
      res.writeHead(200, { "content-type": "application/json" });
      res.end(
        JSON.stringify({
          ok: true,
          key: KEY,
          fileId: "fixture-file-1",
          name: "x.pdf",
          sent: 1,
          failed: 0,
        }),
      );
    });
  });
  await new Promise((resolve, reject) => {
    api.once("error", reject);
    api.listen(0, "127.0.0.1", resolve);
  });
  let fixture;
  let client;
  try {
    // 与 test-send-media 相同的隔离：只复制源码与最小 config；ext 目录一并复制
    // （mcp-snowluma-safe.js 顶部 import ./ext/send-file-mcp.js）。
    fixture = await fs.mkdtemp(path.join(ROOT, ".tmp-send-file-test-"));
    await fs.mkdir(path.join(fixture, "src"));
    await Promise.all(
      ["mcp-snowluma-safe.js", "qq-model-view.js", "sensitive.js"].map((name) =>
        fs.copyFile(
          path.join(ROOT, "src", name),
          path.join(fixture, "src", name),
        ),
      ),
    );
    await fs.cp(
      path.join(ROOT, "src", "ext"),
      path.join(fixture, "src", "ext"),
      { recursive: true },
    );
    await fs.writeFile(
      path.join(fixture, "config.json"),
      JSON.stringify({ consolePort: api.address().port, ...config }),
    );
    const transport = new StdioClientTransport({
      command: process.execPath,
      args: [path.join(fixture, "src", "mcp-snowluma-safe.js")],
      cwd: fixture,
      stderr: "pipe",
    });
    client = new Client({ name: "send-file-test", version: "1.0.0" });
    await client.connect(transport);
    await run({ client, requests });
  } finally {
    if (client) await client.close();
    api.closeAllConnections();
    await new Promise((resolve) => api.close(resolve));
    if (fixture) {
      const resolved = path.resolve(fixture);
      assert.equal(path.dirname(resolved), ROOT);
      assert.ok(path.basename(resolved).startsWith(".tmp-send-file-test-"));
      await fs.rm(resolved, { recursive: true, force: true });
    }
  }
}

test("MCP qq_send_file 默认注册并把载荷/token 转发给桥接", async () => {
  await mcpFixture({}, async ({ client, requests }) => {
    const listed = await client.listTools();
    const tool = listed.tools.find((t) => t.name === "qq_send_file");
    assert.ok(tool, "qq_send_file must be registered by default");
    assert.ok(tool.description.includes("文件"), "描述要说清这是发文件");
    assert.ok(tool.description.includes("64MiB"), "描述要写明体积上限");
    assert.ok(tool.description.includes("file_id"), "描述要说明返回 file_id 而非 message_id");

    const call = await client.callTool({
      name: "qq_send_file",
      arguments: {
        key: KEY,
        token: "fixture-token",
        file: "/tmp/x.pdf",
        name: "x.pdf",
      },
    });
    assert.equal(call.isError, undefined, JSON.stringify(call));
    const data = JSON.parse(
      call.content.find((part) => part.type === "text").text,
    );
    assert.equal(data.ok, true);
    assert.equal(data.fileId, "fixture-file-1");

    const forwarded = requests.at(-1);
    const url = new URL(forwarded.url, "http://fixture");
    assert.equal(url.pathname, "/api/socialV2/send-file");
    assert.equal(
      forwarded.token,
      "fixture-token",
      "session token 必须走 x-agent-token 头",
    );
    const payload = JSON.parse(forwarded.body);
    assert.equal(payload.key, KEY);
    assert.equal(payload.file, "/tmp/x.pdf");
    assert.equal(payload.name, "x.pdf");
  });
});

test("MCP qq_send_file 关闭时不注册且不影响其他工具", async () => {
  await mcpFixture(
    { socialV2: { tools: { sendFile: false } } },
    async ({ client }) => {
      const listed = await client.listTools();
      assert.equal(
        listed.tools.some((t) => t.name === "qq_send_file"),
        false,
      );
      assert.ok(
        listed.tools.some((t) => t.name === "qq_send_message"),
        "关闭一个不该连带关掉别人",
      );
    },
  );
});
