# ADR-0026 扩展宿主：RootManager 固定，LoaderPlugin 与 ManagerPlugin 同时存在

来源：用户裁定（2026-11-01：可替换策略应覆盖 core 内外的扩展点；native 内嵌 / 动态加载 / 其他形式都应能注册；任何人可以创建树外 manager）。技术细节为实现默认。

## 这条决定了什么行为

- **RootManager 是 core 的一部分，不可替换。** 它负责全局 provider id / namespace、API 版本、能力授予、启动策略、热重载边界、审计、子树隔离。
- **两个扩展点同时存在**：
  - **LoaderPlugin**：一种载体的实例化机制（native 内嵌、`.so`/`.dylib`、外置进程、Lua/WASM 脚本）。
  - **ManagerPlugin**：一整棵子树的管理者，拥有自己的 namespace、注册表、配置 schema、生命周期和健康检查。
- **Provider** 是最终实现（平台适配器、wake/receive/send/clock、delivery composer、turn-error……）。
- 组合形状是混合的：

  ```text
  RootManager（core，固定）
   ├─ NativeLoader        -> 内置 Provider
   ├─ ProcessLoader       -> 外置进程 Provider
   └─ LoaderPlugin
        └─ ManagerPlugin（树外，拥有自己的子树注册表）
             ├─ 子树 Loader
             └─ Provider
  ```

  这同时允许“PluginManager + PluginLoader”和“PluginManager + PluginManager”，区别只在注册表/生命周期的所有权落在哪一层。

## 不这样会怎样

- 如果 RootManager 也可替换，就没有固定地方执行“provider id 全局唯一、能力不能越权、插件不能改水位/票/配置”这些全局约束；插件系统自己会变成新的安全问题。
- 如果只有 Loader，没有 Manager，第三方只能增加一种插件格式，不能拥有自己的一整棵子树和一套管理制度。
- 如果只有 Manager，没有 Loader，每一种插件格式都要重新实现一套加载机制，native 内置实现反而没有统一入口。
- 如果内置实现不走注册表，而是代码里的 `if config { ... }`，所谓“可替换”就只是几个配置开关，无法做到“内嵌了也能用动态加载”。

## 技术细节

### Provider id 与命名空间

- 写法 `namespace.name`：第一个点前是 namespace，后面是 name（name 自身可以再含点）。
- 内置 provider 用保留 namespace `acb`（如 `acb.wake.default`、`acb.adapter.onebot11`）。
- 树外 provider / manager 使用自己的 namespace，不能占用 `acb`。
- Root 先查直接注册的 Provider；没有则按 namespace 找到 `ManagerPlugin`，委托它解析。
- 允许子 manager 再建 namespace，但有**深度上限**和**环路检测**；`A -> B -> A` 直接拒绝。

### 契约

```text
ManagerPlugin:
  manifest() -> { namespace, api_version, capabilities, options_schema }
  descriptors() -> [ProviderDescriptor]
  instantiate(id, options) -> ProviderInstance
  reload(scope)
  shutdown()
  health()

LoaderPlugin:
  manifest() -> { kind, api_version, extensions }
  install(manager)
  unload(provider)
```

- `ProviderDescriptor` 至少包含：`id`、`extension_point`、`api_version`、`capabilities`。
- 每种扩展点有自己的 Provider trait 和 conformance suite；没有测试的扩展点不开放给第三方。
- 当前已落地的扩展点是 wake 与 access；其余按 phase 逐步加。

### Root 保留的硬约束

- 只有 Root 能签/验票、推水位、写流水、串行轮次、执行保留策略。
- 只有 Root 能决定启动是 `strict` 还是 `degraded`；指定 provider 加载失败不能悄悄换成默认实现。
- 能力按扩展点授予：wake 不能拿 `send`，delivery composer 不能拿 `ack`，任何插件不能拿 `Store`/`Bridge`/配置写入。
- 插件调用有超时；外置进程 / manager 失败可隔离、可 quarantine，不能卡死 core。
- 热重载只发生在轮次边界；正在跑的轮次持有旧 provider / 配置快照。

### Bootstrap 规则

ManagerPlugin 本身也要被某个 Loader 实例化，但**不能由它自己管理的格式来加载**：

- LuaManager 必须是 native/dynamic/process，不能是一个 `.lua` 文件；否则没有 LuaLoader 就没法加载 LuaManager。
- DynamicManager 必须是 native/process，不能是一个由自己管理的 `.so`。
- 这是插件系统的“先有鸡还是先有蛋”约束，不是实现细节。

### 配置

- 配置写 `provider = "namespace.name"` + `options`；内置实现也走同一张注册表。
- provider 的 `options_schema` 由 manifest 提供；控制台表单应从 schema 生成，而不是继续用固定字段 patch。
- 未知 provider 在启动时拒绝（或按 `degraded` 明确降级），不能等到第一轮才报错。

