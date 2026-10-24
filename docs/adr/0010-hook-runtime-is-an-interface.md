# 钩子运行时是接口，不只是 Lua

`hooks.rs` 把 `mlua::Lua` 直接写在结构体里，三个挂载点的参数是 Lua 表，超时靠 Lua 的 VM 指令钩子。这让「换一种语言写钩子」不只是不支持，而是**结构上不可能**。

## 决定

抽出 `HookRuntime` trait：

```rust
trait HookRuntime {
    fn device(&self) -> &str;
    fn pre_receive(&mut self, event: &HookEvent<'_>) -> Ruling;
    fn post_receive(&mut self, event: &HookEvent<'_>);
    fn should_wake(&mut self, event: &HookEvent<'_>, tags: &[String], reason: Option<&str>)
        -> Result<Option<String>, HookError>;
    fn drain_logs(&self) -> Vec<HookLog>;
}
```

`Pipeline` 里存 `BTreeMap<String, Box<dyn HookRuntime>>`。三处调用点从 `hooks.method(...)` 改成 `HookRuntime::method(hooks.as_mut(), ...)`——只有这一处语法变化，其余一行不改。

现有两种实现：

| 实现 | 隔离靠 | 超时靠 |
| --- | --- | --- |
| `Hooks`（Lua，`hooks.rs`） | 不加载 IO 库 + 只读代理 | VM 指令钩子打断调用 |
| `ProcessHooks`（任何语言，`hook_process.rs`） | 进程边界 | `kill` 并重起 |

## 这个接口不加 `Send`

`mlua::Lua` 不是 `Send`。要求 `Send` 会立刻把 Lua 实现排除在外，而那**不是**真正的约束：ADR-0006 的阻塞模型决定了钩子全部在主循环线程上同步执行，没有第二个线程会碰它。

加 `Send` 只会得到「接口看起来能跨线程，实际没人跨」——一个假的保证比没有保证更糟。

## 外进程必须常驻，不能一调用一起

实测（本机 `python3`）：

| 方式 | 单次成本 |
| --- | --- |
| 每次调用起进程 | **12.4 ms** |
| 常驻子进程，逐行 IPC | **0.031 ms** |

差 400 倍。钩子是每条消息都要过的路径，一次一起等于把吞吐压到每秒几十条。所以 `ProcessHooks` 起一个进程，之后只读写两对管道。

## 两种隔离模型不能共用一个策略

这是这条决策最值得记的部分：Lua 能「打断一次调用而不丢状态」，进程不能。`ProcessHooks` 超时之后必须 `kill`，而 `kill` 掉的进程**不能复用**——下一次调用要重起。

所以「超时」在两个实现里的后果不同：

| | 超时后果 |
| --- | --- |
| Lua | 这一次调用失败，环境仍在，下一次照常 |
| 外进程 | 这一次调用失败，进程没了，下一次重起（付 12.4ms） |

接口能容纳这个差异，是因为它只承诺「这次调用成功或失败」，不承诺「失败之后状态如何」。把「超时可恢复」写进接口才是错的。

## 考虑过的方案

**只抽象「脚本语言」，不支持任意进程。** 那样接口会写成 `fn run(&mut self, source: &str, ...)`，WASM 与进程都塞不进去。而「native 钩子」的诉求恰好是「用任何语言」，不是「用另一种脚本语言」。

**全部走外进程，删掉 Lua。** 每条消息加一次 IPC，且 Lua 版已有的 24 个测试全废。保留两种实现、用 trait 分开，是唯一同时满足两边的做法。

**接口返回 `Box<dyn Any>` 让调用方自己解释。** 那等于没有接口——每个调用点都要 downcast。三个挂载点的输入输出是稳定的，做成具体类型没有代价。

## 后果

- `HookRuntime` 不是 `Send`，所以 `Kernel` 不是 `Send`。这是既成事实（`Pipeline` 里一直是 `Hooks`），这条决策只是把它写明白。
- `ProcessHooks::drain_logs` 返回副本而不是取走——trait 的 `drain_logs` 签名是 `&self`（Lua 版用 `RefCell` 才能取走）。外进程实现的日志量很小，克隆可以接受；要真正取走需要 `&mut self`，而那会让 Lua 版多一层可变借用。
- 外进程的 stderr 直通父进程，不进钩子日志。它的格式内核控制不了，混进结构化日志只会更难读。
- 外进程协议是「一行一个 JSON」，必须每条 flush。写错了的表现是「钩子永远不返回」——所以协议写进了 `ProcessHooks::SKILL` 常量而不是只放在文档里。
