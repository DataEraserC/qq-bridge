// qq_send_media 回归（三面）：
//   1. 纯函数单测 —— 引用分类、归一化上限、段构建顺序（text→image→video）、
//      内联 base64 不走网络、超限/下载失败的中文错误；
//   2. 桥接端点 /api/socialV2/send-media —— 真实 HTTP 处理器 + fixture，
//      用 fakeFetch 观测**实际发给网关的 OneBot 段**（reply→at→text→image→…），
//      守住 flag/token/mode/key/白名单/静默/引用/敏感词/字数/限频闸门与失败回滚；
//   3. 开关一致性（bridge 两份默认表 / toolFlags / toolMap / console.html /
//      config.example.json / TOOL_CONFIG_FLAGS / preset 补丁 / 夹具注入）与
//      MCP 工具注册转发。
// 只用 fixture：不读生产配置、不连 QQ/DSH、不发送任何消息；URL 下载在夹具里
// 用 globals 覆盖 safeFetchBuffer，绝不发起真实网络请求。
import assert from "node:assert/strict";
import fs from "node:fs/promises";
import http from "node:http";
import path from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";
import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { StdioClientTransport } from "@modelcontextprotocol/sdk/client/stdio.js";
import { bridgeHarness } from "./audit-bridge-harness.mjs";
import {
  MEDIA_LIMITS,
  classifyMediaRef,
  normalizeMediaPayload,
  buildMediaSegments,
  mediaSummaryLabel,
} from "../src/ext/send-media.js";

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const KEY = "group:456";
const INLINE = Buffer.from("meme".repeat(30)).toString("base64"); // ≥64 字符，命中纯 base64 启发式

// ── 1. 纯函数 ───────────────────────────────────────────────────────────────
test("classifyMediaRef 分类四种合法引用并拒绝杂散字符串", () => {
  assert.deepEqual(classifyMediaRef("base64://YWJj"), {
    kind: "base64",
    data: "YWJj",
  });
  assert.deepEqual(classifyMediaRef("data:image/png;base64,AAA="), {
    kind: "base64",
    data: "AAA=",
  });
  assert.deepEqual(classifyMediaRef("https://cdn.example/a.png"), {
    kind: "url",
    url: "https://cdn.example/a.png",
  });
  assert.equal(classifyMediaRef(INLINE).kind, "base64");
  assert.throws(() => classifyMediaRef("junk! not media"), /格式不支持/);
  assert.throws(() => classifyMediaRef(""), /不能为空/);
});

test("normalizeMediaPayload 归一化入参并强制上限", () => {
  // 单字符串 images 也能用
  assert.deepEqual(
    normalizeMediaPayload({ images: "https://a/1.png" }).images,
    ["https://a/1.png"],
  );
  // 没有媒体 → 指路 qq_send_message
  assert.throws(
    () => normalizeMediaPayload({ text: "纯文字" }),
    /至少提供 images 或 video/,
  );
  // 超过 9 张
  assert.throws(
    () => normalizeMediaPayload({ images: Array(10).fill("https://a/x.png") }),
    /最多 9 张/,
  );
  // 非法地址立即暴露
  assert.throws(() => normalizeMediaPayload({ images: "junk!" }), /格式不支持/);
  const norm = normalizeMediaPayload({
    images: ["https://a/1.png"],
    video: "https://a/v.mp4",
    text: "  配文  ",
    replyToMessageId: "-9",
    atUserId: "10001",
  });
  assert.deepEqual(norm.images, ["https://a/1.png"]);
  assert.equal(norm.video, "https://a/v.mp4");
  assert.equal(norm.text, "配文");
  assert.equal(norm.replyToMessageId, "-9");
  assert.equal(norm.atUserId, "10001");
});

