# agent-chat-bridge

把各种 agent harness（DSH / WorkBuddy / Trae / Codex / OpenClaw…）接进各种聊天平台
（QQ / KOOK / Discord / Telegram…）的桥。让你在群里或私聊里，跟你自己的 agent 说话。

桥负责四件事：**收发翻译、何时叫醒、记了什么、能发到哪**。大脑在 harness 那边，
桥不替它思考。

## 文档

两套入口，写给不同的人：

- **使用指南**（给人看，不看代码也能上手）：https://dataeraserc.github.io/qq-bridge/guide/
  ——快速开始、配置手册、网页控制台、日常使用、排障手册；
- **开发者文档**（架构、协议、ADR）：https://dataeraserc.github.io/qq-bridge/03-architecture
  ——本仓库 `docs/` 目录的渲染版，想改代码从这里进。
- **插件示例**：[examples/README.md](examples/README.md)
  ——Lua 策略、外置进程适配器、动态库参考实现。

## 现在是什么状态

已经能跑通一整条链：平台推消息 → 记进流水 → 按订阅的唤醒策略决定要不要叫醒 →
打包投给 harness → harness 调工具回话 → 编成平台动作发出去。

| 部分 | 状态 |
|------|------|
| 协议（桥 ↔ harness） | 可用：握手、开轮、工具调用、收工，一行一帧 JSON |
| 核心（连接/订阅/唤醒/流水/水位/笔记/回合调度） | 可用：工具面 9 个工具，同一份记忆的轮次严格串行 |
| 接入点（harness 连进来） | 可用：TCP 长连接 + 接入令牌（一把令牌代表一组设备） |
| harness 适配器（opencode v2） | **真机整轮验过**（2026-10-29 19:26，最后一轮 10 秒回话）：真 QQ 发消息 → 桥开轮 → 模型收到 → `execute` 里调 `send_message` 回话 → `ack_messages` 推水位。连接模型上线后重验过：会话按**记忆**开（`acb:main-memory`），出站消息存平台编号，所以引用机器人的话能叫醒。代码 28 个测试 |
| harness 适配器（DSH / Trae / Codex…） | 没写，判断方法见 [docs/06](docs/06-harness-adapters.md) |
| QQ 适配器（OneBot 11） | 可用：群与私聊收发、拍一拍、其它平台事件，断线自动重连 |
| Discord / KOOK / Telegram | 没写 |
| 存储 | 可用：SQLite 落盘，重启接着上次的账做（编号、水位、笔记、投递次数都在）；旧流水按条数/天数自动清理，只削读过的 |
| Web 管理页 | 没做，现在改配置要改文件重启 |
| `wait_message`（阻塞等新消息） | 没做 |
| 时钟策略（房间安静时例行检查） | 没做 |

## 快速开始

需要 Rust 1.85 以上。

```bash
# 1. 复制示例配置，按自己的情况改
cp config.example.toml config.toml

# 2. 只检查配置，不启动
cargo run -p acb-bridge -- --config config.toml --check

# 3. 起桥
cargo run -p acb-bridge -- --config config.toml

# 4. 另开一个终端，接一个最小 harness（真 harness 还没接）
cargo run -p acb-bridge --bin dev-harness -- --token <配置里设备的 token>
```

第一次起桥会在工作目录建 `state/acb.db`：流水、水位、笔记、「第几次投递」全在里面，
重启接着用。删了等于失忆（但不会起不来）。细节见 [docs/07-storage.md](docs/07-storage.md)。

要用真的 opencode 接进来（**只支持 v2**）。插件留在仓库里，配置文件指向它就行，不要拷贝
（本机根目录是 tmpfs，拷出去的东西不保险）：

```jsonc
// ~/.config/opencode/opencode.json 或项目根的 opencode.jsonc
{ "plugins": [ { "package": "<仓库绝对路径>/harnesses/opencode/plugin",
                 "options": { "token": "<配置里设备的 token>" } } ] }
```

```bash
opencode2 --standalone     # 必须带 --standalone，后台 service 模式解析不到插件入口
```

现成能抄的那份在 [harnesses/opencode/opencode.jsonc.example](harnesses/opencode/opencode.jsonc.example)。
接上之后 stderr 会打 `[acb] info 已接入桥，绑定设备组 main`；有人在 QQ 里说话，模型就会通过
`execute` 调 `await tools.send_message({...})` 回过去（插件工具在 opencode 里走 Code Mode，
不在原生工具表里——这条坑和整轮实测记录见 [docs/06](docs/06-harness-adapters.md)）。

不想装 opencode 也想看这一段通不通，用同一个协议客户端的探针：

```bash
node harnesses/opencode/probe.mjs --token <配置里设备的 token>
```

然后在群里 @ 一下那个号。链通的样子：桥上打出「登录账号 xxx」，harness 那边打出投递
过来的记录并自动回一句，群里能看到那句话。

QQ 这一侧需要 OneBot 11 实现（NapCat / Lagrange / go-cqhttp 等）开出的正向 WebSocket
服务，**消息格式要设成 `array`**——设成 CQ 码字符串的话，@ 认不出来，「@ 我才醒」不会
生效。细节见 [docs/05-adapters.md](docs/05-adapters.md)。

## 文档

先读术语表，很多争论都死在对词的理解不一样上。

| 文档 | 内容 |
|------|------|
| [docs/01-vision.md](docs/01-vision.md) | 愿景、核心管什么、不管什么、非目标 |
| [docs/02-glossary.md](docs/02-glossary.md) | 术语表（全库禁用「会话」这个词） |
| [docs/03-architecture.md](docs/03-architecture.md) | 组件、数据流、存储与并发、目录规划 |
| [docs/04-protocol.md](docs/04-protocol.md) | 桥与 harness 之间的协议 |
| [docs/05-adapters.md](docs/05-adapters.md) | 适配器：房间写法、事件与元素映射、限制 |
| [docs/06-harness-adapters.md](docs/06-harness-adapters.md) | harness 适配器：两半能力、各家怎么接、opencode 那一份怎么用 |
| [docs/07-storage.md](docs/07-storage.md) | 状态库：每张表干什么、清理规则、怎么查、怎么备份、已知限制 |
| [docs/adr/](docs/adr/) | 决策记录，每条标「用户裁定」或「实现默认」 |

## 验证

```bash
cargo test --workspace                    # 单元 + 调度 + 接入点
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings

node --test harnesses/opencode/test/*.test.mjs   # harness 适配器：协议层 18 + 插件逻辑 10
```

## 许可证

MIT，见 [LICENSE](LICENSE)。
