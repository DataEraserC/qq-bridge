# 交接文档 · acb-bridge（qq-bridge-fork / qqab-arch worktree）

> 写给下一个 agent。本文档 + 项目 memory 目录 + 本 session 数据，三者合起来就是完整上下文。

---

## 0. 三个上下文入口（按优先级）

| 优先级 | 路径 | 说明 |
|---|---|---|
| **1. 项目 memory** | `/home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/.workbuddy/memory/` | 长期项目备忘，最权威。`MEMORY.md`（长期笔记）+ `YYYY-MM-DD.md`（每日日志，append-only） |
| **2. 本 session 数据** | **读不到，需要用户手工给** ⚠️ | `~/.workbuddy/projects/<项目名>/` 下**只有 tool-results 转储**，没有会话正文；`~/.workbuddy/sessions/` 里只有 5 个小 .json 元信息，grep 不到 sessionId。**要原始记录只能让用户把 `~/.workbuddy/projects/<项目名>/<sessionId>.jsonl` 打包成 tar 交过来**（74MB，含 15888 条事件） |
| **3. 起点** | 本文件 + memory | 有这两样就够开工了 |

**本 session 的 sessionId：`026d8d74-4955-40f5-a386-457dd2678244`**

**项目根目录（cwd）：`/home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch`**

**WorkBuddy 总目录：`~/.workbuddy/`**（sessions、memory、projects、workbuddy.db 都在这里）

---

## 1. 项目是什么

**acb-bridge**：一个「多 harness × 多聊天平台」的桥（bridge），不是「QQ 机器人框架」。

- 目标：让 DSH / WorkBuddy / Trae / Codex / OpenClaw 这类 **harness**（AI 运行时），通过统一协议接上 QQ / KOOK / Discord / Telegram 这类**聊天平台**。
- core 只管跨平台跨 harness 都成立的部分；harness 工具、平台特有事件不归 core 管，但 core 必须有抽象承载住、不能丢。
- 技术栈：**Rust**（kernel + proto + adapters + bridge），harness 驱动是 **Node.js**（`harnesses/dsh/`、`harnesses/opencode/`）。
- 构建：**nix**（`flake.nix` + `buildRustPackage`）。验证用 `nix build .#acb-bridge`（会自动跑 cargo test）。

### 核心对象模型（ADR-0017 定案，五个一等对象）

1. **连接**（connection）：一个账号一条管道，同平台可多条。房间 = `连接名:段/段`（如 `qq:group/158296748`）。
2. **人格**（persona）：纯文本人设卡，投递时注入系统提示词。
3. **记忆**（memory）：抽象接口，git 是第一种实现。桥只给「仓库路径 + 文件能力 + git 能力」，不定义「什么是记忆」。
4. **harness**：一等对象，`{id, name}`。桥不主动连 harness，只存 id 做校验。
5. **harness 会话**（session）：串行单元，串行键 = `harness id + 会话 id`。挂人格/记忆/model/订阅列表。

**关键裁定**：device 概念已删除，升级为 harness 会话。`send_message(text)` 回当前轮会话；`send_message_to(text, to)` 发本会话订阅范围内的其它房间。跨 harness 会话发送已砍掉。

---

## 2. 当前代码/运行状态

- **协议版本 v3**，桥和 harness 必须同批次换（`frame.rs:160` 严格相等）。
  **注意**：加平台能力（ADR-0020）没有动协议——适配器是进程内的，`Outbound` 扩了 `catalog()` / `call()`
  两个方法、都带默认实现，所以现有测试 mock 一个都没改。
- **库 schema 6**（`state/acb.db`），records 有 `sender_role` 列。
- **真机在跑**：桥 + 两个 dsh driver 进程都在运行，会话复用正常。
- **工具面 24 个**：原 22 个 + `platform_methods` / `platform_call`（ADR-0020）。两个 harness 现在共用
  `harnesses/shared/tools.mjs` 这一份定义（opencode 原来手抄了一份副本，已删——见第 3 节）。