test("buildMediaSegments 按 text→image→video 排段，内联 base64 不走网络", async () => {
  const downloads = [];
  const deps = {
    safeFetchBuffer: async (url, maxBytes) => {
      downloads.push({ url, maxBytes });
      return { buffer: Buffer.from("PNG") };
    },
  };
  const norm = normalizeMediaPayload({
    text: "配文",
    images: [
      "https://cdn/img.png",
      Buffer.from("inline".repeat(30)).toString("base64"),
    ],
    video: "https://cdn/v.mp4",
  });
  const segments = await buildMediaSegments(deps, norm);
  assert.deepEqual(
    segments.map((s) => s.type),
    ["text", "image", "image", "video"],
  );
  assert.equal(
    segments[1].data.file,
    "base64://UE5H",
    "URL 下载后必须转 base64://",
  );
  assert.equal(
    segments[2].data.file,
    "base64://" + Buffer.from("inline".repeat(30)).toString("base64"),
  );
  // 只有 URL 两个走下载，且上限分别来自图片/视频常量
  assert.equal(downloads.length, 2);
  assert.deepEqual(
    downloads.map((d) => d.maxBytes),
    [MEDIA_LIMITS.maxImageBytes, MEDIA_LIMITS.maxVideoBytes],
  );
});

test("buildMediaSegments 拒绝超限内联载荷与下载失败", async () => {
  const limits = { ...MEDIA_LIMITS, maxImageBytes: 8 };
  const big = Buffer.from("x".repeat(64)).toString("base64"); // 解码 64B > 8B，base64 长度 ≥64 不会先撞分类
  const norm = normalizeMediaPayload({ images: big });
  await assert.rejects(
    buildMediaSegments(
      {
        safeFetchBuffer: async () => {
          throw new Error("不应下载");
        },
      },
      norm,
      limits,
    ),
    /过大/,
  );
  const urlNorm = normalizeMediaPayload({ images: "https://cdn/x.png" });
  await assert.rejects(
    buildMediaSegments(
      {
        safeFetchBuffer: async () => {
          throw new Error("DNS 拒绝");
        },
      },
      urlNorm,
    ),
    /下载失败.*DNS 拒绝/,
  );
});

