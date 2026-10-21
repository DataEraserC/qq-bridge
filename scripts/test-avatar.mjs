// qq_get_avatar 回归（双端）：
//   1. 桥接端点 /api/socialV2/avatar —— 真实 HTTP 处理器 + fixture QQ，
//      断言 CDN URL 拼装、归属校验（群成员/本群群头像/私聊对方）、四道闸门。
//   2. MCP 工具 qq_get_avatar —— 真实 stdio MCP server，断言它把图像块带回上下文。
// 只用 fixture：不读生产配置、不连 QQ/DSH、不发送任何消息、不真的访问 CDN
// （safeFetchBuffer 在夹具里被桩掉，断言的是"发给谁、带什么上限"）。
//
// 号码全是假的：URL 拼装用占位号 999999999999 / 888888888888。qlogo 对不存在的号
// 回默认头像（实测 200 + image/png），所以"能取到图"不依赖任何真实账号；
// 真网验证见文件末尾 QQ_BRIDGE_TEST_LIVE=1 的分支。
import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import http from 'node:http';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { Client } from '@modelcontextprotocol/sdk/client/index.js';
import { StdioClientTransport } from '@modelcontextprotocol/sdk/client/stdio.js';
import { bridgeHarness } from './audit-bridge-harness.mjs';
import { buildAvatarUrl, normalizeAvatarSize, normalizeQqId, groupIdFromKey } from '../src/ext/avatar.js';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const GROUP_KEY = 'group:456';
const PRIVATE_KEY = 'private:123';
// 1×1 PNG：夹具回给桥接的"头像字节"，让 getImageDimensions/mimeFromBuffer 走真实分支。
const PNG = Buffer.from(
  'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+k3ioAAAAASUVORK5CYII=',
  'base64',
);

// 占位号：不是任何已知账号，也不该被当成真实号码引用。
// 形态仍是"纯数字 ≤12 位"，所以演练的是真实的拼装路径与归一规则。
const FAKE_USER_QQ = '999999999999';
const FAKE_GROUP_QQ = '888888888888';

// ── 纯函数 ──────────────────────────────────────────────────────────────────
test('avatar url builder: user/group shapes, id and size normalisation', () => {
  assert.equal(
    buildAvatarUrl({ kind: 'user', id: FAKE_USER_QQ, size: 640 }),
    `https://q1.qlogo.cn/g?b=qq&nk=${FAKE_USER_QQ}&s=640`,
  );
  assert.equal(
    buildAvatarUrl({ kind: 'group', id: FAKE_GROUP_QQ, size: 100 }),
    `https://p.qlogo.cn/gh/${FAKE_GROUP_QQ}/${FAKE_GROUP_QQ}/100`,
  );
  // 只接受纯数字：这是防 SSRF 的核心 —— 调用方不能借 target 传任意 URL。
  assert.equal(normalizeQqId('https://evil.example/x'), null);
  assert.equal(normalizeQqId('123abc'), null);
  assert.equal(normalizeQqId('0'), null);
  assert.equal(normalizeQqId(''), null);
  assert.equal(normalizeQqId(' 123 '), '123');
  assert.throws(() => buildAvatarUrl({ kind: 'user', id: 'evil', size: 640 }), /纯数字/);
  // 尺寸不做"就近取整"：非法值回落默认，避免缓存与断言失真。
  assert.equal(normalizeAvatarSize(40), 40);
  assert.equal(normalizeAvatarSize('640'), 640);
  assert.equal(normalizeAvatarSize(41), 640);
  assert.equal(normalizeAvatarSize(undefined), 640);
  assert.equal(groupIdFromKey('group:456'), '456');
  assert.equal(groupIdFromKey('private:123'), null);
  assert.equal(groupIdFromKey('nonsense'), null);
});

