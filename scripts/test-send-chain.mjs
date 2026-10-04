// qq_send_chain 回归（三面）：
//   1. 纯函数单测 —— 段白名单归一化（扁平/嵌套入形状）、**段序原样保持**（图文交错）、
//      段数/图片/视频上限、text 段合计、计数标签、构建时内联不走网络、
//      本地绝对路径直读、发送前字节魔数校验（损坏内容就地拒绝）、下载失败中文错误；
//   2. 桥接端点 /api/socialV2/send-chain —— 真实 HTTP 处理器 + fixture，
//      用 fakeFetch 观测**实际发给网关的 OneBot 段序**
//      （reply→at→text→image→text→image→face→at 原样交错，不重排），
//      守住 flag/token/mode/key/白名单/静默/引用/敏感词/合计字数/限频闸门与失败回滚；
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
import { MEDIA_LIMITS } from "../src/ext/send-media.js";
import {
  CHAIN_LIMITS,
  CHAIN_SEGMENT_TYPES,
  normalizeChainPayload,
  chainText,
  buildChainSegments,
  chainSummaryLabel,
} from "../src/ext/send-chain.js";

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const KEY = "group:456";
// 发送前有字节魔数校验（见 send-media.js），夹具必须用「像图片/视频」的字节：
// PNG 魔数 + 填充 + 尾部 IEND，≥64 base64 字符仍命中纯 base64 启发式。
const PNG_BYTES = Buffer.concat([
  Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
  Buffer.alloc(60, 0x61),
  Buffer.from([0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82]),
]);
const MP4_BYTES = Buffer.concat([
  Buffer.from([0, 0, 0, 0]),
  Buffer.from("ftypisom"),
  Buffer.alloc(16, 0x20),
]);
const PNG_B64 = PNG_BYTES.toString("base64");
const INLINE = PNG_B64; // ≥64 字符，命中纯 base64 启发式
// 夹具版 safeFetchBuffer：按 URL 分流返回合法图片/视频字节
const fixtureFetch = async (url, maxBytes, downloads) => {
  if (downloads) downloads.push({ url, maxBytes });
  return { buffer: String(url).includes(".mp4") ? MP4_BYTES : PNG_BYTES };
};

// ── 1. 纯函数 ───────────────────────────────────────────────────────────────
test("normalizeChainPayload 按原顺序归一化五种段（扁平+嵌套入形状）", () => {
  const norm = normalizeChainPayload({
    segments: [
      { type: "text", text: "  你看这个  " },
      { type: "image", url: "https://cdn/1.png" },
      { type: "text", text: "第二张来了" },
      { type: "image", data: { file: INLINE } },
      { type: "face", id: "178" },
      { type: "at", qq: "10001" },
    ],
    replyToMessageId: "-9",
    atUserId: "10001",
  });
  // 关键契约：段序不重排（这就是图文交错）
  assert.deepEqual(
    norm.segments.map((s) => s.type),
    ["text", "image", "text", "image", "face", "at"],
  );
  assert.equal(norm.segments[0].text, "你看这个", "text 去空白");
  assert.equal(norm.segments[3].ref, INLINE, "嵌套 data.file 也能收");
  assert.equal(norm.segments[4].id, "178");
  assert.equal(norm.segments[5].qq, "10001");
  assert.equal(norm.replyToMessageId, "-9");
  assert.equal(norm.atUserId, "10001");
});

