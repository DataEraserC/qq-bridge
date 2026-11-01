# 适配器：把平台接进桥

适配器是桥与聊天平台之间唯一的一层。核心不认识 QQ，也不认识 OneBot；适配器负责把平台
的话翻译成核心能懂的形状，再把核心要发的东西翻译回去。

## 一件必须先定下来的事：房间怎么写

房间由**连接 + 路径**决定，中间用冒号隔开；路径是「段/段」，每段之间用斜杠。第一段说
场所类型，由适配器决定切成什么样——核心不认识这些词，只当一串段：

| 场所 | 房间写法 | 例子 |
|------|----------|------|
| QQ 群 | `<连接>:group/<群号>` | `qq:group/123456789` |
| QQ 私聊 | `<连接>:private/<对方号>` | `qq:private/10001` |
| 将来的「楼」（帖子） | `<连接>:guild/<id>/channel/<id>/post/<id>` | 深度由那边的适配器定 |

来源：用户裁定（「房间第一段写连接名」），具体段数由适配器定。

两个要点：

- **第一段是连接名，不是平台名**。同一个平台挂两个号就是两条连接（`qq` / `qq2`），
  两个号看同一个群 = 两个房间，各记各的流水和水位，不会重复收。
- **路径前缀就是父房间**：要查「这个群下面所有楼」，前缀匹配 `qq:group/123` 即可，
  不需要为「父房间」单独加字段。

## 配置

```toml
[[connections]]
id = "qq"              # 连接名：房间前缀、订阅引用都用它
platform = "qq"        # 属于哪个平台
kind = "onebot11"      # 用哪个适配器（内置 onebot11 / process）
transport = "ws-client" # 连接方向：桥主动去连（ws-server 还没实现）
url = "ws://127.0.0.1:3001"   # ws-client 必填；process 不用
options = { ... }      # 适配器专属配置；onebot11 目前不需要
token = "..."          # OneBot 实现开了 access_token 就填
account = "10001"      # 这个号登录的是谁，必填
```

- **一个账号一条连接**，同一个平台可以配多条（这是用户裁定的模型，见
  [ADR-0012](adr/0012-platform-connections.md)）。连接名重复、`kind` / `transport` 写错、
  `account` 空、`ws-client` 没给合法地址，都会在启动时报错；未知适配器 kind 在 Root
  装载适配器时拒绝启动。
- 只支持 `ws://`。`wss://` 要额外拉 TLS 依赖，等真有需要再加。
- 地址里可以带查询串，比如 `ws://127.0.0.1:3001?access_token=xxx`——查询串会原样带到
  握手请求里，很多 OneBot 实现就是这么鉴权的。
- 连上之后会问一次 `get_login_info`，跟配置里的 `account` 对一下；对不上只警告——
  自己的话会被当成别人说的，通常说明配置写错了。

## OneBot 11 走哪种连接

用**正向 WebSocket**（`transport = "ws-client"`）：桥作为客户端，连到 OneBot 实现
（NapCat / Lagrange / go-cqhttp 等）开出来的 WS 服务端。

来源：实现默认，可推翻。理由只有一个：反向 WS 要桥自带一个 WS 服务端，多一层监听和鉴权，
正向只要一个地址就能跑。真需要反向，加一个 `transport` 取值即可，配置形状不用变。

### OneBot 侧必须设的一项

**消息格式要设成 `array`**（`post-format=array` / `messagePostFormat: "array"`）。

设成 `string`（CQ 码）也能跑，但适配器认不出 `[CQ:at,qq=...]` 这种文本里的 @，
于是「@ 我才醒」就永远不生效。这不是适配器的锅，是平台没给结构化数据。

## 外置进程适配器（`kind = "process"`）

适配器实现不一定要编译进桥。`kind = "process"` 会让桥按配置启动一个子进程，
双方在 stdin/stdout 上一行一条 JSON（协议定义在 `crates/adapter-api`）：

```text
core ──▶ 子进程   send / call / shutdown
core ◀── 子进程   hello（第一行）/ event / reply / log
```

```toml
[[connections]]
id = "demo"
platform = "demo"
kind = "process"
transport = "process"
account = "bot"

[connections.options]
command = ["/path/to/adapter", "--stdio"]
# 可选：args = ["..."], env = { KEY = "value" }, cwd = "/path",
#       startup_timeout_secs = 10
```

- 子进程启动后第一行必须是 `hello`，带上自己的平台方法目录；超时（默认 10 秒）没等到就启动失败。
- `hello.info` 可以声明这条连接的入站能力：`events`（会产生的平台事件名，如
  `example.greet`）、`event_tags`（跨平台语义标签，如 `notice`）、`supports_at` /
  `supports_reply`。不写就按「不声明」处理；`describe_plugins` 会展示这些字段。
