# 配置手册

配置文件是 `config.toml`（模板见 `config.example.toml`），每份配置在示例里都有带注释的完整写法。
本页按「改得多的 → 改得少的」排序讲。

::: tip 不想手改 TOML？
绝大多数配置（连接/会话/人格/记忆/订阅/唤醒策略）都可以在[网页控制台](./console)里用表单增删改，
保存即热重载，注释和排版不会丢。TOML 手改只是兜底。
:::

## 热重载：改了什么时候生效

改动在**当前这轮对话结束后的下一轮**生效——正在进行的对话不会被腰斩，改完不用重启。
唯一例外是 `[endpoint].listen` 这类「启动才知道」的参数，改完要重启桥。

## 连接：一个账号一条

```toml
[[connections]]
id = "qq"                  # 房间名的前缀就是它
platform = "qq"
kind = "onebot11"
transport = "ws-client"    # 桥主动连平台的 WS 服务端
url = "ws://127.0.0.1:3001"
token = ""                 # OneBot 的 access token，没设就空着
account = "10001"          # 这个号自己的 QQ 号——桥靠它认出「哪句是 agent 自己说的」
allow_destructive = false  # 见下
```

- `account` 一定要填对：agent 自己发的消息不计入未读，靠的就是它。
- `allow_destructive` 默认 `false`（踢人/禁言/撤回别人消息这些破坏性操作模型调不到）。
  想让 agent 真能管理群就写 `true`，或者只放行几个：`allow_destructive = ["group.kick"]`。
- 同一个平台第二个号：再复制一段，`id` 换名（如 `qq2`）。同一个群在两个号眼里是两个房间，各记各的账。

## 会话与订阅：谁在哪个群、怎么醒

```toml
[[sessions]]
id = "main"
harness = "dsh"              # 必须是上面 [[harnesses]] 里定义过的 id
persona = "default"          # 必须是定义过的人格卡
memories = ["main-memory"]   # 可空；多个就是多个 git 仓库
model = ""                   # 可空 = harness 侧自己决定用哪个模型
token = "……"                 # harness 连桥时报这个令牌绑定会话

[[sessions.subscriptions]]
connection = "qq"
room = "group/123456789"     # 私聊是 private/对方QQ号

[sessions.subscriptions.wake]
provider = "acb.wake.default"  # 唤醒策略实现；不写就是内置（ADR-0026）
at_mention = true      # 被 @ 醒
reply = true           # 被回复（引用我发的消息）醒
name_mention = true    # 被叫名字醒
names = ["小助手"]      # name_mention 开了才看这个列表
accumulate = 5         # 都没触发时，攒满 5 条新消息也醒
poke = true            # 被戳一戳醒
ignore_speakers = []   # 这些人的发言不叫醒（但照样记账）
```

要点：

- **私聊建议只留 `at_mention`/`reply`**（示例配置就是这么写的），不然对方每句话都叫醒；
- `accumulate` 调小反应快但费 token，调大省钱但「攒话」；
- 两个会话订同一个群完全合法（比如一个主会话 + 一个备用），各自的水位互不干扰。

## 人格与记忆

```toml
[[personas]]
id = "default"
name = "默认人格"
text = """
只写「怎么说话」和「信息边界」。
不要写「你没有 bash」这类能力声明——能力由会话和 harness 决定。
"""

[[memories]]
id = "main-memory"
name = "主记忆"
kind = "git"                                  # 目前只有 git
location = "/home/you/qqab-memories/main"     # 一个 git 仓库路径
```

记忆是给 harness 用的 git 仓库：桥只负责把每个「会话 × 记忆」的 worktree 建到
`state/worktrees/<记忆id>/<会话id>` 下，之后的文件读写、提交全是 agent 自己的事。

## 文件中继：发图发文件的关键

agent 发图时，桥把本机文件收下来、换成 URL 给平台下载。**`public_base` 必须填「平台那边」
能访问到桥的地址**，这是新装最容易翻车的配置。按你的 OneBot 部署方式选：

| OneBot 跑在哪 | `public_base` |
|---|---|
| podman 容器（同机） | `http://host.containers.internal:3131` |
| docker 容器（同机，原生 Linux） | `http://host.docker.internal:3131`（compose 里要加 `extra_hosts: ["host.docker.internal:host-gateway"]`） |
| Docker Desktop | `http://host.docker.internal:3131` |
| 本机裸跑 | `http://127.0.0.1:3131` |
| 另一台机器 | `http://<桥机IP>:3131`（listen 要 `0.0.0.0`，防火墙放行 3131） |

完整说明（含每个场景的坑和验证命令）见 `deploy/relay-examples.md`。

## 其他全局参数

```toml
[retention]
max_records_per_room = 5000   # 每个房间最多留多少条已读记录
max_age_days = 90             # 已读记录最多留多少天
```

只清理**已读**的记录，任何人还没读到的消息一条不删。两项写 0 = 永不清理。

## 一轮的超时

```toml
turn_timeout_secs = 300       # 不配 = 300 秒；0 = 关闭超时
```

一轮对话超过这个时长还没结束，agent 侧会被**主动打断**（dsh 支持打断），打断算这一轮失败、
照常走失败通知。跑大任务（长调研、批量处理）经常超过 5 分钟的话，把值调大或直接写 0 关掉。

## dsh 侧的环境变量

`deploy/start-drivers.sh` 里给驱动设了两个环境变量，控制 dsh 网页里的工作目录和分组：

- `ACB_CWD`：会话的工作目录（默认仓库下的 `state/agents`）。改了它，dsh 里旧的
  同名会话会报「belongs to」冲突——驱动会自动换个带代次的新 id 建会话，旧的作废；
- `ACB_WORKSPACE_TITLE`：dsh 里新建工作区的显示名（默认「ACB」）。

::: warning dsh 工具清单是启动快照
给 agent 用的工具定义改了之后，要重启 dsh 才会进模型上下文。
:::
