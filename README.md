# QSH

QSH 让 agent 以真实的 QQ 账号身份参与群聊。它由两个互不知道对方内部模型的进程组成：

- **内核**（Rust）—— QQ 侧语义：账号、群、订阅范围、唤醒判定、消息归档、工具执行。
- **适配器**（语言随运行时）—— agent 运行时侧语义：会话、回合、工具声明。当前只有 DSH 适配器，用 TypeScript。

内核不知道会话标识是什么，适配器不知道群号是什么。两者唯一的接触面是本机 Unix socket 上的一份 JSON 协议，见 [协议](docs/architecture/05-protocol.md)。

## 当前状态

**只有文档，没有代码。**

本仓库从空白重建。旧实现是一个 Node 单进程，把 QQ 语义与 DSH 协议混在约一万二千行里，已整体删除；新实现的架构在 [docs/architecture/](docs/architecture/README.md) 中定义，编码尚未开始。因此这套文档描述的是**将要建成的系统**，不是现有系统。

## 文档

| 位置 | 读者 | 内容 |
| --- | --- | --- |
| [GLOSSARY.md](GLOSSARY.md) | 所有人 | 术语表。改设计前先读。 |
| [docs/architecture/](docs/architecture/README.md) | 实现者、维护者 | 分层、领域模型、协议、不变量 |
| [docs/adr/](docs/adr/README.md) | 实现者、维护者 | 已定型的决策与理由 |
| [docs/manual/](docs/manual/README.md) | 使用者 | 怎么配置账号、设备、范围与唤醒规则 |
| [docs/en/](docs/en/README.md) | — | 英文文档预留位，当前为空 |

## 许可

MIT，见 [LICENSE](LICENSE)。
