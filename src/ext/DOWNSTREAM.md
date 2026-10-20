# src/ext/ — 下游扩展目录（维护约定）

本目录是**纯下游新增**，上游 [Derpyu520/qq-bridge](https://github.com/Derpyu520/qq-bridge) 没有它。
目的：把我们自用的功能与上游文件**物理隔离**，每次 `git merge upstream/main` 时冲突面
收敛到官方文件里几处带标记的挂钩行，而不是散布在上游函数体中。

## 原则

1. **功能逻辑只写在 `src/ext/` 里**。每个功能域一个文件，模块纯函数优先、依赖全部经
   `deps` 注入（含 `fetch`），不触碰全局，可脱离桥接单测。
2. **官方文件只允许「加挂钩」，不允许改写上游函数**。每个挂钩带 `// ↓ down (ext)` 注释
   （少数一行式挂钩用 `// ↓ down:`），同步上游时按注释全文检索即可盘点全部接触面。
3. **每个功能一次 `feat(ext):` 提交**（中文长说明 + `测试：` 行 + Assisted-by 尾注），
   与上游提交历史交错但各自原子，出问题可整提交回滚，不改写历史。
4. **四份工具开关保持一致**（新增工具必做，回归测试 `test-send-media.mjs` / `test-send-chain.mjs` /
   `test-message-media.mjs` 断言）：
   - `src/bridge.js` loadConfig 的两个 tools 默认表；
   - `src/bridge.js` console config API 的 `toolFlags` 数组；
   - `public/console.html` 的开关面板行（+ console 名称映射 `toolMap`）;
   - `config.example.json` 的 `socialV2.tools`。
   再加：`src/mcp-snowluma-safe.js` 的 `TOOL_CONFIG_FLAGS` 与 preset 能力描述
   （`dsh/agent-presets/qq-chat-v2/agent.cordis.yml` → `node scripts/build-agent-preset-patches.mjs` 重新生成）。

## 挂钩清单（bridge.js / mcp-snowluma-safe.js / 测试夹具）

| 文件 | 挂钩 | 作用 |
| --- | --- | --- |
| `src/bridge.js` | 顶部 `import ... from './ext/send-media.js'` | 引入路由处理器（`send-chain.js` / `send-file.js` / `message-media.js` 同构，一行 import + 一行分发） |
| `src/bridge.js` | `function extDeps()`（**声明在 `startConsoleServer()` 内**，`v2SessionAllowed` 之后） | 依赖注入表：ext 用到的一切（cfg/guard/队列/闸门/state/fetch）都从这里拿。不能放模块顶层——`agentTokenOk` / `modeAllowed` / `captureSendGuard` 等闸门都在 main 的局部作用域，顶层看不见（跑测试会报 `xxx is not defined`）。只读域还注入存储层入口 `findMessageMedia`；`wsRequest` 注入网关 **WebSocket 动作通道**（HTTP 直连有 2MiB 请求体硬上限，upload_* 与大媒体段必须走它）；`mediaLimits` 注入发送侧媒体上限（复用接收侧 `MAX_MEDIA_BYTES`，两边同一份 `socialV2.media.maxBytes`）；`safeFetchBytes` 注入**不校验内容类型**的字节抓取（`safeFetchBuffer` 会以"不是有效图片"拒掉 zip/mp4，而 `qq_send_file` 与视频 URL 要的正是原始字节；SSRF/重定向/大小上限与前者同源） |
| `src/bridge.js` | `/api/socialV2/send-media` 分发行 | 一行转发给 `handleSendMediaRoute`（`/api/socialV2/send-chain` 一行转发给 `handleSendChainRoute`；`/api/socialV2/send-file` 一行转发给 `handleSendFileRoute`；只读的 `/api/socialV2/message-media` 一行转发给 `handleMessageMediaRoute`） |
| `src/bridge.js` | Host 校验之后、控制台令牌之前的 `/files` 分支行（`↓ down: file-host`） | 只读文件托管 `handleHostedFileRoute`——**刻意在令牌门禁之前**：能力路径（128-bit 随机）本身即门禁，要求 `?token=` 会把管理端全权令牌泄进 URL/日志；其余路由门禁不动 |
| `src/bridge.js` | tools 默认表 ×2 / toolFlags / toolMap | `sendMedia` / `sendChain` / `sendFile` / `getMessageMedia` 开关与控制台映射（**file-host 无开关面**：非 MCP 工具、纯只读端点） |
| `src/mcp-snowluma-safe.js` | `TOOL_CONFIG_FLAGS` 一行 + import + `registerSendMediaTool(...)` / `registerSendChainTool(...)` / `registerSendFileTool(...)` / `registerMessageMediaTool(...)` 各一行 | MCP 工具注册（file-host 不进 MCP） |
| `scripts/audit-bridge-harness.mjs` | import + context spread 一行 | VM 夹具里 `handleSendMediaRoute` / `handleSendChainRoute` / `handleSendFileRoute` / `handleMessageMediaRoute` / `handleHostedFileRoute` 可见 |
| `scripts/test-audit-security-mcp.mjs` | 顶部 import + `vm.runInNewContext` 上下文注入 | import 行被剥离后 ext 注册函数要作为上下文值（否则 `registerSendChainTool is not defined`） |

## 现有功能

- **`send-media.js`** — `qq_send_media`：图片（≤9）/ 图文 / 视频（≤1）统一发送。
  引用形态：本地绝对路径 / `file://`（桥接同机直读，推荐）/ http(s) URL
  （桥接内下载转 `base64://`）/ 纯 base64 / `data:` / `base64://`；
  单张图片字节上限跟随 `socialV2.media.maxBytes`（经 `extDeps().mediaLimits()` 注入，
  与接收侧同源，默认 8MB），视频 ≤16MB。
  发送前统一魔数校验，损坏字节就地拒绝（不再穿透成网关 opaque 的 921）。
  文案与所有 text 段过 `escapeCqText()`（`[CQ:` → `[CQ：`，与上游同一份规则）——
  否则模型写出 `[CQ:at,qq=all]` 就会被网关当 CQ 码解析成 @全体成员。
  与 sticker 同款 sendChain 串行 + 网关直连。
  端点 `POST /api/socialV2/send-media`；MCP 声明在 `send-media-mcp.js`。
  同时导出可复用底层：`escapeCqText` / `resolveMediaLimits` / `resolveMediaBase64` /
  `buildReplyAtSegments` / `postSegmentsV2`。
- **`send-chain.js`** — `qq_send_chain`：图文交错消息链（段序原样发出），
  段白名单 `text/image/video/face/at`，text 段**合计**过字数与敏感词闸门（防拆段绕过），
  每个 text 段发送前过 `escapeCqText()`。
  端点 `POST /api/socialV2/send-chain`；MCP 声明在 `send-chain-mcp.js`；
  发送底层全部复用 `send-media.js` 的导出。
- **`send-file.js`** — `qq_send_file`：**任意类型**文件上传（群文件/私聊文件），
  走 OneBot `upload_group_file` / `upload_private_file`，**经 extDeps 的 `wsRequest`
  （网关 WebSocket 主链路）发送**——HTTP 直连 2MiB 请求体上限装不下大文件，
  WS 帧上限 100MiB（64MiB 文件 ≈ 89MiB 帧）。引用形态复用 `classifyMediaRef`
  （本地绝对路径 / `file://` / http(s) URL 桥接内下载 / base64）；**不做内容魔数校验**
  （任意文件类型），只校验 base64 字母表与长度；文件名过敏感词闸门（消息里唯一可见文本）。
  瞬时传输错误重试一次（实测偶发 TCP connect timeout），OneBot 业务失败不重试。
  响应只有 `file_id`（协议无 message_id，不可引用），recentMessages 记 `[文件] name`。
  端点 `POST /api/socialV2/send-file`；MCP 声明在 `send-file-mcp.js`。
- **`message-media.js`** — `qq_get_message_media`：按需取回**原始媒体元数据**
  （url/file/faceId/caption…），补模型视图把 `media[].file/url` 压成 `{kind, index}` 句柄的洞；
  只回元数据不下载图片字节（那是 `qq_get_message_images` 的活）。只读域：不进
  RULES.md 发送白名单，但过同一套闸门链（key 格式 → agent token → `v2SessionAllowed` →
  `v2ToolEnabled`）。存储层入口 `findMessageMedia` 经 `extDeps()` 注入。
  端点 `GET /api/socialV2/message-media`；MCP 声明在 `message-media-mcp.js`，
  刻意裸 `JSON.stringify`、不用 `serializeModelData`（那个视图正是它要绕开的）。
- **`file-host.js`** — **只读文件托管**：给本机文件一个 `http(s)://<host>/files/<token>`
  能力 URL（`state/hosted/<32hex>/` 放一个文件即"上线"，删除目录即"下线"）。
  **没有上传端点**——投放是本机文件系统操作，天然绕开 HTTP 2MiB 请求体上限，也不存在
  写攻击面。门禁 = 128-bit 随机能力路径本身（挂点在 Host 校验之后、控制台令牌之前，
  刻意不要求 `?token=`——那会把管理端全权令牌泄进 URL/浏览器历史/日志）；其余路由的
  令牌门禁一字未动。响应头全套：nosniff + `CSP: sandbox` + `no-referrer` + `no-store`，
  html/svg 等可执行文档强制 `attachment`（防托管页面在控制台同源执行）；
  CORS 按能力语义放行（ACAO:*，知道 URL 即有权取用）。
  符号链接/隐藏文件不下发，多文件取字典序首个。不支持 Range（首版全量 200）。
  投放由本模块导出的 `publishHostedFile()` / `listHostedFiles()` / `removeHostedFile()`
  完成（纯文件系统操作，先写 `.part` 再 rename，同名重投即换内容），CLI 入口
  `node scripts/file-host.mjs publish|list|remove|url`——桥接进程仍**不暴露任何上传端点**。
  **非 MCP 工具、无四份开关面、不进 preset**；端点 `GET|HEAD /files/<32hex>`（其余 405）。

## 新增一个功能的最短路径（照抄 send-media 的接线）

1. `src/ext/<域>.js` 写 `handleXxxRoute(deps, io)` + 业务函数；
2. bridge.js：import 一行、`extDeps()` 里补一两个 getter/闸门、路由链里加一行分发；
3. 四份开关 + `TOOL_CONFIG_FLAGS` + console 名称映射 + config.example.json；
4. preset 能力行（agent.cordis.yml）→ `node scripts/build-agent-preset-patches.mjs` 重新生成；
5. `scripts/test-<域>.mjs`（纯函数单测 + 夹具 HTTP 闸门 + 开关一致性断言），
   在 `scripts/test-audit.mjs` 的 FILES 白名单里登记，按需加进 `package.json` 的 test 串联；
6. `feat(ext):` 一次提交。