### 真机配置（`config.toml`，已被 .gitignore 忽略）

- 机器人账号：`3683924053`（专吃偷啃的DeepSleep），OneBot 在 snowluma 容器（53001 WS）。
- 两个会话：
  - `dsh`：订阅群 `158296748` + 私聊 `323622442`，失败通知走全局默认（发私聊 323622442）。
  - `dsh-group2`：订阅群 `769843907`，失败通知显式配了（发群 158296748 + 私聊 323622442）。
- `admin_qqs = ["323622442"]`（系统管理员），`delivery_limit = 30`（投递截断）。

---

## 3. 已解决的关键问题（最新在前）

### 2026-10-30 晚：平台特有能力的通用通路（ADR-0020）

**问题**：模型只能看见「这是条合并转发，id 是 X」，读不到正文；想加任何平台能力都得往 core 里塞代码，
core 迟早认识 QQ。

**做法**（完整理由见 `docs/adr/0020-platform-capabilities.md`）：

- core 只加**两个通用工具**，一个都不带平台名字：`platform_methods`（能力目录，不给条件=逐条清单，
  给 `group` / `tag` / `query`=命中方法连参数表，三者可叠加）/ `platform_call`（`{room, method, params}`，
  params 是任意 map，core 原样转交，不认识任何字段）。
- 目录两个维度：名字前缀是**归属**（跟平台 API 命名空间对齐），`MethodSpec.tags` 是**用途**（跨归属，
  如 `image` 同时挂在 `message.send_image` 和 `group.get_file_url` 上）。core 给一张中英别名表
  （`expand_query`），模型说「图片」也能搜到挂 `image` 的方法。
- 适配器自己声明方法表（`MethodSpec { name, summary, tags, risk, scope, params }`），强制 `resource.method` 命名。
  方法还分两种**作用域**：房间级（默认，默认房间里的事）与连接级（`scope: connection`，
  好友列表/账号资料/群列表/点赞/链接检测这类「号上的事」）。连接级方法 `room` 可以不给——
  逼模型为了查好友列表先编一个群号出来，只会把订阅边界变成一张要填的表。10 个方法标了连接级。
  QQ 现在声明 **52 个方法**（`crates/adapters/onebot11/src/catalog.rs`），铺的是「人在这号上能做的事」：
  发言/发图/发语音/发文件/合并转发/撤回/引用/拍一拍/点赞/看历史/看成员/群文件/改名片/改群名/设管理/
  禁言/踢人/发公告/审批入群/加删好友/改群头像。**52 个都对着真机 NapCat 探测过，全部存在**。
- `Outbound` 扩了 `catalog()` / `call()`，**都带默认实现**，所以现有 6 处测试 mock 一个都没改。
  OneBot 侧不需要新通道：`Action{action,params,echo}` 本来就是通用 RPC，`send` 只是它的特例。
- 错误分五种，分类当 message 前缀给模型：`unknown_method`（附同组近似名）/ `unsupported_platform` /
  `not_implemented` / `forbidden` / `bad_params`（指出哪个字段）。模型靠这个自己纠错，不是靠猜。
- `Risk::Destructive` 默认拒，但连接上一个开关能放行：`allow_destructive = true`（全开）/
  `["group.kick"]`（只开这几个）/ 不写（全关）。真机 `config.toml` 现在是 `true`——
  用户要求「人能做的模型都能做」。开了就意味这个号能干的事模型都能干，这是明示的风险。
- opaque 渲染带上「可调什么」（affordance）：`[QQ 平台特有内容（未展开，可调
  platform_call(method="forward.get_content", params={"id":"…"}) 读取正文），原样如下：{…}]`。
  不主动展开正文（体积/限流/隐私），但也不让模型以为自己无能为力。

