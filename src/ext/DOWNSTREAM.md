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
4. **四份工具开关保持一致**（新增工具必做，回归测试 `test-send-media.mjs` 断言）：
   - `src/bridge.js` loadConfig 的两个 tools 默认表；
   - `src/bridge.js` console config API 的 `toolFlags` 数组；
   - `public/console.html` 的开关面板行（+ console 名称映射 `toolMap`）;
   - `config.example.json` 的 `socialV2.tools`。
   再加：`src/mcp-snowluma-safe.js` 的 `TOOL_CONFIG_FLAGS` 与 preset 能力描述
   （`dsh/agent-presets/qq-chat-v2/agent.cordis.yml` → `node scripts/build-agent-preset-patches.mjs` 重新生成）。

## 挂钩清单（bridge.js / mcp-snowluma-safe.js / 测试夹具）

| 文件 | 挂钩 | 作用 |
| --- | --- | --- |
| `src/bridge.js` | 顶部 `import ... from './ext/send-media.js'` | 引入路由处理器 |
| `src/bridge.js` | `function extDeps()`（**声明在 `startConsoleServer()` 内**，`v2SessionAllowed` 之后） | 依赖注入表：ext 用到的一切（cfg/guard/队列/闸门/state/fetch）都从这里拿。不能放模块顶层——`agentTokenOk` / `modeAllowed` / `captureSendGuard` 等闸门都在 main 的局部作用域，顶层看不见（跑测试会报 `xxx is not defined`） |
| `src/bridge.js` | `/api/socialV2/send-media` 分发行 | 一行转发给 `handleSendMediaRoute` |
| `src/bridge.js` | tools 默认表 ×2 / toolFlags / toolMap | `sendMedia` 开关与控制台映射 |
| `src/mcp-snowluma-safe.js` | `TOOL_CONFIG_FLAGS` 一行 + import + `registerSendMediaTool(...)` 一行 | MCP 工具注册 |
| `scripts/audit-bridge-harness.mjs` | import + context spread 一行 | VM 夹具里 `handleSendMediaRoute` 可见 |

## 现有功能

- **`send-media.js`** — `qq_send_media`：图片（≤9）/ 图文 / 视频（≤1）统一发送，
  URL 桥接内下载转 `base64://`，与 sticker 同款 sendChain 串行 + 网关直连。
  端点 `POST /api/socialV2/send-media`；MCP 声明在 `send-media-mcp.js`。

## 新增一个功能的最短路径（照抄 send-media 的接线）

1. `src/ext/<域>.js` 写 `handleXxxRoute(deps, io)` + 业务函数；
2. bridge.js：import 一行、`extDeps()` 里补一两个 getter/闸门、路由链里加一行分发；
3. 四份开关 + `TOOL_CONFIG_FLAGS` + console 名称映射 + config.example.json；
4. preset 能力行（agent.cordis.yml）→ `node scripts/build-agent-preset-patches.mjs` 重新生成；
5. `scripts/test-<域>.mjs`（纯函数单测 + 夹具 HTTP 闸门 + 开关一致性断言），
   在 `scripts/test-audit.mjs` 的 FILES 白名单里登记，按需加进 `package.json` 的 test 串联；
6. `feat(ext):` 一次提交。