// ── 桥接端点 ────────────────────────────────────────────────────────────────
async function harnessRequest(config, globals, run) {
  const h = await bridgeHarness({ config, globals });
  h.setMode('reserved2');
  h.setSelfIdentity('fixture-self', '999');
  const st = h.getSocialV2State(GROUP_KEY);
  h.getSocialV2State(PRIVATE_KEY);
  const server = h.startConsoleServer();
  if (!server.listening) await new Promise((resolve) => server.once('listening', resolve));
  const request = (query, { token = st.agentToken, key = GROUP_KEY } = {}) => new Promise((resolve, reject) => {
    const headers = { 'x-console-token': 'fixture-console-token' };
    if (token !== null) headers['x-agent-token'] = token;
    const req = http.request({
      hostname: '127.0.0.1',
      port: server.address().port,
      path: `/api/socialV2/avatar?key=${encodeURIComponent(key)}${query}`,
      method: 'GET',
      headers,
    }, (res) => {
      let data = '';
      res.setEncoding('utf8');
      res.on('data', (chunk) => { data += chunk; });
      res.on('end', () => {
        try { resolve({ status: res.statusCode, data: JSON.parse(data) }); }
        catch (error) { reject(error); }
      });
    });
    req.on('error', reject);
    req.setTimeout(4000, () => req.destroy(new Error('fixture HTTP request timed out')));
    req.end();
  });
  try { await run({ h, st, request }); }
  finally { await new Promise((resolve) => server.close(resolve)); await h.close(); }
}

const fetchStub = (seen) => async (url, maxBytes) => {
  seen.push({ url, maxBytes });
  return { buffer: PNG, mimeType: 'image/png' };
};

test('avatar returns CDN url + image bytes for a group member', async () => {
  const seen = [];
  await harnessRequest({}, { safeFetchBuffer: fetchStub(seen) }, async ({ request }) => {
    const res = await request('&target=789&size=140');
    assert.equal(res.status, 200);
    assert.equal(res.data.ok, true);
    assert.equal(res.data.avatar.kind, 'user');
    assert.equal(res.data.avatar.id, '789');
    assert.equal(res.data.avatar.size, 140);
    assert.equal(res.data.avatar.url, 'https://q1.qlogo.cn/g?b=qq&nk=789&s=140');
    assert.equal(res.data.avatar.width, 1);
    assert.equal(res.data.avatar.height, 1);
    assert.equal(res.data.avatar.bytes, PNG.length);
    // 默认头像无法区分：这条 note 必须随结果回给模型，否则它会编造"这个人换了头像"。
    assert.match(String(res.data.avatar.note), /默认头像/);
    assert.equal(res.data.image.mimeType, 'image/png');
    assert.equal(res.data.image.data, PNG.toString('base64'));
    // 下载上限来自 socialV2.media.maxBytes（经 mediaLimits 注入），不是硬编码。
    assert.equal(seen.length, 1);
    assert.equal(seen[0].maxBytes, 8 * 1024 * 1024);
  });
});

test('avatar urlOnly skips the download entirely', async () => {
  const seen = [];
  await harnessRequest({}, { safeFetchBuffer: fetchStub(seen) }, async ({ request }) => {
    const res = await request('&target=789&urlOnly=1');
    assert.equal(res.status, 200);
    assert.equal(res.data.avatar.url, 'https://q1.qlogo.cn/g?b=qq&nk=789&s=640');
    assert.equal(res.data.image, undefined);
    assert.equal(res.data.avatar.width, null);
    assert.equal(seen.length, 0, 'urlOnly 不该发起下载');
  });
});