**顺带修掉一个真 bug**：opencode 插件的 `TOOL_SPECS` 是**手抄副本**，早就跟 `shared/tools.mjs` 漂移了——
共享版有 `fetch_records`，opencode 一个都没有，而投递文本里却在提示模型「用 fetch_records 抓剩下的」，
模型照着调只会撞一个不存在的工具。现在两边都从 `shared/tools.mjs` 取，24 个工具完全一致。

**验证**：192 个 Rust 测试全过（原 166，新增 26）；20 个 Node 测试全过；`curl` 直打 OneBot 确认
`get_forward_msg` 的参数名就是 `id`、真能取到转发正文；dsh 的 MCP `tools/list` 确认 24 个工具里
两个新工具都在、schema 正确。

### 已修复并真机验证：`content-blocked` 失败通知不生效

**现象**：模型返回 `400 content-blocked`（agent_router_api_error）时，桥没把失败信息发回用户。

**排查过程的两层根因（都要知道）：**

1. **第一层（代码 bug，已修）**：`engine.rs` 的 `notify_turn_error` 判断「会话有没有配 on_turn_error」时用了 `.map()`，只要会话存在就返回 `Some(空列表)`，导致永远回退不到全局默认。修复：加 `.filter(|s| !s.on_turn_error.is_empty())`。

2. **第二层（真正的根因，已修）**：`config.toml` 里 `default_on_turn_error` 和 `delivery_limit` 两个键写在了 `[endpoint]`/`[console]` **段之后**，被 TOML 规则静默吞进了 `[console]` 表，顶层键实际是空的。**修复：移到 config.toml 最开头（任何 `[table]` 之前）**。这同时解释了 `delivery_limit`（消息截断）也没生效。

**验证**：真机重启后，库 record 85/86 出现 content-blocked 的「[一轮失败]」通知，成功发到私聊 323622442。

### 本会话之前的其他已完成项（详见 memory）

- 昵称/身份显示（sender.role：SystemAdmin/Owner/Admin，基于 admin_qqs + OneBot role）
- 消息过多投递截断（delivery_limit + 提示抓取）
- 会话复用（driver 不删缓存 + seedMaps 跨重启复用，ADR-0018 session_maps 表）

---

## 4. 关键路径速查

| 东西 | 路径 |
|---|---|
| 项目根 | `/home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch` |
| 配置文件 | 同上 `/config.toml`（真机，已忽略入库）、`/config.example.toml`（模板） |
| 状态库 | 同上 `/state/acb.db`（三件套：.db / -wal / -shm） |
| 桥二进制 | 同上 `/result/bin/acb-bridge`（`result` 是 nix build 软链） |
| kernel 源码 | `crates/kernel/src/`（engine.rs / config.rs / endpoint.rs / storage/sqlite.rs） |
| proto | `crates/proto/src/`（frame.rs / record.rs） |
| onebot 适配器 | `crates/adapters/onebot11/src/`（event.rs / link.rs / outbound.rs） |
| dsh harness 驱动 | `harnesses/dsh/src/driver.mjs`（+ dsh-client.mjs / mcp-server.mjs / mcp-http-server.mjs） |
| opencode harness | `harnesses/opencode/plugin/`（acb-wire.mjs / index.js） |
| 架构文档 | `docs/adr/`（0017 多 agent 模型、0018 会话映射、0019 并行会话等） |
| 起停脚本 | `deploy/acb-up.sh`（起）/ `acb-down.sh`（停，带备份）/ `start-drivers.sh` |
| dsh 部署层 overlay | `harnesses/dsh/dsh-mcp.patch.yml`（权威副本，原在 `~/qqab-scratch/dsh-live/web-acb-prod-v2.patch.yml`） |
| dsh 的 API key | `deploy/acb.env`（**不入库**；模板 `deploy/acb.env.example`） |
| 日志 | `~/qqab-scratch/logs/{dsh,bridge,drivers}.log`（scratch **不持久**，重启即清，别把东西存那儿） |

---

## 5. 怎么起停（真机）