test("normalizeChainPayload 强制段白名单与各上限", () => {
  assert.deepEqual(CHAIN_SEGMENT_TYPES, [
    "text",
    "image",
    "video",
    "face",
    "at",
  ]);
  assert.throws(() => normalizeChainPayload({}), /非空数组/);
  assert.throws(() => normalizeChainPayload({ segments: [] }), /非空数组/);
  assert.throws(() => normalizeChainPayload({ segments: "text" }), /非空数组/);
  assert.throws(
    () => normalizeChainPayload({ segments: [{ type: "json", data: {} }] }),
    /不在白名单/,
    "json 段能构造合并转发，必须留在白名单外",
  );
  assert.throws(
    () => normalizeChainPayload({ segments: [{ type: "reply", data: {} }] }),
    /replyToMessageId/,
    "reply 不是段类型——必须在最前，走顶层参数",
  );
  assert.throws(
    () => normalizeChainPayload({ segments: [{ type: "text", text: "  " }] }),
    /不能为空/,
  );
  assert.throws(
    () => normalizeChainPayload({ segments: [{ type: "face", id: "xx" }] }),
    /数字 id/,
  );
  assert.throws(
    () => normalizeChainPayload({ segments: [{ type: "at", qq: "all" }] }),
    /正整数/,
  );
  assert.throws(
    () =>
      normalizeChainPayload({ segments: [{ type: "image", url: "junk!" }] }),
    /格式不支持/,
  );
  assert.throws(
    () =>
      normalizeChainPayload({
        segments: Array(CHAIN_LIMITS.maxSegments + 1).fill({
          type: "text",
          text: "x",
        }),
      }),
    /最多 20 段/,
  );
  assert.throws(
    () =>
      normalizeChainPayload({
        segments: Array(10).fill({ type: "image", url: "https://a/x.png" }),
      }),
    /最多 9 个/,
  );
  assert.throws(
    () =>
      normalizeChainPayload({
        segments: [
          { type: "video", url: "https://a/v1.mp4" },
          { type: "video", url: "https://a/v2.mp4" },
        ],
      }),
    /最多 1 个/,
  );
});

test("chainText 汇总 text 段 / chainSummaryLabel 按类型计数", () => {
  const norm = normalizeChainPayload({
    segments: [
      { type: "text", text: "甲" },
      { type: "image", url: "https://a/1.png" },
      { type: "text", text: "乙" },
      { type: "face", id: "1" },
      { type: "at", qq: "2" },
    ],
  });
  assert.equal(chainText(norm), "甲乙", "合计是各 text 段按序拼接");
  assert.equal(chainSummaryLabel(norm), "[消息链 文x2+图x1+表情x1+@x1]");
  const videoNorm = normalizeChainPayload({
    segments: [{ type: "video", url: "https://a/v.mp4" }],
  });
  assert.equal(chainSummaryLabel(videoNorm), "[消息链 视频]");
});

test("buildChainSegments 保持段序，内联不走网络、URL 按类型上限下载", async () => {
  const downloads = [];
  const deps = {
    safeFetchBuffer: async (url, maxBytes) =>
      fixtureFetch(url, maxBytes, downloads),
  };
  const norm = normalizeChainPayload({
    segments: [
      { type: "text", text: "看这个" },
      { type: "image", url: "https://cdn/img.png" },
      { type: "text", text: "第二张来了" },
      { type: "image", url: INLINE },
      { type: "video", url: "https://cdn/v.mp4" },
      { type: "face", id: "178" },
      { type: "at", qq: "10001" },
    ],
  });
  const segments = await buildChainSegments(deps, norm);
  assert.deepEqual(
    segments.map((s) => s.type),
    ["text", "image", "text", "image", "video", "face", "at"],
    "段序与入参完全一致（图文交错）",
  );
  assert.equal(
    segments[1].data.file,
    "base64://" + PNG_B64,
    "URL 下载转 base64://",
  );
  assert.equal(segments[3].data.file, "base64://" + INLINE, "内联直接成段");
  assert.equal(segments[5].data.id, 178, "face id 发给网关用数字");
  assert.equal(segments[6].data.qq, "10001", "at 段 qq 保留字符串");
  assert.deepEqual(
    downloads.map((d) => d.url),
    ["https://cdn/img.png", "https://cdn/v.mp4"],
  );
  assert.deepEqual(
    downloads.map((d) => d.maxBytes),
    [MEDIA_LIMITS.maxImageBytes, MEDIA_LIMITS.maxVideoBytes],
    "上限分别来自图片/视频常量",
  );
});