test('avatar ownership: group session only sees its own group and its members', async () => {
  const seen = [];
  await harnessRequest({}, { safeFetchBuffer: fetchStub(seen) }, async ({ request, h }) => {
    // 本群群头像：允许。
    const own = await request('&target=456&kind=group');
    assert.equal(own.status, 200);
    assert.equal(own.data.avatar.url, 'https://p.qlogo.cn/gh/456/456/640');
    // 别的群：拒绝。
    const other = await request('&target=999999&kind=group');
    assert.equal(other.status, 403);
    assert.match(String(other.data.error), /只能取当前群/);
    // 非本群成员：拒绝（fail-closed 的另一半——成员列表由夹具给 789/10001）。
    const outsider = await request('&target=555555');
    assert.equal(outsider.status, 403);
    assert.match(String(outsider.data.error), /不是群 456 的成员/);
    // 本群成员：允许。
    const member = await request('&target=10001');
    assert.equal(member.status, 200);
    assert.equal(h.calls.members.includes(456) || h.calls.members.includes('456'), true, '必须真的查过成员列表');
  });
});

test('avatar ownership: private session only sees the peer or self', async () => {
  const seen = [];
  await harnessRequest({}, { safeFetchBuffer: fetchStub(seen) }, async ({ request, h }) => {
    // agentToken 是**按会话**签发的：私聊要用私聊自己的票，不能借用群会话的。
    const peerToken = h.getSocialV2State(PRIVATE_KEY).agentToken;
    const asPeer = (q) => request(q, { key: PRIVATE_KEY, token: peerToken });
    const peer = await asPeer('&target=123');
    assert.equal(peer.status, 200);
    const self = await asPeer('&target=999');
    assert.equal(self.status, 200, '自己的头像必须放行');
    const stranger = await asPeer('&target=777');
    assert.equal(stranger.status, 403);
    assert.match(String(stranger.data.error), /只能取对方或自己/);
    // 私聊不能取群头像。
    const group = await asPeer('&target=123&kind=group');
    assert.equal(group.status, 403);
    assert.match(String(group.data.error), /只有群会话能取群头像/);
  });
});

test('avatar enforces key/target validation and every agent gate', async () => {
  const seen = [];
  await harnessRequest({}, { safeFetchBuffer: fetchStub(seen) }, async ({ request, st }) => {
    assert.equal((await request('&target=789', { key: 'nonsense' })).status, 400);
    assert.equal((await request('')).status, 400);
    assert.equal((await request('&target=abc')).status, 400);
    assert.equal((await request('&target=789&kind=nope')).status, 400);
    assert.equal((await request('&target=789', { token: 'wrong-token' })).status, 403);
    assert.equal((await request('&target=789', { key: 'group:999' })).status, 403);
    assert.ok(st.agentToken.length > 0);
  });
});

test('avatar honors the getAvatar tool flag', async () => {
  const seen = [];
  await harnessRequest(
    { socialV2: { tools: { getAvatar: false } } },
    { safeFetchBuffer: fetchStub(seen) },
    async ({ request }) => {
      const res = await request('&target=789');
      assert.equal(res.status, 403);
      assert.match(String(res.data.error), /工具未启用：qq_get_avatar/);
      assert.equal(seen.length, 0, '闸门拒绝后不该下载');
    },
  );
});