- 入站事件发 `event`；`send` / `call` 按 `id` 回 `reply`，错误用 `kind` 分类（`bad_params` / `forbidden` / `unavailable` …）。
- 子进程退出后自动重启：500ms 起按倍数退避、最多 10 秒；重启期间未完成的请求明确报「适配器进程退出了」。
  重启成功后沿用同一个 outbound 句柄，平台方法目录按新 hello 更新；能力声明是启动时快照。
- 子进程崩溃后，未完成的请求明确报「通道不可用」；桥退出时发 `shutdown`，2 秒内不收尾就 kill。
- 一个最小的参考实现见 `crates/adapters/process/src/bin/fixture.rs`。
- 想直接抄一个可运行的 Node 版本，见 [`examples/process/adapter.mjs`](../examples/process/adapter.mjs)。

## 动态库插件（`[[plugins]]`）

适配器也可以编成 `.so` / `.dylib`，由桥用 libloading 装载、在桥进程内运行：

```toml
[[plugins]]
path = "/path/to/libdemo_adapter.so"
kind = "demo"          # 可选：适配器 manifest 的 kind 必须等于它
```

动态库只导出 `acb_adapter_entry_v1`，返回一张 C 函数表；manifest、连接配置、
入站事件、发送/调用请求和回执都是 JSON，形状与 `kind = "process"` 完全一致
（`crates/adapter-api`）。插件通过回调 `submit_event` 把入站记录交给桥。

- 动态库和桥同进程：插件 panic、死循环或内存错误会带倒整座桥。需要崩溃隔离就用
  `kind = "process"`。
- 动态库的发送/调用带 2 秒超时：插件卡住时 core 拿回错误而不是陪它一起挂着。插件线程
  本身无法安全打断，超时后这个 handle 视为不可用，需要重启桥。
- 动态库没有 unload：Root 不保留 loader，适配器连接也持有库的函数指针；要换插件就重启桥。
- 第三方插件必须声明 `metadata.summary`；wake 插件还必须声明 `effects`
  （至少 `may_wake` / `may_stay_silent`），否则启动时拒绝。
- 同一个载体也能装决策型策略：manifest 写 `extension = "access"` / `"receive"` / `"clock"`，
  provider id 用 `namespace.name`。插件收到 `CoreMessage::Decide`（`point` 是扩展点，
  `request` 是 access / receive / clock 的 wire 形状），返回 `WireResult::Decision`：
  access 用 `allow` / `deny`，receive 用 `record` / `drop`，clock 用 `due` / `skip`。
  报错按安全侧兜底（access 拒绝、receive 接收、clock 不触发）。
- 一个最小的参考实现见 `crates/adapters/dynamic-fixture/src/lib.rs`；策略插件参考
  `crates/plugins/wake-fixture`。用户可读的示例总览见 [`examples/README.md`](../examples/README.md)。

### 适配器 manifest 声明什么

三种载体（内置、外置进程、动态库）共用同一份 manifest，除 id / platform / kind /
transport / 平台方法目录外，还声明入站语义能力：

| 字段 | 含义 |
|------|------|
| `events` | 会写进流水的平台事件名（不带平台前缀），如 `poke`、`group_increase`；OneBot 的 `notice_type` 是开放的，没列出的也会原样记 |
| `event_tags` | 给事件打的跨平台语义标签，如 `poke`、`reaction`、`notice`；唤醒策略的 `event_tags` 按它匹配 |
| `supports_at` | 入站记录里的 `at_self`（@ 到本账号 / 冲我来的事件）是否可信 |
| `supports_reply` | 入站记录里的 `reply_to_self`（引用我发的消息）是否可信 |

这些字段是**声明不是保证**：agent 通过 `describe_plugins` / `platform_methods(all=true)` /
`list_rooms` 看得到，用来判断某个平台能配哪些唤醒条件；core 不拿它做强制校验。
`process` 载体声明 `supports_at` / `supports_reply`（子进程原样带 hints），事件留空由子进程决定；
动态库插件在自己的 `WireManifest` 里填。

### wake 策略插件

同一张 `[[plugins]]` 表也装策略 provider。插件 manifest 写
`extension = "wake"`、`id = "vendor.wake"`，然后在订阅里引用：

```toml
[[plugins]]
path = "/path/to/libwake.so"

[sessions.subscriptions.wake]
provider = "vendor.wake"

# 插件专属参数：core 不解释，原样交给 provider（wire / Lua 里是 options.extra）
[sessions.subscriptions.wake.options]
threshold = 5
```