test("mediaSummaryLabel 生成 recentMessages 摘要", () => {
  assert.equal(
    mediaSummaryLabel({ images: ["a", "b"], video: null }),
    "[图片x2]",
  );
  assert.equal(mediaSummaryLabel({ images: [], video: "v" }), "[视频]");
  assert.equal(
    mediaSummaryLabel({ images: ["a"], video: "v" }),
    "[图片x1+视频]",
  );
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
          path: "/api/socialV2/send-media",
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

test("send-media 把 reply→at→text→image→video 段原样发给网关并记账", async () => {
  const downloads = [];
  await harnessRequest(
    {},
    async ({ h, st, post }) => {
      // messageId 必须是数字：路由按「非零整数」校验 replyToMessageId；
      // resolveReplyTargetV2 先按本地 seq 找，再回退网关——夹具里传 seq 最稳。
      h.appendSocialV2Message(
        KEY,
        "fixture-person",
        "在吗",
        "在吗",
        false,
        false,
        "-900111",
        [],
        "789",
      );
      const replySeq = String(st.recentMessages.at(-1).seq);
      assert.equal(replySeq, "1");
      const res = await post({
        text: "看这个",
        images: [INLINE, "https://cdn.fixture/img.png"],
        video: "https://cdn.fixture/v.mp4",
        replyToMessageId: replySeq,
        atUserId: "10001",
      });
      assert.equal(res.status, 200, JSON.stringify(res.data));
      assert.equal(res.data.ok, true);
      assert.equal(res.data.sent, 1);
      assert.equal(res.data.messageId, 9001, "网关回的 message_id 要透传");
      assert.equal(res.data.summary, "[图片x2+视频]");

      const call = h.calls.http.at(-1);
      assert.match(call.url, /\/send_group_msg$/);
      assert.equal(call.body.group_id, 456);
      const segs = call.body.message;
      assert.deepEqual(
        segs.map((s) => s.type),
        ["reply", "at", "text", "image", "image", "video"],
      );
      // seq → 真实 messageId 映射后，reply 段里装的是真实消息 id
      assert.equal(segs[0].data.id, "-900111");
      assert.equal(segs[1].data.qq, "10001");
      assert.equal(segs[2].data.text, "看这个");
      assert.ok(segs[3].data.file.startsWith("base64://"), "内联图直接成段");
      assert.equal(
        segs[4].data.file,
        "base64://UE5H",
        "URL 图必须先在桥接内下载转码（防网关侧 SSRF）",
      );
      // URL 下载经夹具注入的 safeFetchBuffer，没有真实网络请求
      assert.deepEqual(
        downloads.map((d) => d.url),
        ["https://cdn.fixture/img.png", "https://cdn.fixture/v.mp4"],
      );

      // recentMessages 记账：媒体元数据不入库，靠 messageId 用 qq_get_message_media 读回
      const last = st.recentMessages.at(-1);
      assert.equal(last.sender, "我");
      assert.equal(last.isSelf, true);
      assert.equal(last.messageId, "9001");
      assert.equal(last.text, "[图片x2+视频] 看这个");
      assert.equal(
        last.media.length,
        0,
        "媒体元数据不入库（跨 realm 数组用 length 断言）",
      );
      assert.equal(last.hasMedia, false);
      assert.ok(
        st.lastAiReplyAt > 0,
        "发送要刷新 lastAiReplyAt / 唤醒观测状态",
      );
    },
    {
      globals: {
        safeFetchBuffer: async (url, maxBytes) => {
          downloads.push({ url, maxBytes });
          return { buffer: Buffer.from("PNG") };
        },
      },
    },
  );
});

test("send-media 守住 flag/token/mode/key/白名单/载荷/敏感词/字数闸门", async () => {
  await harnessRequest(
    { socialV2: { tools: { sendMedia: false } } },
    async ({ h, post }) => {
      const res = await post({ images: [INLINE] });
      assert.equal(res.status, 403);
      assert.match(String(res.data.error), /工具未启用：qq_send_media/);
      assert.equal(h.calls.http.length, 0);
    },
  );
  await harnessRequest({}, async ({ h, post }) => {
    assert.equal(
      (await post({ images: [INLINE] }, { token: "wrong-token" })).status,
      403,
      "错 token",
    );
    const noToken = await post({ images: [INLINE] }, { token: null });
    assert.equal(noToken.status, 403);
    assert.match(String(noToken.data.error), /agent token/);
    // token 校验在 key 格式之前（与 send-sticker 同构），所以格式坏 key 报 403
    const badKey = await post({ images: [INLINE] }, { key: "nonsense" });
    assert.equal(badKey.status, 403);
    assert.match(String(badKey.data.error), /agent token|允许范围/);
    assert.equal(
      (await post({ images: [INLINE] }, { key: "group:999" })).status,
      403,
      "白名单外",
    );
    assert.equal(
      (await post({ text: "纯文字" })).status,
      400,
      "没媒体指路 qq_send_message",
    );
    assert.match(
      String((await post({ text: "纯文字" })).data.error),
      /至少其一不能为空/,
    );
    assert.equal(
      (await post({ images: Array(10).fill(INLINE) })).status,
      400,
      "超过 9 张",
    );
    assert.equal((await post({ images: ["junk!"] })).status, 400, "非法地址");
    assert.equal(
      (await post({ images: [INLINE], replyToMessageId: "abc" })).status,
      400,
      "非法引用格式",
    );
    // 私聊 + @：先引导 private:123 会话（自带 agentToken），token/key 都用它的
    const stPrivate = h.getSocialV2State("private:123");
    const priv = await post(
      { images: [INLINE], atUserId: "10001" },
      { key: "private:123", token: stPrivate.agentToken },
    );
    assert.equal(priv.status, 400, "私聊不许 @");
    assert.match(String(priv.data.error), /私聊不需要 @/);
    const sensitive = await post({
      images: [INLINE],
      text: "password: hunter2xyz",
    });
    assert.equal(sensitive.status, 403);
    assert.match(String(sensitive.data.error), /敏感信息/);
    assert.equal(
      (await post({ images: [INLINE], text: "字".repeat(501) })).status,
      400,
      "超单条字数上限",
    );
    assert.equal(h.calls.http.length, 0, "任何闸门失败都不该真的发消息");
  });
});

test("send-media 非 reserved2 模式直接拒绝", async () => {
  await harnessRequest(
    {},
    async ({ post }) => {
      const res = await post({ images: [INLINE] });
      assert.equal(res.status, 403);
      assert.match(String(res.data.error), /reserved2/);
    },
    { mode: "chat" },
  );
});

test("send-media 限频 429：被拒的请求不记账", async () => {
  await harnessRequest(
    { socialV2: { send: { maxSendPerMinute: 1 } } },
    async ({ st, post }) => {
      assert.equal((await post({ images: [INLINE] })).status, 200);
      const second = await post({ images: [INLINE] });
      assert.equal(second.status, 429);
      assert.match(String(second.data.error), /频率超限/);
      assert.equal(st.sendTimes.length, 1, "被拒的那次不该记账");
    },
  );
});

test("send-media 发送失败时 500 并回滚预占额度", async () => {
  await harnessRequest(
    { socialV2: { send: { maxSendPerMinute: 0 } } },
    async ({ st, post }) => {
      // 下载阶段就失败（离线 stub，不发真实网络请求）：此时额度已预占，
      // 路由 catch 必须把它移除并返回中文 500。
      const res = await post({ images: "https://cdn.fixture/x.png" });
      assert.equal(res.status, 500);
      assert.match(String(res.data.error), /发送媒体失败/);
      assert.equal(st.sendTimes.length, 0, "失败后预占额度必须回滚");
    },
    {
      globals: {
        safeFetchBuffer: async () => {
          throw new Error("fixture DNS 拒绝");
        },
      },
    },
  );
});

// ── 3. 开关一致性 + MCP ────────────────────────────────────────────────────
test("sendMedia 开关在四份清单 + MCP + preset + 夹具保持同步", async () => {
  const bridgeSrc = await fs.readFile(path.join(ROOT, "src/bridge.js"), "utf8");
  const defaults = bridgeSrc.match(/^\s*sendMedia: true,$/gm) || [];
  assert.ok(
    defaults.length >= 2,
    `loadConfig 两份默认表都要有 sendMedia（只找到 ${defaults.length} 处）`,
  );
  assert.ok(
    bridgeSrc.includes("'sendMedia'"),
    "console config API 的 toolFlags 要登记",
  );
  assert.ok(
    bridgeSrc.includes("sendMedia: 'qq_send_media'"),
    "console 名称映射 toolMap 要登记",
  );
  assert.ok(bridgeSrc.includes("'/api/socialV2/send-media'"), "路由分发行存在");
  assert.ok(
    bridgeSrc.includes("from './ext/send-media.js'"),
    "ext import 挂钩存在",
  );

  const example = JSON.parse(
    await fs.readFile(path.join(ROOT, "config.example.json"), "utf8"),
  );
  assert.equal(example.socialV2.tools.sendMedia, true);

  const consoleHtml = await fs.readFile(
    path.join(ROOT, "public/console.html"),
    "utf8",
  );
  assert.ok(consoleHtml.includes('data-v2-tool="sendMedia"'), "控制台开关行");
  assert.ok(
    consoleHtml.includes("<b>qq_send_media</b>"),
    "控制台开关展示工具名",
  );

  const mcpSrc = await fs.readFile(
    path.join(ROOT, "src/mcp-snowluma-safe.js"),
    "utf8",
  );
  assert.ok(
    mcpSrc.includes("qq_send_media: 'sendMedia'"),
    "TOOL_CONFIG_FLAGS 登记（漏登记=默认关却仍可见）",
  );
  assert.ok(
    mcpSrc.includes(
      "registerSendMediaTool({ server, z, agentApi, serializeModelData, cfg })",
    ),
    "注册调用挂钩",
  );
  assert.ok(
    mcpSrc.includes("from './ext/send-media-mcp.js'"),
    "MCP ext import 挂钩",
  );

  const preset = await fs.readFile(
    path.join(ROOT, "dsh/agent-presets/qq-chat-v2/agent.cordis.yml"),
    "utf8",
  );
  assert.ok(
    preset.includes("【发图片/图文/视频】qq_send_media"),
    "preset 源能力行",
  );
  const patch = await fs.readFile(
    path.join(ROOT, "plugins/qq-agent-presets/presets/qq-chat-v2.patch.yml"),
    "utf8",
  );
  assert.ok(
    patch.includes("【发图片/图文/视频】qq_send_media"),
    "生成的补丁要同步（跑 build-agent-preset-patches）",
  );

  const harnessSrc = await fs.readFile(
    path.join(ROOT, "scripts/audit-bridge-harness.mjs"),
    "utf8",
  );
  assert.ok(harnessSrc.includes("sendMediaExt"), "夹具要注入 ext 模块");

  await fs.access(path.join(ROOT, "src/ext/DOWNSTREAM.md"));
  const auditSrc = await fs.readFile(
    path.join(ROOT, "scripts/test-audit.mjs"),
    "utf8",
  );
  assert.ok(
    auditSrc.includes("'test-send-media.mjs'"),
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
          messageId: 9001,
          summary: "[图片x1]",
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
    // 与 test-message-media 相同的隔离：只复制源码与最小 config；ext 目录一并复制
    // （mcp-snowluma-safe.js 顶部 import ./ext/send-media-mcp.js）。
    fixture = await fs.mkdtemp(path.join(ROOT, ".tmp-send-media-test-"));
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
    client = new Client({ name: "send-media-test", version: "1.0.0" });
    await client.connect(transport);
    await run({ client, requests });
  } finally {
    if (client) await client.close();
    api.closeAllConnections();
    await new Promise((resolve) => api.close(resolve));
    if (fixture) {
      const resolved = path.resolve(fixture);
      assert.equal(path.dirname(resolved), ROOT);
      assert.ok(path.basename(resolved).startsWith(".tmp-send-media-test-"));
      await fs.rm(resolved, { recursive: true, force: true });
    }
  }
}

test("MCP qq_send_media 默认注册并把载荷/token 转发给桥接", async () => {
  await mcpFixture({}, async ({ client, requests }) => {
    const listed = await client.listTools();
    const tool = listed.tools.find((t) => t.name === "qq_send_media");
    assert.ok(tool, "qq_send_media must be registered by default");
    assert.ok(tool.description.includes("图片"), "描述要说清这是发图/视频");
    assert.ok(tool.description.includes("URL"), "描述要指引大文件走 URL");

    const call = await client.callTool({
      name: "qq_send_media",
      arguments: {
        key: KEY,
        token: "fixture-token",
        images: ["https://a/1.png"],
        video: "https://a/v.mp4",
        text: "配文",
        replyToMessageId: "-9",
      },
    });
    assert.equal(call.isError, undefined, JSON.stringify(call));
    const data = JSON.parse(
      call.content.find((part) => part.type === "text").text,
    );
    assert.equal(data.ok, true);
    assert.equal(data.messageId, 9001);

    const forwarded = requests.at(-1);
    const url = new URL(forwarded.url, "http://fixture");
    assert.equal(url.pathname, "/api/socialV2/send-media");
    assert.equal(
      forwarded.token,
      "fixture-token",
      "session token 必须走 x-agent-token 头",
    );
    const payload = JSON.parse(forwarded.body);
    assert.equal(payload.key, KEY);
    assert.deepEqual(payload.images, ["https://a/1.png"]);
    assert.equal(payload.video, "https://a/v.mp4");
    assert.equal(payload.text, "配文");
    assert.equal(payload.replyToMessageId, "-9");
  });
});

test("MCP qq_send_media 关闭时不注册且不影响其他工具", async () => {
  await mcpFixture(
    { socialV2: { tools: { sendMedia: false } } },
    async ({ client }) => {
      const listed = await client.listTools();
      assert.equal(
        listed.tools.some((t) => t.name === "qq_send_media"),
        false,
      );
      assert.ok(
        listed.tools.some((t) => t.name === "qq_send_message"),
        "关闭一个不该连带关掉别人",
      );
    },
  );
});
