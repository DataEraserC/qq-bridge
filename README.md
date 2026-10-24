# qq-agent-bridge

qq-agent-bridge 让 agent 以真实的 QQ 账号身份参与群聊。它由两个互不知道对方内部模型的进程组成：

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
| `crates/kernel/src/turn.rs` | 按记忆名的串行队列、回合票签发与作废、积压合并、超时清扫 |
| `crates/kernel/src/notes.rs` | 笔记存储（三类记录、覆盖写、淘汰、摘要渲染、JSON 往返） |
| `crates/kernel/src/archive.rs` | 按记忆名分片的归档（按群的未读水位、去重、条数与字节双限、投递文本渲染、JSON 往返） |
| `crates/kernel/src/deliver.rs` | 投递组装：未读 + 上下文渲染成内容块、拍一拍判定、人格与笔记摘要、水位建议 |
| `crates/kernel/src/pipeline.rs` | 事件管线：归档 → 判定 → 入队 → 投递，积压合并与唤醒原因保留 |

尚未实现：OneBot 连接、socket 传输与重试、工具面、钩子宿主。

```sh
cargo test                      # 全部测试
cargo clippy --all-targets -- -D warnings
```

`crates/kernel/tests/fixtures/documented.toml` 是文档里配置示例的合集，由测试直接解析；`crates/kernel/tests/documented_notes.rs` 与 `crates/kernel/tests/documented_archive.rs` 分别直接读 `docs/architecture/08-notes.md` 和 `docs/architecture/03-kernel.md` 抽代码块与上限表——文档与实现漂移时这几处会失败。

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
