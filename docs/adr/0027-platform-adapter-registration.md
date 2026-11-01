# ADR-0027 平台适配器在 core 外：注册表 + 事件/动作队列 + 唤醒提示

来源：用户裁定（2026-11-01：平台适配器、harness 适配器在 core 外；平台适配器实现平台接口，注册到 core；core 用它拉事件、入库、唤醒；平台独有事件唤醒不应污染 core）。技术细节为实现默认。

## 这条决定了什么行为

- **PlatformAdapter 在 core 外**：实现特定聊天平台的连接、解码、出站、方法目录和平台调用。
- **AdapterRegistry** 按 `kind` 注册工厂；core 按连接配置实例化适配器。适配器 manifest 声明：id、platform、kind、transport、能力、目录版本、options schema。
- **适配器拥有 I/O，core 拥有账本和调度**：

  ```text
  适配器线程（socket/WS）
     ├─ 归一化事件 ──▶ EventQueue ──▶ core 状态线程（ingest / 未读 / 唤醒）
     └─ ActionQueue ◀── 发送 / 平台调用命令 ── core
  ```

  适配器不直接调 `Bridge::ingest`、不写 store、不决定唤醒、不改水位；core 从事件队列拉取/消费，完成入库和唤醒。

- **唤醒不写死平台语义**：适配器把平台事件归一化成 `InboundEvent`，附带通用 `WakeHints`（`mention_me`、`reply_to_me`、`poked_me`、`mention_all`、`tags`）和原始 `payload`。内置唤醒策略只匹配通用 hints/tags；平台独有策略是策略插件，可以读 `platform` + `payload`。core 不认识 `poke`、`group_increase` 等平台事件名。
- **harness 适配器天然在 core 外**：bridge 只提供协议和 manifest/registry（harness id、token、协议版本、能力），不把 harness 代码加载进 bridge 进程。bridge 自己若要提供驱动，走外置进程 Provider。

## 不这样会怎样

- 适配器直接调 `ingest` / 持有全局锁：平台慢响应会卡住整座桥，平台怪癖会渗进 core。
- core 解析平台事件名：每接一个平台，core 的 match 分支就多一层；唤醒策略无法独立替换。
- 适配器直接写 store：流水、水位、ack 的一致性失去唯一写入者。
- harness 适配器塞进 bridge 进程：语言/ABI/崩溃域都被绑死，与“多 harness”目标相反。

## 技术细节

### 适配器契约（Phase 2 落地）

```text
PlatformAdapter:
  manifest() -> AdapterManifest
  connect(host: AdapterHost)
  send(command) -> Sent
  call(method, params) -> Value
  shutdown()

AdapterHost:
  event_sink: bounded queue
  action_source: bounded queue
  log / clock / cancel
```

- 每条连接一个适配器实例；core 不持有适配器实例，只持有 registry 和动作队列句柄。
- 事件队列有界；满了要背压或断开，不能无声丢。
- 动作回执用 correlation id + 结果队列；core 不在锁里等平台回话。
- `MethodSpec` 必须 owned（`Arc<[MethodSpec]>` / `Cow<'static, str>`），动态适配器才能构造目录。
- `MethodSpec` 必须带执行契约：`targets`（哪些参数是房间目标、哪些是成员目标）、`effect`（read/write/send/delete）、`record`（成功后是否写流水/推水位）。core 在不知道方法名的前提下靠这些字段守住边界。

### InboundEvent / WakeHints

```text
InboundEvent:
  connection, room, at,
  kind: message | event,
  actor, target,
  elements,
  platform,
  hints: { mention_me, reply_to_me, poked_me, mention_all, tags },
  raw: Option<Value>
```

- hints 由适配器计算，core 只传递；策略插件决定是否唤醒。
- 平台独有事件放在 `tags` / `raw` 里；core 不解析。
- `reply_to_self` 这类需要查库的判定仍由 core 做（表里有平台 id）。

### 迁移顺序