access / receive / clock / delivery 的策略配置也有同名的 `.options` 自由表；
`options_schema` 目前只是说明，core 不做强制校验。

插件通过 `CoreMessage::Decide` 收到 `WireWakeRequest`（发送者、正文、@/引用/拍、
标签、未读数），返回 `WireResult::Decision { decision = "wake" | "stay" }`。
解析失败或插件报错按 `stay` 兜底，不会误把会话叫醒。wake 判定在 core 的入库路径里，
所以这一版只开放动态库载体；外置进程会在 core 锁内做 I/O，等以后有异步判定再开。

## Agent 怎么知道插件在做什么

桥给 agent 一个只读工具 `describe_plugins`：

- 默认 `scope=session`：返回本会话实际生效的 access / delivery / turn_error / receive /
  send_hooks，每个订阅的 wake / clock，以及当前连接用的适配器 kind、平台方法数和说明。
- `scope=global`：返回整个 Root 目录，可用 `extension` 过滤。

每条都带 `title` / `summary` / `description` / `tags` / `effects` / `health`。描述由插件
作者在 manifest 里声明，是**说明不是指令**；`effects`（如 `may_delay`、
`may_modify_text`、`may_wake`）是文档，不是强制保证。控制台的 `GET /api/plugins`
是同一份目录的只读 JSON。

轮次开头也会带 `wake_reason`：内置/trusted/动态库 wake 策略命中时给出具体原因
（如「有新消息（@ 了我）」），agent 不用猜自己为什么被叫醒。Lua 脚本可以返回
`{ decision = "wake", reason = "..." }` 表；只返回布尔/字符串时原因为空（协议版本 5）。
事件记录扩了 `tags` / `payload` / `summary` 后，协议版本升到 6。

## 事件怎么映射

| OneBot 事件 | 变成什么 | 说明 |
|-------------|----------|------|
| `post_type=message`，`message_type=group` | 一条消息，房间 `qq:group/<群号>`（连接名是 `qq` 时）| 发送者名字优先用群名片，其次昵称 |
| `post_type=message`，`message_type=private` | 一条消息，房间 `qq:private/<号>`（同理） | |
| `post_type=notice`，`notice_type=notify`，`sub_type=poke` | 平台事件 `poke`，tags `["poke", "notice"]`，payload 原始事件，正文「[拍了拍我]」 | 拍的是我则标记「冲我来」，唤醒策略据此判定 |
| 其它 `notice` | 平台事件（事件类型就是 `notice_type`），tags `["notice", notice_type]` | 进退群、禁言、撤回等，正文是一句人话，原始事件进 payload |
| `post_type=request` | 平台事件 `request_<类型>`，tags `["request", request_type]` | 加好友、加群邀请 |
| `post_type=meta_event` | **不记** | 心跳每几秒一次，记下来只会把流水撑爆 |
| `post_type=message_sent` | **不记** | 自己发出消息的回执，桥在发送时已经记过 |

消息、拍一拍、其它事件走的是同一条路（ADR-0005）：都变成一条记录，交给桥决定要不要
算未读、要不要叫醒。core 不认识平台事件名——适配器给事件打 `tags`，唤醒策略按
`event_tags` 匹配；要精确到平台事件名就写 `events = ["qq.poke"]`，带前缀的名字必须
属于该房间的连接平台（`qq.*` 匹配整个平台），配错在启动时就报错。

## 内容元素怎么翻译

出去（桥 → QQ）：

| 元素 | OneBot 段 | 备注 |
|------|-----------|------|
| text | `text` | |
| at | `at`，`all` 时 `qq="all"` | |
| quote | `reply` | |
| image / audio / video | `image` / `record` / `video` | `file` 接受三种：http(s) 直链、`base64://…`（原始内容 750 KB 内，走协议帧）、桥文件中继给的 URL |
| face | `face` | |
| file | **报错** | OneBot 11 没有标准文件段，明确报错而不是悄悄丢 |
| opaque（platform=qq） | 原样送出 | 平台特有内容照搬 |
| opaque（别的平台） | **报错** | 别处的内容发不到 QQ |

进来（QQ → 桥）：

| OneBot 段 | 元素 |
|-----------|------|
| `text` / `at` / `reply` | text / at / quote |
| `image` / `record` / `video` | image / audio / video，优先取直链，取不到用平台内部文件名 |
| `face` | 表情，带平台名与 id |
| 其它（卡片、合并转发、markdown…） | 不透明载荷，原样留着，编回 QQ 时照抄 |

