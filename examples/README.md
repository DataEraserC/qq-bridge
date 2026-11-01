# 插件示例

这里的文件是给用户照抄的最小示例，不是桥的内置功能。三个载体都能选：

| 目录 | 载体 | 说明 |
|------|------|------|
| `lua/` | Lua 脚本 | 写策略 provider 最快；需要 `ACB_LUA_DIR` |
| `process/` | 外置进程 | 任意语言写平台适配器；通过连接配置启动 |
| 动态库 | `.so` / `.dylib` | 参考 `crates/adapters/dynamic-fixture`（适配器）和 `crates/plugins/wake-fixture`（策略） |

## Lua 策略

文件名就是 provider 名：`quiet_hours.lua` → `lua.quiet_hours`。

```text
examples/lua/
  quiet_hours.lua      wake：夜间不主动醒，@/回复仍然醒
  allow_read_only.lua  access：只放行读
  drop_noise.lua       receive：正文以 "!!" 开头的消息不记流水
```

让桥装上 LuaLoader（systemd 用户服务）：

```bash
systemctl --user edit acb-bridge
```

```ini
[Service]
Environment=ACB_LUA_DIR=/绝对路径/到/仓库/examples/lua
```

```bash
systemctl --user daemon-reload
systemctl --user restart acb-bridge
```

然后在配置里引用：

```toml
# 唤醒策略：按房间
[sessions.subscriptions.wake]
provider = "lua.quiet_hours"

# 插件专属参数：core 不解释，原样交给 provider（脚本里是 options.extra）
[sessions.subscriptions.wake.options]
start_hour = 23
end_hour = 8

# 访问策略：按会话
[sessions.access]
provider = "lua.allow_read_only"

# 入站策略：按连接（放在 [[connections]] 后面）
[connections.receive]
provider = "lua.drop_noise"
```

注意：Lua 每次调用最多 50ms；脚本拿不到 Store / 流水 / 配置写入，脚本里的计数器只活在
内存里，桥重启就清零。需要跨重启的统计，等 provider 状态/历史接口补齐后再做。

插件专属参数走 `.options` 自由表：wake / access / delivery / clock / receive 都支持。
脚本收到时统一放在 `options.extra`（receive 的第二个参数也是 options 表）：

```lua
-- wake / access / clock / delivery
decide = function(options, context) ... options.extra.min_count ... end
-- receive
decide = function(request, options) ... options.extra.blocked ... end
```

## 外置进程适配器

`process/adapter.mjs` 用 Node 写了一个最小适配器：说 hello（声明事件/标签能力）、
发一条入站消息和一条平台事件、回 send/call。

```toml
[[connections]]
id = "example"
platform = "example"
kind = "process"
transport = "process"
account = "example-bot"

[connections.options]
command = ["node", "examples/process/adapter.mjs"]
```

会话订阅 `example:group/1` 后，启动适配器时会收到示例消息与事件；`platform_methods`
里能看到 `example.echo` / `example.send`；`describe_plugins` 的 `adapters` 里能看到
`events = ["example.greet"]`、`event_tags = ["notice"]`。

任何语言都能写：只要第一行是 hello（带 `info` 声明入站能力），之后一行一条 JSON，
按 `id` 回 reply 即可。完整 wire 形状见 `crates/adapter-api` 和
[docs/05-adapters.md](../docs/05-adapters.md)。

## 动态库

动态库示例是 Rust 的 conformance fixture，直接当参考实现看：

- 平台适配器：`crates/adapters/dynamic-fixture`
- wake / access / receive / clock 策略：`crates/plugins/wake-fixture`

配置写法：

```toml
[[plugins]]
path = "/绝对路径/到/libdemo_adapter.so"
kind = "demo"          # 可选；必须和 manifest 的 kind 一致
```

动态库和桥同进程：插件 panic/卡死会影响桥；要隔离就用 `process`。
