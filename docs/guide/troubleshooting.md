# 排障手册

## 第一步：确认四个东西都活着

```sh
export XDG_RUNTIME_DIR=/run/user/$(id -u)
systemctl --user is-active dsh-acb.service acb-bridge.service acb-drivers.service
ss -tln | grep -E "7700|3080|3100|3131"
```

三个都要 `active`，端口都要在监听。重启电脑后要重跑 `deploy/acb-up.sh`。

::: warning 别用 ps 判断
进程存在 ≠ 活着。挂在终端/AI 会话里的常驻进程，会话一撤后 socket 全废
（报 `os error 38` / `ENOSYS`），表现就是「进程都在、群里没人理」。
判活看 `ss -tn` 里的 **ESTAB 连接**：驱动连桥 7700、驱动连 dsh 3080，各至少一条。
:::

## 「群里不回话」三步定位

按顺序查，哪步断了修哪步：

1. **看控制台**（3100）：连接在线吗？消息记进流水了吗（概览/会话页的未读数动没动）？
2. **看 harness 侧**：dsh 里对应的会话有没有新的 user/assistant 消息？
   没有就是桥→harness 这段断了；有但没回，是 harness/模型侧的问题；
3. **看连接**：`ss -tn | grep 7700` 有没有 ESTAB。dsh 换了 launch token 后驱动会自动
   重新铸 cookie，一般不用管；实在不行 `deploy/acb-up.sh` 整链重拉。

## 常见报错速查

| 报错 | 原因 | 处理 |
|---|---|---|
| `session/create 失败（session/conflict）：belongs to ...` | dsh 里已有同 id 会话但工作目录不同（比如改过 `ACB_CWD`） | 驱动会自动换新 id 重试，可忽略；要归组的话在控制台断开重置一次映射 |
| `session.create accepts workspaceId or cwd, not both` | 旧版驱动的 bug（两个参数同时传） | 升级驱动后重启 |
| `fetch failed`（发图时） | 平台（OneBot 实现的容器）去下载图片 URL 失败：`public_base` 填了平台访问不到的地址 | 按[文件中继配置](./config#文件中继-发图发文件的关键)核对 `public_base`，用验证命令从容器里 curl 一次 |
| `第 N 段（image）缺 url` | 消息段的 `url` 字段缺失 | 桥的报错会提示认哪些别名（`file`/`path`/`src` 也认）；给本机路径即可自动中继 |
| 攒了一堆旧消息反复重投 | 老版本的水位 bug（自己发的消息在别的订阅者账上留洞） | 升级到 2026-10-31 之后的版本，开轮时自动自愈，不用修库 |
| `未分组`（dsh 网页里会话不进工作区） | 旧版建会话只传了 cwd，dsh 不按 cwd 分组 | 控制台断开重置一次映射，新会话会归到「ACB」工作区 |

## 数据安全

- 一切状态在 `state/acb.db`（含 `-wal`/`-shm` 两个伴生文件）。**备份就是拷这三个文件**，
  `deploy/acb-down.sh` 停的时候会自动备份；
- 换版本/改库结构前先备份；`config.toml` 被控制台改坏时，旁边有 `.bak` 可以回滚；
- 删库 = agent 失忆（流水、水位、笔记账目全清），但桥不会起不来，重建即可。

## 还是不行

拿上这几样去提 issue：桥日志（`journalctl --user -u acb-bridge.service`）、
驱动日志（`~/qqab-scratch/logs/`）、控制台会话页截图、出问题时你在群里发的原话。