// ── MCP 工具 ────────────────────────────────────────────────────────────────
async function mcpFixture(config, run) {
  const requests = [];
  const api = http.createServer((req, res) => {
    const url = new URL(req.url, 'http://fixture');
    requests.push({ url: req.url, token: req.headers['x-agent-token'] });
    res.writeHead(200, { 'content-type': 'application/json' });
    res.end(JSON.stringify({
      ok: true,
      key: url.searchParams.get('key'),
      avatar: {
        kind: url.searchParams.get('kind') ?? 'user',
        id: url.searchParams.get('target'),
        size: Number(url.searchParams.get('size') ?? 640),
        url: 'https://q1.qlogo.cn/g?b=qq&nk=789&s=640',
        width: 1,
        height: 1,
        bytes: PNG.length,
        note: 'fixture',
      },
      image: { mimeType: 'image/png', data: PNG.toString('base64') },
    }));
  });
  await new Promise((resolve, reject) => {
    api.once('error', reject);
    api.listen(0, '127.0.0.1', resolve);
  });
  let fixture;
  let client;
  try {
    // 与 test-message-media 相同的隔离方式：只复制源码与最小 config，
    // 不把生产配置、凭据或用户运行时状态带进测试夹具。
    fixture = await fs.mkdtemp(path.join(ROOT, '.tmp-avatar-test-'));
    await fs.mkdir(path.join(fixture, 'src'));
    await Promise.all(['mcp-snowluma-safe.js', 'qq-model-view.js', 'sensitive.js'].map((name) =>
      fs.copyFile(path.join(ROOT, 'src', name), path.join(fixture, 'src', name))));
    await fs.cp(path.join(ROOT, 'src', 'ext'), path.join(fixture, 'src', 'ext'), { recursive: true });
    await fs.writeFile(path.join(fixture, 'config.json'), JSON.stringify({ consolePort: api.address().port, ...config }));
    const transport = new StdioClientTransport({
      command: process.execPath,
      args: [path.join(fixture, 'src', 'mcp-snowluma-safe.js')], cwd: fixture, stderr: 'pipe',
    });
    client = new Client({ name: 'avatar-test', version: '1.0.0' });
    await client.connect(transport);
    await run({ client, requests });
  } finally {
    if (client) await client.close();
    api.closeAllConnections();
    await new Promise((resolve) => api.close(resolve));
    if (fixture) {
      const resolved = path.resolve(fixture);
      assert.equal(path.dirname(resolved), ROOT);
      assert.ok(path.basename(resolved).startsWith('.tmp-avatar-test-'));
      await fs.rm(resolved, { recursive: true, force: true });
    }
  }
}

test('MCP qq_get_avatar returns an image block and passes the agent token', async () => {
  await mcpFixture({ socialV2: { tools: { getAvatar: true } } }, async ({ client, requests }) => {
    const result = await client.callTool({
      name: 'qq_get_avatar',
      arguments: { key: GROUP_KEY, token: 'fixture-token', target: '789' },
    });
    assert.equal(result.isError, undefined);
    const image = result.content.find((c) => c.type === 'image');
    assert.ok(image, '必须回图像块（进视觉上下文）');
    assert.equal(image.mimeType, 'image/png');
    assert.equal(image.data, PNG.toString('base64'));
    const text = result.content.find((c) => c.type === 'text');
    const summary = JSON.parse(text.text);
    assert.equal(summary.id, '789');
    assert.equal(summary.url, 'https://q1.qlogo.cn/g?b=qq&nk=789&s=640');
    assert.equal(requests.length, 1);
    assert.equal(requests[0].token, 'fixture-token');
    assert.match(requests[0].url, /^\/api\/socialV2\/avatar\?/);
  });
});

test('MCP qq_get_avatar is absent when the flag is off', async () => {
  await mcpFixture({ socialV2: { tools: { getAvatar: false } } }, async ({ client }) => {
    const { tools } = await client.listTools();
    assert.equal(tools.some((t) => t.name === 'qq_get_avatar'), false);
  });
});