多带两个字段，都是给「引用回复」用的：

- 每条消息的 `message_id` 存进记录（`platform_id`）。我方发出去的消息也把 OneBot 返回的
  `message_id` 存下来——**没有它，就反查不出「被引用的是谁发的」**。
- `reply` 段解成 `quote` 元素，只留被引用那条的编号。判定「这是不是在回我」放在桥里做
  （要查库），不在适配器里做：适配器只管把平台给的东西翻译全，不猜语义。

## 文件怎么跨命名空间（ADR-0025）

平台跑在容器里、agent 在宿主机是默认部署姿势，两边不共用文件系统：agent 给的
`/home/.../a.png`，平台 stat 不到，报一个裸 ENOENT。解决不靠路径映射（每个环境要单独配
一遍，docker/podman/windows × 各家 OneBot 配不完），靠**桥自带文件服务**：

- 连接配置里加了 `[file_relay]`（`listen` / `public_base` / `dir`）就启用：
  `send_message` 的元素和 `platform_call` 的 params 里，凡是**本机真实存在**的绝对路径，
  桥收下文件、换成 `http://<public_base>/f/<token>/<文件名>` 交给平台。
- 没配就完全不碰路径，行为同以前。`http(s)://`、`base64://`、本机不存在的路径（可能是
  平台侧才有的，比如共享卷）一律原样透传。
- `send_message` 换了路径时回执带 `relayed: [{from, to}]`。
- 错误信息里出现 ENOENT / realpath / image not found 时，桥会附上三条出路，防止排查
  被裸 ENOENT 带偏去查拼写和权限。

## 合并转发怎么摊开

QQ 的「聊天记录」卡片是一条消息里嵌一个转发 id，展开一层又是同样的东西。`message.forward_content`
在**适配器里**递归取（`forward.rs`），一次给到最里层：

- 参数 `id`（转发 id）+ 可选 `depth`（默认 3、最多 5）/ `max_nodes`（默认 200、最多 500）；
- 返回摊平的 `nodes`：每条带 `path`（`1.0` = 第 2 条的第 1 个回复）、`depth`、`sender`、`text`、
  原始 `content`，以及嵌套时的 `forward_id`；
- 另有 `leaf_text`：全部节点正文拼成的一段话，嵌套再深也一眼看完；
- 同一个转发 id 只展开一次（防自引用）；到上限 `has_more` 为真，把某个 `forward_id` 再喂进
  来就能接着看。

递归放在这里而不是 core：**「一次只给一层」是平台的规矩**，core 不认识任何平台方法名（ADR-0020）。

## 连接出问题会怎样

- **连不上**：每 5 秒重试一次，不退出进程。OneBot 后开也能连上。
- **断了**：自动重连；断在轮次中间的那一轮作废，没确认的消息仍是未读，下一轮重投。
- **发消息时没连上**：工具调用直接返回错误，不假装成功。
- **发消息等回话超时**：10 秒没回就报错。平台真的卡住时，AI 知道这次没发出去。

连上之后适配器会问一次「你登录的是哪个号」（`get_login_info`）。如果这个号和连接里
配的 `account` 对不上，会打一行提醒——因为对不上就意味着自己说的话会被当成别人
说的，严重时会自己回自己。

## 已知限制

1. 不支持 `wss://`。
2. 不支持反向 WebSocket（桥当服务端）。
3. 发文件会报错：OneBot 11 没有标准的文件段，各实现的扩展不统一，不做猜测。
4. 合并转发只能收、不能主动发（发的时候是原样透传，能不能成看平台实现）。
5. 一条连接对应一个登录账号。要同时挂两个 QQ 号，就配两个平台名（如 `qq` 和 `qq2`）
   各连一条——这是配置层的限制，不是设计上的。

## 怎么验

平台这一侧只有真环境能验。拿真的 OneBot 地址填进配置后：

```bash
# 1. 先只检查配置，不启动
cargo run -p acb-bridge -- --config config.toml --check

# 2. 起桥
cargo run -p acb-bridge -- --config config.toml

# 3. 另开一个终端，用开发用的最小 harness 接上去
cargo run -p acb-bridge --bin dev-harness -- --token <配置里的 token>
```

然后在群里 @ 一下那个号，桥上应当看到：

```text
适配器 qq：已连上 ws://127.0.0.1:3001
适配器 qq：登录账号 10001
```

harness 那边应当看到投递过来的记录，并自动回一句话；群里能看到那句话，就说明整条链
通了（收 → 记 → 醒 → 投 → 发 → 回流）。