1. 先加 `PlatformAdapter` trait + `AdapterRegistry` + `EventQueue` / `ActionQueue`，`onebot11` 作为第一个注册实现；core 不再在锁里调 outbound。
2. 把唤醒判定从 `WakePolicy::decide` 改为 wake provider + `WakeContext`；适配器填 hints/tags，先保持默认策略行为不变。
3. 补 `MethodSpec` 的 target/effect/record 元数据，修掉 `platform_call` 跨房间和发送不记账。
4. 再考虑动态适配器（外置进程 / 动态加载）。

### 与现有 ADR 的关系

- 不推翻 ADR-0012 的“一条连接一个账号/一个适配器实例”，只是把实例化从 `main.rs` 硬编码改成注册表。
- 不推翻 ADR-0020 的“两个通用工具”，但把目录从展示信息升级为执行契约。
- 不推翻 ADR-0017/0019 的会话模型；平台适配器不知道订阅，core 负责权限。

## 实现记录（2026-11-01）

- **Phase 2a 已落地**：`call_payload` 现在拒绝在 `params` 里换 `group_id`/`user_id`；群目标只能等于当前房间，要操作别的房间必须用 `room` 参数。
- 顺手修掉两个目标参数混用问题：
  - `poke.send` 在群里 `user_id` 是“拍谁”（成员目标），`group_id` 才是房间目标；私聊里 `user_id` 由房间推导。之前 `call_payload` 对“同时有 group/private 目标”的路由处理是错的。
  - `message.send_forward` 现在按房间推导 `group_id`/`user_id`，并拒绝在群里指定 `user_id` 把目标换成私聊。
- **Phase 2b 已落地（target 部分）**：`MethodSpec` 增加 `targets: &[TargetSpec]`；`TargetSpec` 区分 `Room` / `Member`，并声明房间路径类型。kernel 的 `platform_call` 在 `check_params` 之后调用 `check_target_params`，即使适配器自己没检查，core 也会拒绝在 `params` 里换房间目标。OneBot 的 targets 由 `ROUTES` 推导，避免方法表和路由表各抄一份。
- **Phase 2c-1 已落地**：`AdapterManifest` + `AdapterFactory` + `AdapterRegistry`；`main.rs` 不再知道 `OneBotOutbound` / `LinkSlot` / `supervise` 怎么拼，只注册 `OneBotAdapterFactory`，按连接 `kind` 取 `AdapterInstance`。连接线程仍由适配器自己的 `launch` 启动，core 不持有 socket。
- **Phase 2c-2 已落地**：`Outbounds` 提供可克隆的 `OutboundHandle`；endpoint 把 `send_message` / `send_message_to` / `platform_call` 拆成“锁内 prepare / 锁外 execute / 锁内 finish”。入站事件走 `EventQueue`，适配器只提交 `RecordDraft`，core 的 `drain_events` 线程统一 `ingest`；适配器侧不再 `try_lock`。
- **Phase 2c-3 已落地**：`MethodSpec` 增加 `effect`（read/write/send/delete）与 `record`（none/message/event）；OneBot 的 effect/record 由风险、标签和方法名推导，不维护第二份手写表。
- **`Outbound::call` 返回 `CallOutcome`**：适配器把发送类方法（发消息、合并转发、群公告）**实际发出去的内容**翻译成 `OutgoingRecord`，core 不认识平台消息段。`finish_platform_call` 成功后写流水并对**所有订阅者** ack，修掉「用 `platform_call` 发的消息在自己水位上留洞、导致积压反复重投」的问题。
- **Phase 3（2026-11-01，动态适配器 P1）**：先把宿主边界和 Root 接好，不改变收发行为。
  - `MethodSpec` / `ParamSpec` / `TargetSpec` 改为 `Cow` owned 字段，`Outbound::catalog` 返回 `Arc<[MethodSpec]>`；OneBot 目录首次访问时构造并缓存，动态适配器可以自己构造目录。
  - `AdapterFactory` / `AdapterInstance` 去掉 `Store` 泛型；`launch` 只拿 `AdapterHost`（入站队列 + 退出信号），不再拿 `Bridge`。OneBot 的账号核对改用连接配置快照，适配器侧不再读 core 状态。
  - 适配器登记进 `PluginManager`：`ExtensionPoint::PlatformAdapter`、`ProviderDescriptor.kind`、`register_adapter` / `create_adapter`；删除独立的 `AdapterRegistry`。内置 `onebot11` 和未来的动态/外置适配器走同一套 manifest、版本和能力审计。
  - 验证：`cargo test --workspace` 全绿，`nix build .#acb-bridge` 通过；重启后 1 条连接、2 个 harness 正常连上。