// ── 开关一致性 ──────────────────────────────────────────────────────────────
test('getAvatar 开关在四份清单 + MCP + preset + 夹具保持同步', async () => {
  const bridgeSrc = await fs.readFile(path.join(ROOT, 'src/bridge.js'), 'utf8');
  const defaults = bridgeSrc.match(/^\s*getAvatar: true,?$/gm) || [];
  assert.ok(defaults.length >= 2, `loadConfig 两份默认表都要有 getAvatar（只找到 ${defaults.length} 处）`);
  assert.ok(bridgeSrc.includes("'getAvatar'"), 'console config API 的 toolFlags 要登记');
  assert.ok(bridgeSrc.includes("getAvatar: 'qq_get_avatar'"), 'console 名称映射 toolMap 要登记');
  assert.ok(bridgeSrc.includes("'/api/socialV2/avatar'"), '路由分发行存在');
  assert.ok(bridgeSrc.includes("from './ext/avatar.js'"), 'ext import 挂钩存在');

  const example = JSON.parse(await fs.readFile(path.join(ROOT, 'config.example.json'), 'utf8'));
  assert.equal(example.socialV2.tools.getAvatar, true);

  const consoleHtml = await fs.readFile(path.join(ROOT, 'public/console.html'), 'utf8');
  assert.ok(consoleHtml.includes('data-v2-tool="getAvatar"'), '控制台开关行');
  assert.ok(consoleHtml.includes('<b>qq_get_avatar</b>'), '控制台开关展示工具名');

  const mcpSrc = await fs.readFile(path.join(ROOT, 'src/mcp-snowluma-safe.js'), 'utf8');
  assert.ok(mcpSrc.includes("qq_get_avatar: 'getAvatar'"), 'TOOL_CONFIG_FLAGS 登记（漏登记=默认关却仍可见）');
  assert.ok(
    mcpSrc.includes('registerAvatarTool({ server, z, agentApi, cfg })'),
    '注册调用挂钩（漏了它工具根本不存在）',
  );
  assert.ok(mcpSrc.includes("from './ext/avatar-mcp.js'"), 'MCP ext import 挂钩');

  const preset = await fs.readFile(path.join(ROOT, 'dsh/agent-presets/qq-chat-v2/agent.cordis.yml'), 'utf8');
  assert.ok(preset.includes('qq_get_avatar'), 'preset 源能力行');
  const patch = await fs.readFile(path.join(ROOT, 'plugins/qq-agent-presets/presets/qq-chat-v2.patch.yml'), 'utf8');
  assert.ok(patch.includes('qq_get_avatar'), '生成的补丁要同步（跑 build-agent-preset-patches）');

  const harnessSrc = await fs.readFile(path.join(ROOT, 'scripts/audit-bridge-harness.mjs'), 'utf8');
  assert.ok(harnessSrc.includes('avatarExt'), '夹具要注入 ext 模块');
});

// ── 真网：假号也能出图 ──────────────────────────────────────────────────────
// 默认跳过（CI 不依赖外网）。要证明"占位号照样能拿到一张真实图片"时跑：
//   QQ_BRIDGE_TEST_LIVE=1 npm run test-avatar
// 断言的是 qlogo 的真实行为：不存在的号回**默认头像**（200 + image/*），
// 所以换掉真实号码不会让"有图"这条退化 —— 这也是工具本身依赖的性质
// （路线：CDN 对任何纯数字号都回图，无法区分"没设头像"与"号不存在"）。
if (process.env.QQ_BRIDGE_TEST_LIVE === '1') {
  test('live: 占位号在 qlogo 上仍返回一张可解码的图片', async () => {
    for (const spec of [
      { kind: 'user', id: FAKE_USER_QQ, size: 640 },
      { kind: 'group', id: FAKE_GROUP_QQ, size: 640 },
    ]) {
      const url = buildAvatarUrl(spec);
      const res = await fetch(url, { signal: AbortSignal.timeout(15000) });
      assert.equal(res.status, 200, `${spec.kind} 应 200：${url}`);
      const mime = String(res.headers.get('content-type') || '');
      assert.match(mime, /^image\//, `${spec.kind} 应回图片：${url} → ${mime}`);
      const buf = Buffer.from(await res.arrayBuffer());
      assert.ok(buf.length > 0, `${spec.kind} 图片不能为空`);
      // 真 PNG/JPEG 签名：证明拿到的不是错误页 HTML。
      const magic = buf.subarray(0, 8).toString('hex');
      assert.ok(
        magic.startsWith('89504e47') || magic.startsWith('ffd8ff'),
        `${spec.kind} 应是 PNG/JPEG，实际 magic=${magic}`,
      );
    }
  });
}
