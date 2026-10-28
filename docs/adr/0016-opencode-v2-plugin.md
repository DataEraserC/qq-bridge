# ADR-0016 opencode 插件只做 v2：写法换了，验证被上游 bug 挡住

来源：实现默认（选哪套 API、怎么注册工具、会话 id 存哪，都是我定的，可以推翻；
「v1 不支持」是实测结论，不是偏好）。

## 这条决定了什么行为

- opencode 适配器**只针对 v2**（`opencode2 2.x`）。v1 那套写法（`export const X = async
  ({client}) => ({ tool: {...} })`）在 v2 里**根本不会被加载**，所以不做双兼容。
- 插件入口是一个**普通对象默认导出**：`export default { id: "qqab", async setup(ctx) {…} }`。
  不 import `@opencode/plugin`（本地插件解析不到这个包，实测报
  `Cannot find package '@opencode/plugin'`）；`Plugin.define` 只是语法糖，不用它也能加载。
- 工具用 `ctx.tool.transform(editor => editor.add({ name, description, input, execute }))`
  注册，`input` 是 **JSON Schema**（v1 是 zod）。
- 一轮的动作顺序是 **`ctx.session.prompt` → `ctx.session.wait` → `turn_end`**。
  `prompt` 只保证把话排进会话，模型随后才跑；只等 `prompt` 就收工，票会提前作废，
  之后的工具调用全是 `unauthorized`。
- 会话 id 存在 `ctx.storage`（插件自己的持久 JSON）里，按设备一份；每次使用前
  `ctx.session.get` 确认还活着，被删了就重建。

## 不这样会怎样

- 留着 v1 写法：插件安安静静地不加载，日志里连「loading plugin」都没有（`.mjs` 不会被
  自动发现），排查起来像是「装错了目录」，实际是版本不对。
- import `@opencode/plugin`：加载直接失败。而失败信息是「找不到包」，很容易被误判成
  插件没装依赖，进而去装一个用不上的包。
- 只等 `prompt`：真跑起来会看到「模型明明调了 send_message，桥却说没有进行中的轮次」。
  这个坑假宿主测试踩过，症状和原因隔得很远。
- 会话 id 只记在内存：opencode 一重启就失忆，等于每次换一个新人格，和设备模型
  （记忆按设备一份）冲突。

## 技术细节

- 目录：`harnesses/opencode/plugin/`（`index.js` + `qqab-wire.mjs`）。整体拷到
  `~/.config/opencode/plugins/qqab/` 或 `<项目>/.opencode/plugins/qqab/` 即可，v2 会按目录加载。
  `.opencode/plugins/` 下直接放 `.mjs` 文件**不会**被发现，只认 `.js` / `.ts` 和插件目录。
- 配置键是 `plugins`（复数，v1 的 `plugin` 已废弃），本地路径必须是**目录**——
  指向单个文件会被跳过，只在日志里留一句 `configured plugin path must be a directory`。
- 插件里**不能 `console.log`**：`serve --stdio` 用 stdout 当 RPC 通道，写一行就把协议污染，
  表现是 opencode 报 `Transport: The socket connection was closed unexpectedly`。
  日志走 `ctx.app.log`，退路是 `console.error`。
- **验证现状**：本机 `opencode2 2.0.22`（Bun 1.4.2）加载任何插件都会段错误
  （`panic: Segmentation fault`，官方提示是 Bun 的 bug），4 行空插件同样崩。
  所以「真 opencode 里跑完一轮」这条还没验过。替代方案是 `test/plugin.test.mjs` 里的
  假宿主（照 v2 文档实现 `tool.transform` / `session` / `storage` / `app`），
  能验工具注册、一轮的完整动作、会话复用与重建、错误透传、卸载关连接。
- 等上游修好后，要补验两件：`prompt` 之后工具调用能否拿到票；`input` 的 JSON Schema
  它是否照单接受。

## 参考

- 适配器怎么拆、各家 harness 的判断：[`docs/06-harness-adapters.md`](../06-harness-adapters.md)
- 拆法与演化：[`ADR-0013`](0013-harness-adapter-shape.md)
