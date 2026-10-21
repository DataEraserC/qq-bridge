# QSH

QSH 让 agent 以真实的 QQ 账号身份参与群聊。它由两个互不知道对方内部模型的进程组成：

- **内核**（Rust）—— QQ 侧语义：账号、群、订阅范围、唤醒判定、消息归档、工具执行。
- **适配器**（语言随运行时）—— agent 运行时侧语义：会话、回合、工具声明。当前只有 DSH 适配器，用 TypeScript。

内核不知道会话标识是什么，适配器不知道群号是什么。两者唯一的接触面是本机 Unix socket 上的一份 JSON 协议，见 [协议](docs/architecture/05-protocol.md)。

## 当前状态

**骨架已开始，QQ 侧尚未连通。**

本仓库从空白重建。旧实现是一个 Node 单进程，把 QQ 语义与 DSH 协议混在约一万二千行里，已整体删除。

已实现的部分：

| 位置 | 内容 |
| --- | --- |
| `crates/proto` | 协议帧、错误码、未知类型忽略 |
| `crates/kernel/src/config.rs` | `config.toml` 解析与启动期校验（含 `(群, 账号)` 冲突检查） |
| `crates/kernel/src/wake.rs` | 唤醒判定链，含忽略名单与休眠期 |

尚未实现：OneBot 连接、归档、队列、投递、工具面、钩子宿主、笔记。

```sh
cargo test                      # 全部测试
cargo clippy --all-targets -- -D warnings
```

`crates/kernel/tests/fixtures/documented.toml` 是文档里配置示例的合集，由测试直接解析——文档与实现漂移时这里会失败。

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