**一条命令：`deploy/acb-up.sh` 起，`deploy/acb-down.sh` 停。**

⚠️ **持久性边界（2026-10-30 确认）：** 根子卷 `/root` 重启即清空。持久的只有 `@persistent` 子卷下的
`~/Documents`（本仓库）、`~/.dsh`、`~/.workbuddy`。**`~/qqab-scratch` 整个目录重启后不存在**——所以
起停脚本、dsh overlay、密钥都放仓库 `deploy/`，scratch 只留日志。`~/qqab-scratch/acb-up.sh` 现在只是
指向仓库的符号链接，重启后就没了，别把它当唯一入口。
（down 会先自动备份库三件套，再按「驱动 → 桥」顺序停；up 按「桥 → 驱动」顺序起，并打印桥自述状态。）

**为什么不用原来的 `run_in_background` 托管（重要，别再退回去）：**
2026-10-30 21:2x 实测踩过——桥和驱动挂在 AI 会话的沙箱里跑，会话一关，进程**还活着但开不出新
socket**：桥报 `连不上 127.0.0.1:53001：Function not implemented (os error 38)`，驱动报
`connect ENOSYS 127.0.0.1:7700`。表现是「进程都在、群里却没人理」，最容易误判成模型不发言。
现在两个进程挂在**用户级 systemd**（`acb-bridge.service` / `acb-drivers.service`，`Restart=always`），
不依赖任何会话，掉线自愈。

**手工操作（等价）：**
```bash
export XDG_RUNTIME_DIR=/run/user/1000
systemctl --user status acb-bridge acb-drivers      # 看状态
systemctl --user restart acb-drivers                # 只重启驱动（改了 driver 代码时用这个）
systemctl --user stop acb-drivers acb-bridge        # 停（顺序：驱动先、桥后）
```
注意：这两个是 `systemd-run` 起的**临时单元**，重启电脑/注销后消失，再跑一次 `acb-up.sh` 即可
（想开机自启，得写成 `~/.config/systemd/user/` 下的真单元文件，目前没做）。

**坑（务必记住）：**
- 判断「活没活」别只看 `ps`——要看 `ss -tn | grep 7700` 有没有 ESTAB（第三步最容易漏，往往是真因）。
- 桥和驱动**必须同批次**（协议版本严格相等），只换一边必 handshake fail。
- 排「它不回话」三步：① 库里归档了吗、未读多少；② harness 会话有没有新 user/assistant；
  ③ `ss -tn | grep 7700` 有没有 ESTAB。
- dsh 的 launch token 每次重启都变、只打在启动它的终端里。**新版 `deploy/start-drivers.sh` 根本不传
  `DSH_TOKEN`**：驱动走离线铸造 cookie（读 `~/.dsh/.credentials.yaml` 的 `client-connection/browser-session`
  密钥），`ensureAuth` 里铸造优先，所以重启后自动跟上，不需要人类抄 token。
- 会话令牌也不再写死在脚本里：`deploy/start-drivers.sh` 从 `config.toml` 的 `[[sessions]]` 块里读，
  改配置不用同步改脚本（两边一漂就握手失败）。
- opencode 会卸载插件却不退出进程，桥对断连零日志，只能靠上面第③步发现。

---

## 6. 已知边界 / 未完成（下一步可做）

1. **poke 事件昵称兜底**：OneBot 的 notice/poke 事件常不带 sender 字段，昵称显示成 QQ 号。需要 `get_group_member_info` 异步查 API 兜底。**注**：现在有 `platform_call` 了，但那是给模型用的，适配器内部兜底还得自己写（未做）。
2. **dsh 重启后旧 sessionId 失效的自愈**：seed 的旧 sessionId 若在 dsh 重启后失效，prompt 会失败。需区分「session 失效」vs「模型失败」做自愈兜底（留待观察）。
3. **git 仓库有个异常的 `~/` 目录**（误创建的），里面是 qqab-memories，待清理（不是关键）。
4. ~~平台能力只有 4 个纯读方法~~（已完成）：现在 52 个，覆盖「人在这号上能做的事」，
   全部对着真机 NapCat 探测过。表在 `crates/adapters/onebot11/src/catalog.rs`。
