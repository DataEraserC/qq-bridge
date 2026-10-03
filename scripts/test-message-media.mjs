// qq_get_message_media 回归（双端）：
//   1. 桥接端点 /api/socialV2/message-media —— 真实 HTTP 处理器 + fixture QQ/DSH，
//      断言它把**原始 media.url/file** 原样回给带 agent token 的调用方，并守住 400/403 闸门。
//   2. MCP 工具 qq_get_message_media —— 真实 stdio MCP server，断言它不经
//      serializeModelData（模型视图会把 media.url 压成 index 句柄，那正是本工具要补的洞）。
// 只用 fixture：不读生产配置、不连 QQ/DSH、不发送任何消息。
import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import http from 'node:http';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { Client } from '@modelcontextprotocol/sdk/client/index.js';
import { StdioClientTransport } from '@modelcontextprotocol/sdk/client/stdio.js';
import { bridgeHarness } from './audit-bridge-harness.mjs';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const KEY = 'group:456';
const MEDIA_URL = 'https://multimedia.nt.qq.com.cn/download?fixture=1&rkey=fixture-rkey';
const MEDIA = { kind: 'image', file: 'fixture-local-image.jpg', url: MEDIA_URL };

// ── 桥接端点 ────────────────────────────────────────────────────────────────
async function harnessRequest(config, run) {
  const h = await bridgeHarness({ config });
  h.setMode('reserved2');
  const st = h.getSocialV2State(KEY);
  const server = h.startConsoleServer();
  if (!server.listening) await new Promise((resolve) => server.once('listening', resolve));
  const request = (route, { token = st.agentToken, key = KEY, query = '' } = {}) => new Promise((resolve, reject) => {
    const headers = { 'x-console-token': 'fixture-console-token' };
    if (token !== null) headers['x-agent-token'] = token;
    const req = http.request({ hostname: '127.0.0.1', port: server.address().port,
      path: `/api/socialV2/${route}?key=${encodeURIComponent(key)}${query}`,
      method: 'GET', headers,
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
  const append = (text, media) => {
    h.appendSocialV2Message(
      KEY, 'fixture-person', text, text, false, false,
      `fixture-${h.getSocialV2State(KEY).lastUnreadSeq + 1}`, media, '789',
    );
    return h.getSocialV2State(KEY).lastUnreadSeq;
  };
  try { await run({ h, st, request, append }); }
  finally { await new Promise((resolve) => server.close(resolve)); await h.close(); }
}

test('message-media returns the raw media url by messageId and by local seq', async () => {
  await harnessRequest({}, async ({ st, request, append }) => {
    const seq = append('带图消息', [MEDIA]);
    const messageId = st.recentMessages.at(-1).messageId;
    for (const ref of [messageId, String(seq)]) {
      const res = await request('message-media', { query: `&messageId=${encodeURIComponent(ref)}` });
      assert.equal(res.status, 200, `reference ${ref}`);
      assert.equal(res.data.ok, true);
      assert.equal(res.data.media.length, 1);
      // 核心契约：原始 url/file 必须原样回传（模型视图只该压 messages/newMessages）。
      assert.equal(res.data.media[0].url, MEDIA_URL, `url must survive for ${ref}`);
      assert.equal(res.data.media[0].file, MEDIA.file);
    }
    // 对照：无媒体消息回空数组 + note，而不是报错。
    append('纯文本消息');
    const textOnly = await request('message-media', { query: '&messageId=fixture-2' });
    assert.equal(textOnly.status, 200);
    assert.deepEqual(textOnly.data.media, []);
    assert.match(String(textOnly.data.note), /没有可用的媒体元数据/);
  });
});

test('message-media enforces key/messageId validation and every agent gate', async () => {
  await harnessRequest({}, async ({ st, request }) => {
    assert.equal((await request('message-media', { key: 'nonsense', query: '&messageId=1' })).status, 400);
    assert.equal((await request('message-media', { query: '' })).status, 400);
    assert.equal((await request('message-media', { token: 'wrong-token', query: '&messageId=1' })).status, 403);
    assert.equal((await request('message-media', { key: 'group:999', query: '&messageId=1' })).status, 403);
    assert.equal(st.agentToken.length > 0, true);
  });
});

test('message-media honors the getMessageMedia tool flag', async () => {
  await harnessRequest({ socialV2: { tools: { getMessageMedia: false } } }, async ({ request, append }) => {
    append('带图消息', [MEDIA]);
    const res = await request('message-media', { query: '&messageId=fixture-1' });
    assert.equal(res.status, 403);
    assert.match(String(res.data.error), /工具未启用：qq_get_message_media/);
  });
});

// ── MCP 工具 ────────────────────────────────────────────────────────────────
async function mcpFixture(config, run) {
  const requests = [];
  const api = http.createServer((req, res) => {
    const url = new URL(req.url, 'http://fixture');
    requests.push({ url: req.url, token: req.headers['x-agent-token'] });
    res.writeHead(200, { 'content-type': 'application/json' });
    if (url.pathname === '/api/socialV2/message-media') {
      res.end(JSON.stringify({ ok: true, key: url.searchParams.get('key'), messageId: url.searchParams.get('messageId'), media: [MEDIA] }));
      return;
    }
    // 对照端点：unread 走 serializeModelData，media.url 会被压成 index 句柄。
    res.end(JSON.stringify({ ok: true, readThroughSeq: 21, messages: [{
      seq: 21, messageId: '-2026', sender: '小明', userId: '10001', text: '[图片]',
      media: [MEDIA], hasMedia: true,
    }] }));
  });
  await new Promise((resolve, reject) => {
    api.once('error', reject);
    api.listen(0, '127.0.0.1', resolve);
  });
  let fixture;
  let client;
  try {
    // 与 test-qq-model-view 相同的隔离方式：只复制源码与最小 config，
    // 不把生产配置、凭据或用户运行时状态带进测试夹具。
    fixture = await fs.mkdtemp(path.join(ROOT, '.tmp-message-media-test-'));
    await fs.mkdir(path.join(fixture, 'src'));
    await Promise.all(['mcp-snowluma-safe.js', 'qq-model-view.js', 'sensitive.js'].map((name) =>
      fs.copyFile(path.join(ROOT, 'src', name), path.join(fixture, 'src', name))));
    // mcp-snowluma-safe 顶部 import ./ext/send-media-mcp.js——ext 目录必须整体进夹具
    await fs.cp(path.join(ROOT, 'src', 'ext'), path.join(fixture, 'src', 'ext'), { recursive: true });
    await fs.writeFile(path.join(fixture, 'config.json'), JSON.stringify({ consolePort: api.address().port, ...config }));
    const transport = new StdioClientTransport({
      command: process.execPath,
      args: [path.join(fixture, 'src', 'mcp-snowluma-safe.js')], cwd: fixture, stderr: 'pipe'
    });
    client = new Client({ name: 'message-media-test', version: '1.0.0' });
    await client.connect(transport);
    await run({ client, requests });
  } finally {
    if (client) await client.close();
    api.closeAllConnections();
    await new Promise((resolve) => api.close(resolve));
    if (fixture) {
      const resolved = path.resolve(fixture);
      assert.equal(path.dirname(resolved), ROOT);
      assert.ok(path.basename(resolved).startsWith('.tmp-message-media-test-'));
      await fs.rm(resolved, { recursive: true, force: true });
    }
  }
}

test('MCP qq_get_message_media returns the url that the compacted message view hides', async () => {
  await mcpFixture({}, async ({ client, requests }) => {
    const listed = await client.listTools();
    const tool = listed.tools.find((t) => t.name === 'qq_get_message_media');
    assert.ok(tool, 'qq_get_message_media must be registered by default');
    assert.ok(tool.description.includes('url'), 'description must tell the model this is how it gets a URL');

    const common = { key: KEY, token: 'fixture-token' };
    const detail = await client.callTool({ name: 'qq_get_message_media', arguments: { ...common, messageId: '-2026' } });
    const text = detail.content.find((part) => part.type === 'text').text;
    const data = JSON.parse(text);
    assert.equal(data.media[0].url, MEDIA_URL, 'the tool response must carry the raw media url');
    assert.equal(data.messageId, '-2026');
    const forwarded = requests.at(-1);
    assert.equal(new URL(forwarded.url, 'http://fixture').pathname, '/api/socialV2/message-media');
    assert.equal(new URL(forwarded.url, 'http://fixture').searchParams.get('key'), KEY);
    assert.equal(new URL(forwarded.url, 'http://fixture').searchParams.get('messageId'), '-2026');
    assert.equal(forwarded.token, common.token, 'the session token must be forwarded');

    // 对照组：为什么需要这个工具 —— 消息列表视图会把 url 压成 {kind,index}。
    const unread = await client.callTool({ name: 'qq_get_unread_messages', arguments: common });
    const unreadText = unread.content.find((part) => part.type === 'text').text;
    assert.equal(unreadText.includes(MEDIA_URL), false, 'message view keeps hiding the url (compactModelMessage)');
    assert.deepEqual(JSON.parse(unreadText).messages[0].media, [{ kind: 'image', index: 1 }]);
  });
});

test('MCP qq_get_message_media is not registered when getMessageMedia is disabled', async () => {
  await mcpFixture({ socialV2: { tools: { getMessageMedia: false } } }, async ({ client }) => {
    const listed = await client.listTools();
    assert.equal(listed.tools.some((t) => t.name === 'qq_get_message_media'), false);
    // 关闭一个工具不该连带关掉别人。
    assert.ok(listed.tools.some((t) => t.name === 'qq_get_message_detail'));
  });
});
