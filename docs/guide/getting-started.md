# 快速开始

从零到「在 QQ 群里跟 agent 说上话」，大概四步：**准备平台 → 写配置 → 构建启动 → 验证**。

## 你需要准备什么

- 一台 Linux 机器（NixOS 直接用本仓库的 flake；其他发行版装 nix 也能用）；
- 一个聊天平台的账号给 agent 用（本文以 QQ 为例，需要再跑一个 OneBot 实现，如 NapCat / Lagrange / snowluma）；
- agent 侧的运行时（harness），比如 [DSH](https://dsh)。桥不主动连 harness，是 harness 拿着令牌来连桥。

## 第一步：写配置

```sh
cp config.example.toml config.toml
```

照着里面的注释改，重点就几处（每项的详细解释见[配置手册](./config)）：

```toml
[endpoint]
listen = "127.0.0.1:7700"        # harness 连桥的地址

[console]
listen = "127.0.0.1:3100"        # 网页控制台，强烈建议开
token = "换成一长串你自己的随机字符串"

[storage]
path = "state/acb.db"            # 消息流水，备份就是拷这个文件

[file_relay]
listen = "0.0.0.0:3131"          # 发图/发文件用的中继，见「日常使用」
public_base = "http://127.0.0.1:3131"   # 平台那边能访问到桥的地址

[[connections]]                  # 你的 QQ 号
id = "qq"
kind = "onebot11"
transport = "ws-client"
url = "ws://127.0.0.1:3001"      # OneBot 实现的 WS 地址
account = "10001"                # 机器人 QQ 号

[[personas]]                     # 人设
id = "default"
name = "默认人格"
text = "你是谁、什么语气、什么不能说，写这里"

[[harnesses]]
id = "dsh"
name = "DeepSeek"

[[sessions]]                     # 会话：把上面的都串起来
id = "main"
harness = "dsh"
persona = "default"
token = "换成一长串你自己的随机字符串"

[[sessions.subscriptions]]
connection = "qq"
room = "group/123456789"

[sessions.subscriptions.wake]    # 怎么叫醒它
at_mention = true
reply = true
poke = true
```

## 第二步：构建

```sh
nix build -L .#acb-bridge
```

产出在 `result/bin/acb-bridge`，测试会顺带全跑一遍（全绿才出结果）。

## 第三步：启动

```sh
deploy/acb-up.sh     # 按顺序起 dsh → 桥 → 驱动，全部挂 systemd 用户服务
deploy/acb-down.sh   # 停（会先备份数据库）
```

为什么挂 systemd 而不是 `nohup`：桥和驱动是常驻进程，挂在终端/AI 会话里，会话一撤
进程活着却开不出新连接，表现为「进程都在、群里没人理」。systemd 用户服务没这个问题。
重启电脑后重新跑一次 `deploy/acb-up.sh` 即可。

## 第四步：验证

1. 浏览器打开 `http://127.0.0.1:3100/?token=<你的控制台令牌>` ——概览页能看到连接在线、会话已绑定；
2. 在订阅的群里 @ 它说句话，正常应答就是全通了。

## 卡住了

去[排障手册](./troubleshooting)，最常见的问题和三步定位法都在那里。