test("buildChainSegments 拒绝超限内联载荷与下载失败", async () => {
  const limits = { ...MEDIA_LIMITS, maxImageBytes: 8 };
  const big = Buffer.from("x".repeat(64)).toString("base64"); // 解码 64B > 8B
  const norm = normalizeChainPayload({
    segments: [{ type: "image", url: big }],
  });
  await assert.rejects(
    buildChainSegments(
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
  const urlNorm = normalizeChainPayload({
    segments: [{ type: "image", url: "https://cdn/x.png" }],
  });
  await assert.rejects(
    buildChainSegments(
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

test("buildChainSegments 读取本地绝对路径段（不走网络）", async () => {
  const dir = await fs.mkdtemp(path.join(os.tmpdir(), "qqc-"));
  try {
    const file = path.join(dir, "图 a.png");
    await fs.writeFile(file, PNG_BYTES);
    const deps = {
      safeFetchBuffer: async () => {
        throw new Error("本地文件不该走网络");
      },
    };
    const norm = normalizeChainPayload({
      segments: [
        { type: "text", text: "本地" },
        { type: "image", url: file },
        { type: "image", url: `file://${file}` },
      ],
    });
    const segments = await buildChainSegments(deps, norm);
    assert.equal(segments[1].data.file, "base64://" + PNG_B64);
    assert.equal(segments[2].data.file, "base64://" + PNG_B64);
  } finally {
    await fs.rm(dir, { recursive: true, force: true });
  }
});

test("chain 损坏字节走同一套发送前校验（就地中文拒绝）", async () => {
  const deps = { safeFetchBuffer: async () => ({ buffer: PNG_BYTES }) };
  // 长度非法（mod 4 = 1）
  await assert.rejects(
    buildChainSegments(
      deps,
      normalizeChainPayload({
        segments: [{ type: "image", url: "A".repeat(65) }],
      }),
    ),
    /长度非法/,
  );
  // URL 下到 HTML 错误页
  const htmlFetch = {
    safeFetchBuffer: async () => ({
      buffer: Buffer.from("<html><body>500 Internal Error</body></html>"),
    }),
  };
  await assert.rejects(
    buildChainSegments(
      htmlFetch,
      normalizeChainPayload({
        segments: [{ type: "image", url: "https://cdn/x.png" }],
      }),
    ),
    /无法识别/,
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
          path: "/api/socialV2/send-chain",
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

test("send-chain 把 reply→text→image→text→image→face→at 交错段原样发给网关并记账", async () => {
  const downloads = [];
  await harnessRequest(
    {},
    async ({ h, st, post }) => {
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
      const res = await post({
        segments: [
          { type: "text", text: "看这个" },
          { type: "image", url: "https://cdn.fixture/img.png" },
          { type: "text", text: "第二张来了" },
          { type: "image", url: INLINE },
          { type: "face", id: "178" },
          { type: "at", qq: "10001" },
        ],
        replyToMessageId: replySeq,
      });
      assert.equal(res.status, 200, JSON.stringify(res.data));
      assert.equal(res.data.ok, true);
      assert.equal(res.data.sent, 1);
      assert.equal(res.data.messageId, 9001, "网关回的 message_id 要透传");
      assert.equal(res.data.summary, "[消息链 文x2+图x2+表情x1+@x1]");

      const call = h.calls.http.at(-1);
      assert.match(call.url, /\/send_group_msg$/);
      assert.equal(call.body.group_id, 456);
      const segs = call.body.message;
      assert.deepEqual(
        segs.map((s) => s.type),
        ["reply", "text", "image", "text", "image", "face", "at"],
        "reply 头段在最前，其余段序与入参完全一致（图文交错不重排）",
      );
      assert.equal(segs[0].data.id, "-900111", "seq → 真实 messageId 映射");
      assert.equal(segs[1].data.text, "看这个");
      assert.equal(
        segs[2].data.file,
        "base64://" + PNG_B64,
        "URL 图必须先在桥接内下载转码（防网关侧 SSRF）",
      );
      assert.equal(segs[3].data.text, "第二张来了");
      assert.equal(segs[4].data.file, "base64://" + INLINE);
      assert.equal(segs[5].data.id, 178);
      assert.equal(segs[6].data.qq, "10001");
      // 只有 URL 图走下载；夹具注入的 safeFetchBuffer 收到图片上限
      assert.deepEqual(
        downloads.map((d) => d.url),
        ["https://cdn.fixture/img.png"],
      );
      assert.deepEqual(
        downloads.map((d) => d.maxBytes),
        [MEDIA_LIMITS.maxImageBytes],
      );

      const last = st.recentMessages.at(-1);
      assert.equal(last.sender, "我");
      assert.equal(last.isSelf, true);
      assert.equal(last.messageId, "9001");
      assert.equal(
        last.text,
        "[消息链 文x2+图x2+表情x1+@x1] 看这个第二张来了",
        "记录为摘要标签 + 合并后的 text",
      );
      assert.equal(last.media.length, 0, "媒体元数据不入库");
      assert.equal(last.hasMedia, false);
      assert.ok(st.lastAiReplyAt > 0, "发送要刷新 lastAiReplyAt");
    },
    {
      globals: {
        safeFetchBuffer: async (url, maxBytes) =>
          fixtureFetch(url, maxBytes, downloads),
      },
    },
  );
});

test("send-chain 守住 flag/token/mode/key/白名单/段白名单/合计字数/敏感词闸门", async () => {
  await harnessRequest(
    { socialV2: { tools: { sendChain: false } } },
    async ({ h, post }) => {
      const res = await post({
        segments: [{ type: "text", text: "hi" }],
      });
      assert.equal(res.status, 403);
      assert.match(String(res.data.error), /工具未启用：qq_send_chain/);
      assert.equal(h.calls.http.length, 0);
    },
  );
  await harnessRequest({}, async ({ h, post }) => {
    const seg = [{ type: "text", text: "hi" }];
    assert.equal(
      (await post({ segments: seg }, { token: "wrong-token" })).status,
      403,
      "错 token",
    );
    const noToken = await post({ segments: seg }, { token: null });
    assert.equal(noToken.status, 403);
    assert.match(String(noToken.data.error), /agent token/);
    // token 校验在 key 格式之前（与 send-media 同构），所以格式坏 key 报 403
    const badKey = await post({ segments: seg }, { key: "nonsense" });
    assert.equal(badKey.status, 403);
    assert.match(String(badKey.data.error), /agent token|允许范围/);
    assert.equal(
      (await post({ segments: seg }, { key: "group:999" })).status,
      403,
      "白名单外",
    );
    // 载荷闸门
    assert.equal((await post({})).status, 400, "缺 segments");
    assert.match(
      String((await post({})).data.error),
      /qq_send_message/,
      "纯文本该指路 qq_send_message",
    );
    assert.equal(
      (await post({ segments: [{ type: "json", data: {} }] })).status,
      400,
      "白名单外段类型",
    );
    assert.equal((await post({ segments: "text" })).status, 400, "非数组");
    assert.equal(
      (
        await post({
          segments: Array(21).fill({ type: "text", text: "x" }),
        })
      ).status,
      400,
      "超 20 段",
    );
    assert.equal(
      (await post({ segments: Array(10).fill({ type: "image", url: INLINE }) }))
        .status,
      400,
      "超 9 图",
    );
    assert.equal(
      (
        await post({
          segments: [
            { type: "video", url: "https://a/v1.mp4" },
            { type: "video", url: "https://a/v2.mp4" },
          ],
        })
      ).status,
      400,
      "超 1 视频",
    );
    assert.equal(
      (
        await post({
          segments: seg,
          replyToMessageId: "abc",
        })
      ).status,
      400,
      "非法引用格式",
    );
    // 私聊 + 链内 at 段：先引导 private:123 会话（自带 agentToken）
    const stPrivate = h.getSocialV2State("private:123");
    const priv = await post(
      {
        segments: [
          { type: "text", text: "hi" },
          { type: "at", qq: "10001" },
        ],
      },
      { key: "private:123", token: stPrivate.agentToken },
    );
    assert.equal(priv.status, 400, "私聊不许 @（链内也不行）");
    assert.match(String(priv.data.error), /私聊不需要 @/);
    // 敏感词按 text 段**合计**（拆成两段也拦得住）
    const sensitive = await post({
      segments: [
        { type: "text", text: "password: " },
        { type: "text", text: "hunter2xyz" },
      ],
    });
    assert.equal(sensitive.status, 403);
    assert.match(String(sensitive.data.error), /敏感信息/);
    // 合计字数：两段各 300 字、合计 600 > 单条 500 上限（防拆段绕过）
    const long = await post({
      segments: [
        { type: "text", text: "字".repeat(300) },
        { type: "text", text: "字".repeat(300) },
      ],
    });
    assert.equal(long.status, 400);
    assert.match(String(long.data.error), /text 段合计 600 字/);
    assert.equal(h.calls.http.length, 0, "任何闸门失败都不该真的发消息");
  });
});

test("send-chain 非 reserved2 模式直接拒绝", async () => {
  await harnessRequest(
    {},
    async ({ post }) => {
      const res = await post({ segments: [{ type: "text", text: "hi" }] });
      assert.equal(res.status, 403);
      assert.match(String(res.data.error), /reserved2/);
    },
    { mode: "chat" },
  );
});

test("send-chain 限频 429：被拒的请求不记账", async () => {
  await harnessRequest(
    { socialV2: { send: { maxSendPerMinute: 1 } } },
    async ({ st, post }) => {
      assert.equal(
        (await post({ segments: [{ type: "text", text: "hi" }] })).status,
        200,
      );
      const second = await post({ segments: [{ type: "text", text: "hi" }] });
      assert.equal(second.status, 429);
      assert.match(String(second.data.error), /频率超限/);
      assert.equal(st.sendTimes.length, 1, "被拒的那次不该记账");
    },
  );
});

test("send-chain 发送失败时 500 并回滚预占额度", async () => {
  await harnessRequest(
    { socialV2: { send: { maxSendPerMinute: 0 } } },
    async ({ st, post }) => {
      const res = await post({
        segments: [{ type: "image", url: "https://cdn.fixture/x.png" }],
      });
      assert.equal(res.status, 500);
      assert.match(String(res.data.error), /发送消息链失败/);
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
test("sendChain 开关在四份清单 + MCP + preset + 夹具保持同步", async () => {
  const bridgeSrc = await fs.readFile(path.join(ROOT, "src/bridge.js"), "utf8");
  const defaults = bridgeSrc.match(/^\s*sendChain: true,$/gm) || [];
  assert.ok(
    defaults.length >= 2,
    `loadConfig 两份默认表都要有 sendChain（只找到 ${defaults.length} 处）`,
  );
  assert.ok(
    bridgeSrc.includes("'sendChain'"),
    "console config API 的 toolFlags 要登记",
  );
  assert.ok(
    bridgeSrc.includes("sendChain: 'qq_send_chain'"),
    "console 名称映射 toolMap 要登记",
  );
  assert.ok(bridgeSrc.includes("'/api/socialV2/send-chain'"), "路由分发行存在");
  assert.ok(
    bridgeSrc.includes("from './ext/send-chain.js'"),
    "ext import 挂钩存在",
  );

  const example = JSON.parse(
    await fs.readFile(path.join(ROOT, "config.example.json"), "utf8"),
  );
  assert.equal(example.socialV2.tools.sendChain, true);

  const consoleHtml = await fs.readFile(
    path.join(ROOT, "public/console.html"),
    "utf8",
  );
  assert.ok(consoleHtml.includes('data-v2-tool="sendChain"'), "控制台开关行");
  assert.ok(
    consoleHtml.includes("<b>qq_send_chain</b>"),
    "控制台开关展示工具名",
  );

  const mcpSrc = await fs.readFile(
    path.join(ROOT, "src/mcp-snowluma-safe.js"),
    "utf8",
  );
  assert.ok(
    mcpSrc.includes("qq_send_chain: 'sendChain'"),
    "TOOL_CONFIG_FLAGS 登记（漏登记=默认关却仍可见）",
  );
  assert.ok(
    mcpSrc.includes(
      "registerSendChainTool({ server, z, agentApi, serializeModelData, cfg })",
    ),
    "注册调用挂钩",
  );
  assert.ok(
    mcpSrc.includes("from './ext/send-chain-mcp.js'"),
    "MCP ext import 挂钩",
  );

  const preset = await fs.readFile(
    path.join(ROOT, "dsh/agent-presets/qq-chat-v2/agent.cordis.yml"),
    "utf8",
  );
  assert.ok(
    preset.includes("【图文交错（消息链）】qq_send_chain"),
    "preset 源能力行",
  );
  const patch = await fs.readFile(
    path.join(ROOT, "plugins/qq-agent-presets/presets/qq-chat-v2.patch.yml"),
    "utf8",
  );
  assert.ok(
    patch.includes("【图文交错（消息链）】qq_send_chain"),
    "生成的补丁要同步（跑 build-agent-preset-patches）",
  );

  const harnessSrc = await fs.readFile(
    path.join(ROOT, "scripts/audit-bridge-harness.mjs"),
    "utf8",
  );
  assert.ok(harnessSrc.includes("sendChainExt"), "夹具要注入 ext 模块");

  // import 行被剥离的 VM 夹具要把 ext 注册函数当上下文值注入
  const securitySrc = await fs.readFile(
    path.join(ROOT, "scripts/test-audit-security-mcp.mjs"),
    "utf8",
  );
  assert.ok(
    securitySrc.includes("registerSendChainTool"),
    "security-mcp 夹具注入（否则 registerSendChainTool is not defined）",
  );

  await fs.access(path.join(ROOT, "src/ext/DOWNSTREAM.md"));
  const auditSrc = await fs.readFile(
    path.join(ROOT, "scripts/test-audit.mjs"),
    "utf8",
  );
  assert.ok(
    auditSrc.includes("'test-send-chain.mjs'"),
    "本测试要登记进 test-audit 白名单",
  );
  const pkg = JSON.parse(
    await fs.readFile(path.join(ROOT, "package.json"), "utf8"),
  );
  assert.ok(pkg.scripts["test-send-chain"], "package.json 要有 test 串联");
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
          summary: "[消息链 文x1+图x1]",
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
    // （mcp-snowluma-safe.js 顶部 import ./ext/send-chain-mcp.js）。
    fixture = await fs.mkdtemp(path.join(ROOT, ".tmp-send-chain-test-"));
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
    client = new Client({ name: "send-chain-test", version: "1.0.0" });
    await client.connect(transport);
    await run({ client, requests });
  } finally {
    if (client) await client.close();
    api.closeAllConnections();
    await new Promise((resolve) => api.close(resolve));
    if (fixture) {
      const resolved = path.resolve(fixture);
      assert.equal(path.dirname(resolved), ROOT);
      assert.ok(path.basename(resolved).startsWith(".tmp-send-chain-test-"));
      await fs.rm(resolved, { recursive: true, force: true });
    }
  }
}

test("MCP qq_send_chain 默认注册并把 segments/token 转发给桥接", async () => {
  await mcpFixture({}, async ({ client, requests }) => {
    const listed = await client.listTools();
    const tool = listed.tools.find((t) => t.name === "qq_send_chain");
    assert.ok(tool, "qq_send_chain must be registered by default");
    assert.ok(tool.description.includes("交错"), "描述要说清图文交错能力");
    assert.ok(tool.description.includes("segments"), "描述要说明段数组入参");

    const call = await client.callTool({
      name: "qq_send_chain",
      arguments: {
        key: KEY,
        token: "fixture-token",
        segments: [
          { type: "text", text: "看这个" },
          { type: "image", url: "https://a/1.png" },
          { type: "text", text: "第二句" },
          { type: "image", url: "https://a/2.png" },
        ],
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
    assert.equal(url.pathname, "/api/socialV2/send-chain");
    assert.equal(
      forwarded.token,
      "fixture-token",
      "session token 必须走 x-agent-token 头",
    );
    const payload = JSON.parse(forwarded.body);
    assert.equal(payload.key, KEY);
    assert.deepEqual(payload.segments, [
      { type: "text", text: "看这个" },
      { type: "image", url: "https://a/1.png" },
      { type: "text", text: "第二句" },
      { type: "image", url: "https://a/2.png" },
    ]);
    assert.equal(payload.replyToMessageId, "-9");
  });
});

test("MCP qq_send_chain 关闭时不注册且不影响其他工具", async () => {
  await mcpFixture(
    { socialV2: { tools: { sendChain: false } } },
    async ({ client }) => {
      const listed = await client.listTools();
      assert.equal(
        listed.tools.some((t) => t.name === "qq_send_chain"),
        false,
      );
      assert.ok(
        listed.tools.some((t) => t.name === "qq_send_media"),
        "关闭一个不该连带关掉别人",
      );
      assert.ok(
        listed.tools.some((t) => t.name === "qq_send_message"),
        "关闭一个不该连带关掉别人",
      );
    },
  );
});