## 落地阶段

1. **Phase 1（本 ADR 的首个实现）**：RootManager + NativeLoader + 内置 wake provider；配置支持 `wake.provider`，引擎走 provider 解析；行为与默认策略一致。
2. Phase 2：PlatformAdapter 注册表 + 事件/动作队列（见 ADR-0027）。
3. Phase 3：ManagerPlugin / LoaderPlugin 契约落地，先做一个受信任的树外 manager，再做 LuaManager + LuaLoader。
4. Phase 4：DeliveryComposer + pre/post hooks；最后才是“换投递引擎”这类顶层插件。

## 实现记录

- **Phase 1**：RootManager + NativeLoader + 内置 wake provider；`wake.provider` 可替换，默认行为不变。
- **Phase 2d（2026-11-01）**：AccessPolicy 扩展点落地。
  - 会话配置新增 `[sessions.access]`：`provider` + `rooms`；不写就是 `acb.access.subscribed`，行为与原来写死的「只能碰自己订阅的房间」完全一致。
  - 内置三个 provider：`acb.access.subscribed`（默认）、`acb.access.all_rooms`（整个会话豁免）、`acb.access.elevated_in_rooms`（`rooms` 里列出的房间不用订阅也能读写）。
  - **策略只回答 allow/deny**；参数、目标、破坏性、记账仍由 core 统一执行。破坏性操作继续由连接的 `allow_destructive` 单独把关，两个维度不互相顶替。
  - 读取/发送/平台调用都走同一个 `AccessRequest`（会话、连接、房间、动作、会话自己的订阅）；连接级请求 `room = None`。
  - 未知 access provider 在启动时拒绝；树外 ManagerPlugin 可以注册 access provider，返回错类型会被 Root 拒绝。
- **Phase 3a（2026-11-01）**：ManagerPlugin / PluginLoader 契约补齐，先做受信任的树外 manager。
  - manager 增加 `manifest()`（namespace、api_version、capabilities、options_schema）；Root 安装时校验 API 版本、能力并集、descriptor 的 namespace 和所需能力。
  - manager 增加 `children()` / `reload()` / `shutdown()` / `health()`；Root 递归安装子树，最多 4 层并拒绝环路；生命周期由 Root 驱动（shutdown 按安装逆序，子 manager 先收）。
  - loader 增加 `manifest()`（kind、api_version、extensions）与 `unload()`（默认不支持，动态 loader 再覆盖）；Root 加载前校验版本和扩展点。
  - 新增独立 crate `acb-plugin-trusted`（namespace `trusted`），提供 `trusted.accumulate_only`：只在未读攒够时唤醒，`@`/昵称/拍/引用都不单独叫醒。`TrustedLoader` 把它装进 Root，`main` 启动时安装；配置写 `provider = "trusted.accumulate_only"` 即可启用。
- **Phase 3b（2026-11-01）**：LuaManager / LuaLoader 落地。
  - 新增独立 crate `acb-plugin-lua`，用 `mlua`（Lua 5.4 编进二进制）实现一个 native manager：脚本目录里的 `*.lua` 一一对应 `lua.<文件名>` provider。
  - 脚本返回 `{ extension = "wake"|"access", decide = function(options, context) ... end }`；wake 返回 `"wake"/"stay"`，access 返回 `"allow"/"deny"`（也接受布尔）。
  - 脚本只能看到 options/context 的普通 table，拿不到 Store / Bridge / 配置写入；标准库只开 `ALL_SAFE`。
  - 每次调用有 50ms 超时（指令钩子打断）；超时或报错按安全侧兜底（wake 不醒、access 拒绝），并累计失败数让 manager health 变 Degraded。
  - `reload` 只换实现，不允许脚本增删 provider（descriptor 集合变化要求重启）。目录来自环境变量 `ACB_LUA_DIR`，没设就不装 LuaLoader。
  - 这满足 bootstrap 约束：LuaManager 是 native 实现，不是它自己加载的 `.lua` 文件。
- **Phase 4（2026-11-01）**：投递 composer + 发送前后钩子。
  - 会话新增 `[sessions.delivery] provider = ...`，默认 `acb.delivery.default`（原来的「最旧一小段 + 最新一批」逻辑搬进 provider）。composer 只返回编号集合和 `omitted`；未知编号被 core 忽略，票/水位/ack 仍由 Root 算；解析失败回退内置 composer，保证一轮不会空手开。
  - 会话新增 `[sessions.outbound] before = [...] / after = [...]`：`before` 按顺序改写正文（可加签名、脱敏），`after` 只观察回执。`before` 失败则本次发送失败；`after` 不影响已发出去的消息。钩子只拿到 session / room / origin，不拿 Store、票或水位。
  - NativeLoader 注册内置 `acb.delivery.default`；Root 支持 Delivery / Send 两个扩展点，未知 provider 启动时拒绝。
  - 失败通知也走发送钩子（origin = turn_failure）；`platform_call` 的发送类方法不经过 send hooks——那类消息由适配器直接产出 `OutgoingRecord`，core 不认识平台消息段。
  - native composer / hook 是受信任的同步调用，本轮不加超时；树外或动态载体接入这些扩展点时再补超时与隔离。
