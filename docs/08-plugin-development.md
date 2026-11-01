# 插件开发：把适配器和策略写到 core 外

这一页写给**插件开发者**：你不需要改 bridge 源码，只要按契约写一个 Lua 脚本、外置进程
或动态库，就能把平台接进来，或者替换唤醒、访问、投递、失败通知等策略。

如果你要改 bridge 本身（core、协议、存储、内置 harness 适配器），看
[架构](03-architecture.md)、[协议](04-protocol.md) 和
[harness 适配器](06-harness-adapters.md)，不要从这一页开始。

## 先选载体

| 载体 | 能做什么 | 优点 | 代价 |
|------|----------|------|------|
| Lua（`ACB_LUA_DIR`） | 七种策略扩展点 | 不用编译，改完 reload 即生效 | 每次调用 50ms 上限；拿不到 Store/流水；不能写平台适配器 |
| 外置进程（`kind = "process"`） | 平台适配器 | 任何语言；崩溃隔离；可自动重启 | 只能做适配器；策略判定在 core 锁内，不开放进程载体 |
| 动态库（`[[plugins]]`） | 平台适配器 + 七种策略 | 进程内、全扩展点、C ABI + JSON wire | 与桥同进程，panic/卡死会带倒桥；不支持 unload |
| 内置 native | core 开发者使用 | 无跨进程开销 | 要改仓库代码并重新构建 |

## 扩展点一览

| `extension` | 作用 | Lua | 动态库 | 外置进程 |
|-------------|------|-----|--------|----------|
| `platform_adapter` | 连接平台、发事件、发送/调用 | 否 | 是 | 是 |
| `wake` | 一条记录要不要叫醒会话 | 是 | 是 | 否 |
| `access` | 工具能读/写/碰哪些房间 | 是 | 是 | 否 |
| `delivery` | 一轮投哪些记录编号 | 是 | 是 | 否 |
| `send` | 发送前改写正文、发送后观察回执 | 是 | 是 | 否 |
| `turn_error` | 一轮失败后执行哪些通知动作 | 是 | 是 | 否 |
| `clock` | 现在要不要开例行检查 | 是 | 是 | 否 |
| `receive` | 一条入站记录收不收 | 是 | 是 | 否 |

策略的插件专属参数统一放在配置的 `.options` 自由表里。Lua 和动态库都会收到
`options.extra`；`send` 没有 options；`turn_error` 的 Lua 写法是
`decide(request, options)`，旧的单参数 `decide(request)` 仍然兼容。

## 写一个动态库插件

动态库只导出 `acb_adapter_entry_v1`，返回一张 C 函数表：

```rust
#[unsafe(no_mangle)]
pub extern "C" fn acb_adapter_entry_v1() -> *const AdapterV1 {
    &TABLE
}
```

函数表必须包含 `manifest` / `create` / `invoke` / `destroy` / `free_string`。
所有跨 ABI 的文本都是 JSON，形状定义在 `crates/adapter-api`：

```text
core ──▶ 插件   create(WireInstanceConfig) / invoke(CoreMessage) / Shutdown
插件 ──▶ core   manifest() / invoke 返回 AdapterMessage / HostV1.submit_event
```

- `manifest` 返回 `WireManifest`：`id`、`api_version`、`extension`、`capabilities`，
  以及平台适配器需要的 `platform` / `kind` / `transports` / `events` / `event_tags` /
  `supports_at` / `supports_reply`。
- `create` 收到 `WireInstanceConfig`：适配器带连接配置，策略 provider 带扩展点和 options。
- `invoke` 收到 `CoreMessage::Decide`（策略）或 `Describe` / `Send` / `Call`（适配器），
  返回 `AdapterMessage::Reply`。
- `destroy` 释放 handle；桥退出前会先发 `Shutdown`。

第三方动态库插件会被启动审计：

- `id` 用 `namespace.name`，`acb` 是内置保留 namespace。
- `api_version` 必须等于 `PLUGIN_API_VERSION`（当前 1），C 表 `abi_version` 必须等于
  `ABI_VERSION`（当前 1）。
- `metadata.summary` 必填；`wake` 还必须声明 `effects`（至少 `may_wake` /
  `may_stay_silent`）。`options_schema` 目前只是说明，不强制校验。
- 平台适配器的 `kind` 必须和 `[[plugins]] kind` 一致（如果配置里写了）。

最小可构建示例见 [`examples/dynamic/wake-provider`](../examples/dynamic/wake-provider)；
更完整的 conformance fixture 见 `crates/adapters/dynamic-fixture` 和
`crates/plugins/wake-fixture`。

## 写一个外置进程适配器

桥按连接配置启动子进程，双方在 stdin/stdout 上一行一条 JSON：

```toml
[[connections]]
id = "demo"
platform = "demo"
kind = "process"
transport = "process"
account = "bot"

[connections.options]
command = ["/path/to/adapter", "--stdio"]
# 可选：args / env / cwd / startup_timeout_secs
```

子进程的第一行必须是 `hello`：

```json
{
  "type": "hello",
  "name": "demo",
  "catalog": [],
  "info": {
    "events": ["demo.greet"],
    "event_tags": ["notice"],
    "supports_at": true,
    "supports_reply": true
  }
}
```

之后：

- 入站记录发 `{"type":"event","record":{...}}`。
- `send` / `call` 按 `id` 回 `reply`。错误用 `kind` 分类（`bad_params` / `forbidden` /
  `unavailable` …）。