- **Phase 3 P2（2026-11-01，外置进程）**：适配器实现可以放在桥进程外。
  - 新增 `acb-adapter-api`：core 与适配器共用的 JSON wire（`hello` / `event` / `send` / `call` / `reply` / `shutdown`），方法目录、错误分类都有稳定的 wire 形状。
  - 新增 `acb-adapter-process`（`kind = "process"`，`transport = "process"`）：按 `[connections.options]` 的 `command` / `args` / `env` / `cwd` / `startup_timeout_secs` 启动子进程；第一行必须是 `hello`（带平台方法目录），之后事件进 `EventQueue`、动作按 correlation id 回执。
  - 子进程崩溃或退出后，未完成的请求明确返回「通道不可用」，不会让 core 无限挂着；退出时发 `shutdown`，2 秒内不收尾就 kill。
  - 连接配置的 `url` 改为可选，`kind` / `transport` 不再由 config 写死；是否支持某个 transport 由适配器 manifest 在启动时决定。
  - conformance 测试真启动一个 fixture 子进程，覆盖 hello、入站事件、发送回执、调用回执、错误分类和缺 `command` 的启动失败。
- **Phase 3 P3（2026-11-01，动态库）**：适配器实现可以编成 `.so` / `.dylib`，在桥进程内运行。
  - `acb-adapter-api` 增加 C ABI：只导出一个 `acb_adapter_entry_v1`，返回 `AdapterV1` 函数表（manifest / create / invoke / destroy / free_string），core 通过 `HostV1` 回调把事件、日志和退出信号交给插件。跨 ABI 的文本全是 JSON，和外置进程共用同一套 wire。
  - 新增 `acb-adapter-dynamic`（`DynamicLoader`）：按 `[[plugins]]` 的 `path` / `kind` 用 libloading 装载，插件自己报 manifest，注册进 Root 后与内置/进程适配器完全同构。
  - 动态库在桥进程内，插件 panic 或挂死会带倒整座桥；需要崩溃隔离时用 `process`。P3 不做调用超时（in-process 无法打断），这是明确取舍。
  - conformance 测试真的用 libloading 打开一个 fixture cdylib，覆盖 manifest、describe 目录、入站事件、发送/调用回执、错误分类和 shutdown。
- **Phase 3 P4（2026-11-01，策略扩展点）**：wake provider 也能走同一套动态库 wire。
  - `WireManifest` 增加 `extension`（默认 `platform_adapter`，P4 支持 `wake`）；动态库 `create` 的参数改为带标签的 `WireInstanceConfig`（`adapter` / `provider`）。
  - 新增 `CoreMessage::Decide` / `WireResult::Decision` / `WireWakeRequest`：wake 插件按 `WireWakeRequest` 判定，返回 `wake` / `stay`；解析失败或插件报错一律按 `stay` 兜底。
  - `DynamicLoader` 按 manifest 的 extension 分流：适配器注册进适配器表，wake 插件注册成 `ManagerPlugin` 子树（provider id 来自 manifest.id），provider 实例缓存在 manager 里。
  - 配置表 `[[adapters]]` 更名为 `[[plugins]]`，同一份清单可以装适配器和策略插件。wake 只支持动态库载体：策略判定在 core 的 ingest 路径里，外置进程会在 core 锁内做 I/O，违反 ADR-0027 的队列原则。
  - conformance 测试用 wake fixture cdylib 真加载并验证 `at_self` / 关键词判定。
- **尚未做**：access 等其它策略扩展点的动态库载体。AccessPolicy provider 已在 ADR-0026 Phase 2d 落地。

## 参考

- 扩展宿主：[ADR-0026](0026-plugin-host.md)
- 连接模型：[ADR-0012](0012-platform-connections.md)
- 平台能力：[ADR-0020](0020-platform-capabilities.md)
- 事件模型：[ADR-0005](0005-platform-events.md)