5. ~~破坏性方法的配置开关~~（已完成）：`[[connections]] allow_destructive = true / ["group.kick"] / 省略`。
   真机现在开的是 `true`。
6. **待验证**：真实消息触发时，模型是否会自己调 `message.forward_content` 把转发正文读出来，
   以及平台特有方法（`platform_call`）这条通路在**模型手里**用不用得顺。
   （方法名/参数名已在真机上逐个探测过，桥侧校验与路由有测试覆盖；差的是「模型真的会去调」这一步。）
7. **换 dsh 模型必须调 `session/selectModel`**（踩过一次）：dsh 把「上次用的模型」记在会话里
   （`modelSelection.lastUsed`），复用老会话时它**优先于** `agent-default-model`。只改补丁里的默认
   模型，老会话会一声不响继续打旧渠道——网关日志里还是旧模型，桥这边毫无报错。
   现在 `deploy/start-drivers.sh` 从补丁读 `provider/model/reasoningEffort` 传给驱动，
   驱动在每轮开始前对会话 `selectModel` 一次（`TurnDriver#pinModel`，失败只 warn 不阻断）。
   手工改单个会话：`node harnesses/dsh/src/dsh-client.mjs` 里没有 CLI，用下面这段：
   `dsh.selectModel(sessionId, { provider, model, reasoningEffort })`（`DshClient` 的方法）。
   —— 顺带说明：`agent-default-model` 只影响**新建**的会话。

---

## 7. 工作方式约定（用户硬性要求，务必遵守）

- 动手前先给 ≤10 行的「现状→目标」差距表。
- ADR 必须标「来源：用户裁定 / 来源：实现默认」，只有用户真说过的话才标用户裁定；正文用大白话。
- 汇报要具体：改了什么、验证命令与输出、还剩什么；**验证必须真跑，E2E 优先**。
- 不确定就停下来问，给 2 个选项和后果。
- 提交前给 `git diff --stat`；推送须用户明示。
- **用户不懂技术细节，不要拿术语堆三页。**

---

## 8. 重要技术教训（本 session 沉淀）

1. **TOML 顶层键位置**：顶层键必须放在任何 `[table]`/`[[array]]` 之前，写在 table 段后面的裸键会被静默吞进那个 table。加配置项后先用 `tomllib`/`toml` 解析一遍确认落点。这类「配置写了但没生效」最隐蔽。
2. **`Option::map + unwrap_or_else` 判断「是否配置」**：空集合也是「有值」≠「配置了」，得用 `filter` 判空。
3. **DIAG 日志是定位神器**：加一行打印「运行时实际解析值」的 eprintln，比逐层读代码快得多。
4. **nix 环境 Node 22 的 http/WS 全废**（node:http 走 undici WASM 报 OOM），测试 dsh 用**系统 Node 24**（`/etc/profiles/per-user/nixos/bin/node`）。
5. **加新 Rust 依赖会撞 mtime**，本地 cargo 解包必失败，直接 `nix build` 验证即可。
6. **别把常驻进程挂在 AI 会话里**：会话沙箱一撤，进程活着但 `connect()` 返回 ENOSYS
   （桥表现为 `os error 38`，驱动表现为 `connect ENOSYS`）。桥/驱动一律用 `systemd-run --user`
   托管，见第 5 节。判断「活没活」看 ESTAB，不看 `ps`。
7. **别信交接文档里写的「现成 token」**：dsh 每次重启换 launch token 且只打在自己终端上。
   验证 dsh 通不通，直接用 `harnesses/dsh/src/dsh-client.mjs` 的 `DshClient.ensureAuth()`
   跑一次（会打印 `minted`/`token`），比猜 token 快。