- **Phase 4b（2026-11-01）**：LuaManager 支持 delivery / send 扩展点。
  - `extension = "delivery"`：`compose(options, request)`，`request.records` 是普通 table（含 id / at / sender / kind / text / elements 等），返回 `{ keep = {编号...}, omitted = {from,to,count} }`；返回 nil 或报错时回退内置 composer。
  - `extension = "send"`：`before(context, elements)` 可改写元素数组并返回，`after(context, sent)` 只观察回执。元素九种类型完整往返，opaque 载荷用 `payload_json` 字符串。
  - 继承 Lua 的 50ms 超时；send.before 报错则这次发送失败，send.after 报错只累计 health（Degraded）。
- **Phase 5a（2026-11-01）**：turn-error provider 化。
  - 会话新增 `[sessions.turn_error] provider = ...`，默认 `acb.turn_error.default`；默认实现保持原行为：会话 `on_turn_error` 非空就用它，否则回退全局 `default_on_turn_error`。
  - provider 只返回动作列表（log / send:房间）；文本组装、发送和锁外执行仍归 core。未知 provider 启动时拒绝。
  - Lua 增加 `extension = "turn_error"`：`decide(request)` 返回 `{ {kind="log"}, {kind="send", room="qq:group/ops"} }`；报错回退默认选择，并累计 health。
- **Phase 5b（2026-11-01）**：clock provider 化（ADR-0011 落地）。
  - 订阅新增 `[sessions.subscriptions.clock]`：`provider` + `interval_secs` / `idle_secs`；默认 `acb.clock.off`，写了阈值但没写 provider 时自动用 `acb.clock.interval_idle`。
  - 内置 `acb.clock.interval_idle`：固定间隔或房间空闲任一命中就开一轮；`TurnReason::Clock`，投递可以为空。
  - `main` 每秒调一次 `Bridge::tick_clock`；`last_clock_at` 只存在内存里，重启后重新计间隔。会话正在跑或已有排队时不叠轮次。
  - Lua 增加 `extension = "clock"`：`is_due(options, context)` 返回布尔（也接受 due/skip）；报错按「不触发」兜底。
- **Phase 5c（2026-11-01）**：receive provider 化。
  - 连接新增 `[connections.receive] provider = ...`，默认 `acb.receive.all`；入站策略只回答 `record` / `drop`。
  - 被丢掉的记录不写流水、不叫醒任何人，也不会让水位出现空洞；provider 拿不到 Store / 票 / 配置写入。
  - Lua 增加 `extension = "receive"`：`decide(request)` 返回 record/drop 或布尔；报错按「接收」兜底，避免误丢消息。

- **Phase 6（2026-11-01）**：插件目录与描述（让 agent 能“察觉”策略）。
  - 新增 `PluginMetadata`（title / summary / description / tags / effects / docs_url / options_schema），接入 `ProviderDescriptor`、`ManagerManifest`、`AdapterManifest` 和 `WireManifest`；`effects` 是文档，不是强制保证。
  - Root 新增 `catalog()` / `catalog_json()` / `info()`：合并 manager 说明、标注 `source`（builtin / trusted / lua / dynamic）和 manager health；内置 provider、OneBot、process、动态库 fixture 都带描述。
  - agent 新增只读工具 `describe_plugins`：默认返回本会话实际生效的 access / delivery / turn_error / receive / send_hooks / 订阅 wake/clock / 当前适配器；`scope=global` 看整个目录，可按 extension 过滤。
  - 控制台新增 `GET /api/plugins`；只给 options schema，不给实际 options、路径或 token。描述可能不可信（插件作者写的），按数据处理、限长、转义。
  - wake provider 新增 `decide_with_reason`：内置/trusted/动态库实现返回命中原因（@、引用、名字、关键词、攒条、拍），经 `Delivery.wake_reason` 下发；协议版本升到 5，harness 把原因渲染进轮次开头（如「有新消息（@ 了我）」）。
  - P5d：`PluginSource` 非 `Builtin` 的适配器和 manager provider 必须声明 `metadata.summary`；动态库 wake 插件还必须声明 `effects`，否则启动失败。描述从“建议”变成第三方插件准入门槛。

## 参考

- 平台适配器注册与队列：[ADR-0027](0027-platform-adapter-registration.md)
- 订阅策略：[ADR-0002](0002-subscription-model.md)
- 配置管理：[ADR-0009](0009-config-management.md)
- 状态与票：[ADR-0014](0014-state-on-disk.md)、[ADR-0010](0010-ack-watermark.md)