- 子进程退出后自动重启，退避从 500ms 起翻倍、最多 10 秒；重启期间请求明确失败。
- 桥退出时发 `shutdown`，2 秒内不收尾就 kill。

参考实现：[`examples/process/adapter.mjs`](../examples/process/adapter.mjs)。

## 写一个 Lua 策略

文件名就是 provider 名：`recent_only.lua` → `lua.recent_only`。脚本返回一个表，
至少包含 `extension` 和该扩展点要求的函数：

```lua
return {
  extension = "delivery",
  compose = function(options, request)
    return { keep = { request.records[#request.records].id } }
  end,
}
```

各扩展点的函数名：

| 扩展点 | 函数 |
|--------|------|
| wake / access | `decide(options, context)` / `decide(_, request)` |
| delivery | `compose(options, request)` |
| send | `before(context, elements)` / `after(context, sent)` 至少一个 |
| turn_error | `decide(request, options)` |
| clock | `is_due(options, context)` |
| receive | `decide(request, options)` |

`WakeContext` 和 `ClockContext` 都带 `last_wake_at`（这个会话×房间上一次因任何原因
开轮的时间）。做「多久没醒就醒一次」时，wake 端只能覆盖「有入站记录时」的判定；
没人说话也要到点开轮，必须再配一个 clock provider。参考
[`examples/lua/wake_policy.lua`](../examples/lua/wake_policy.lua) 和
[`examples/lua/three_hour_clock.lua`](../examples/lua/three_hour_clock.lua)。

每次调用最多 50ms，超时按安全侧兜底；脚本拿不到 Store / Bridge / 配置写入。
可抄的示例在 [`examples/lua`](../examples/lua)，总览见
[`examples/README.md`](../examples/README.md)。

## 配置与装配

```toml
# 动态库策略
[[plugins]]
path = "/绝对路径/到/libacb_example_wake.so"

[sessions.subscriptions.wake]
provider = "example.wake"

[sessions.subscriptions.wake.options]
keyword = "在吗"

# 动态库平台适配器
[[plugins]]
path = "/绝对路径/到/libdemo_adapter.so"
kind = "demo"

# Lua 策略
[sessions.delivery]
provider = "lua.recent_only"

[sessions.delivery.options]
keep = 20

[sessions.outbound]
before = ["lua.signature"]

[sessions.turn_error]
provider = "lua.timeout_log_only"

[sessions.turn_error.options]
ops_room = "qq:group/ops"
```

provider id、扩展点、kind 写错会在启动时拒绝，不会等到第一轮才报。配置检查用：

```bash
acb-bridge --config config.toml --check
```

## 错误与兜底

插件出错时 core 不会静默跳过，也不会把会话带进危险状态：

| 扩展点 | 失败时的行为 |
|--------|--------------|
| wake | `stay`（不叫醒） |
| access | 拒绝 |
| receive | 照收（宁可多记一条，不误丢） |
| clock | 不触发 |
| delivery | 回退内置「最旧一小段 + 最新一批」 |
| turn_error | 回退会话动作 / 全局默认动作 |
| send.before | 本次发送失败 |
| send.after | 已发消息不受影响，只累计插件失败次数 |
| 平台适配器 | 发送/调用返回明确的不可用或平台错误 |

判定值本身不合法也按安全侧兜底（例如未知的 wake 判定按 `stay`）。插件失败次数会进
日志和 manager health，可通过 `describe_plugins` / 控制台 `/api/plugins` 查看。

## 开发与测试

```bash
# 动态库示例：独立构建
cargo build --manifest-path examples/dynamic/wake-provider/Cargo.toml

# 各载体的 conformance 测试
cargo test -p acb-adapter-process --test conformance
cargo test -p acb-adapter-dynamic-fixture --test loader
cargo test -p acb-plugin-lua --lib
cargo test -p acb-adapter-wake-fixture --test policy
```

上线前至少确认：

1. 配置 `--check` 通过；
2. 桥启动日志里没有「加载失败 / 未知 provider」；
3. `describe_plugins` 能看到插件、扩展点、metadata 和 health；
4. 真实触发一次扩展点（叫醒、丢一条、发一条、跑一次失败通知），看日志和结果。

## 权限、隔离与版本

- 插件能力由 descriptor 授予；插件拿不到 `Store` / `Bridge` / 配置写入，也不能替别的
  扩展点做决定。`effects` 是给人和 agent 看的说明，不是强制保证。
- 访问策略和连接的 `allow_destructive` 是两道独立闸门；插件不能替破坏性操作放行。
- 动态库在桥进程内，panic/死循环会影响整座桥；需要崩溃隔离就用外置进程。
- 动态库策略判定是同步调用，没有超时；把插件写成快速、无阻塞的判定函数。
- 动态库卸载不支持：换 `.so` 需要重启桥；外置进程崩溃会自动重启。
- wire 的演进是加字段/加消息，ABI 版本和 `PLUGIN_API_VERSION` 对不上会被拒绝。

## 权威定义

- wire / ABI 类型：`crates/adapter-api`
- 平台适配器与事件映射：[05-adapters.md](05-adapters.md)
- 插件宿主设计：[ADR-0026](adr/0026-plugin-host.md)
- 适配器注册与生命周期：[ADR-0027](adr/0027-platform-adapter-registration.md)
- 可运行示例：[examples](../examples/README.md)
