# ADR-0001 核心用 Rust 写

来源：实现默认，可以随时推翻，不需要用户同意。

## 这条决定了什么行为

桥的核心和适配器用 Rust 写，按 workspace 分三个 crate：`proto`（桥与 harness 的协议）、`kernel`（核心）、`adapters/*`（各平台适配器）。

## 不这样会怎样

换成 Python/Node，写起来快，但常驻进程的内存占用、依赖管理、并发控制都要多操心；旧 Node 实现 21249 行的失控已经演示过一遍。换成 Go 也行，但旧 Rust 实现（327 个测试全绿）证明这条路人走得通，测试资产虽然不搬，经验是现成的。

## 技术细节

edition 2024、resolver 3。`proto` 独立成 crate，让 harness 侧的接入库可以直接复用帧定义。
