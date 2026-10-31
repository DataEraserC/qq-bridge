---
layout: home

hero:
  name: acb-bridge
  text: 把 AI agent 接进聊天平台的桥
  tagline: 多 harness × 多平台 —— 让你在 QQ 群里、私聊里，跟自己的 agent 说话
  actions:
    - theme: brand
      text: 使用指南
      link: /guide/
    - theme: alt
      text: 快速开始
      link: /guide/getting-started
    - theme: alt
      text: 开发者文档
      link: /03-architecture

features:
  - icon: 🔌
    title: 收发翻译
    details: 平台消息和 agent 工具调用互转，内容段统一抽象，平台做不了会明确报错而不是悄悄丢。
  - icon: ⏰
    title: 何时叫醒
    details: 被 @、被回复、被叫名字、被戳一戳、攒够 N 条——每个房间单独配，私聊和群可以完全不同的策略。
  - icon: 📒
    title: 记了什么
    details: 所有消息进流水（SQLite），读没读有水位记账；长期记忆是 agent 自己维护的 git 仓库。
  - icon: 🛡️
    title: 能发到哪
    details: agent 只能往订阅过的房间发消息；破坏性平台操作（踢人禁言）默认全关，由你显式放行。
  - icon: 🖥️
    title: 网页控制台
    details: 连接/会话/人格/记忆增删改查、断开映射重置会话、热重载——日常管理不用碰配置文件。
  - icon: 🧩
    title: 大脑在外面
    details: 桥不替 agent 思考。DSH、WorkBuddy、Trae、Codex……通过统一协议接入，各用各的模型和工具。
---
