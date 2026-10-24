//! 运行时：把连接、管线与适配器接入串成一个进程。
//!
//! 这一层只有三件事：**把事件喂给管线、把管线的结论变成出站帧、把回合的收尾接回水位**。
//! 它不做 I/O——出站是 [`Outbound`]，由 `main` 真正写出去。所以整个状态机可以在没有网络、
//! 没有 socket 的测试里跑完一轮完整的「收消息 → 投递 → 工具调用 → 回合结束」。
//!
//! 三条约束在这里落地：
//!
//! - **投递成功才推进水位**（见 `docs/architecture/03-kernel.md#队列语义`）。`deliver_ok`
//!   到达前，那一批消息一直是未读；失败就重投，拿到的是同一批。
//! - **进行中的回合只有一份上下文。** 回合票是身份来源，工具调用靠 `(记忆名, 票)` 取回
//!   这一轮的身份，不从别处推断。
//! - **模型只能发到自己这一轮的范围。** 发送账号来自回合上下文里的 `send_accounts`，不是
//!   配置里的全部账号，也不是模型给的参数。
//!
//! 适配器不在线时投递不外发到别处，也不丢：作废这一轮的票、留一个重试点，由 [`Inbound::Tick`]
//! 重投。归档里的未读一直在。

use std::collections::BTreeMap;
use std::sync::mpsc::{Receiver, Sender, TryRecvError};

use serde_json::Value;
use subtle::ConstantTimeEq;
use time::{Duration, OffsetDateTime};

use qqab_proto::{
    DeliverFailed, DeliverOk, ErrorBody, ErrorCode, Frame, Hello, HelloOk, HelloReject,
    PROTOCOL_VERSION, Ping, Pong, ToolCall, ToolResult, TurnEnd,
};

use crate::adapter::AdapterEvent;
use crate::chat::{ChatAction, ChatEvent, ChatMessage, ChatPoke, ChatStatus};
use crate::config::{Config, Device, ScopeConfig, Secret};
use crate::conn::{Pool, route};
use crate::deliver::Delivery;
use crate::persona::Personas;
use crate::pipeline::{Action as PipelineAction, Pipeline};
use crate::transport::{ActionOutcome, ActionRequest, Incoming, WriteError};
use crate::turn::Ticket;

/// 重投间隔。固定值：适配器不在线只有「还没起来」与「崩了」两种，指数退避在这里没有
/// 额外收益，而固定间隔让日志可读。
pub const REDELIVER_AFTER: Duration = Duration::seconds(5);

/// 进入运行时的输入。
pub enum Inbound {
    /// 某条 QQ 连接上发生的事。
    Qq { account: String, incoming: Incoming },
    /// 某条连接建好了，可以发动作。
    LinkUp {
        account: String,
        actions: Sender<ActionRequest>,
    },
    /// 适配器侧发生的事。
    Adapter(AdapterEvent),
    /// 时钟推进：收在途动作的回执、清扫超时回合、重投。
    Tick,
}

/// 运行时要求做的事。
pub enum Outbound {
    /// 给某条适配器连接写一行。
    Frame { conn: u64, line: String },
    /// 在某个账号的连接上发一个动作，结果回 `request.reply`。
    Act {
        account: String,
        request: ActionRequest,
    },
    /// 断开某条适配器连接（协议协商失败时）。
    CloseAdapter { conn: u64 },
    /// 日志。
    Log {
        level: &'static str,
        message: String,
    },
}

/// 一个在途动作：发起时留下回执通道，`Tick` 时收。
struct PendingAction {
    conn: u64,
    /// `tool_call` 的请求标识，回 `tool_result` 时原样带回。
    id: String,
    reply: Receiver<Result<ActionOutcome, WriteError>>,
}

/// 待重投。
///
/// 记住目标设备与群，而不是到重投时再去配置里找一个——一台设备可以订阅多个群，按「第一个
/// scope」猜会把消息投到错误的群里。
struct Retry {
    attempts: u32,
    due: OffsetDateTime,
    device: String,
    group: String,
}

/// 回合身份：从票里取回来的那几项。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnContext {
    pub memory: String,
    pub device: String,
    pub group: String,
    pub send_accounts: Vec<String>,
}

impl From<crate::turn::TurnContext> for TurnContext {
    fn from(context: crate::turn::TurnContext) -> Self {
        Self {
            memory: context.memory,
            device: context.device,
            group: context.group,
            send_accounts: context.send_accounts,
        }
    }
}

/// 内核运行时的全部状态。
pub struct Kernel {
    config: Config,
    personas: Personas,
    pipeline: Pipeline,
    pool: Pool,
    /// 各账号的动作入队口。连接断开时移除。
    actions: BTreeMap<String, Sender<ActionRequest>>,
    /// 适配器连接与其写出通道。
    adapters: BTreeMap<u64, Sender<String>>,
    /// 已投递、尚未 `deliver_ok` 的那一批。键是 `deliver` 的请求标识。
    awaiting: BTreeMap<String, Delivery>,
    /// 进行中的回合身份。键是记忆名——一个记忆名同时只有一个回合。
    contexts: BTreeMap<String, TurnContext>,
    pending_actions: Vec<PendingAction>,
    retries: BTreeMap<String, Retry>,
    frame_seq: u64,
    echo_seq: u64,
    /// 概率唤醒的确定性随机源。可重放比真随机重要——排查「它为什么说话」时能复现。
    rng: u64,
}

impl Kernel {
    pub fn new(config: Config, personas: Personas, pipeline: Pipeline, pool: Pool) -> Self {
        Self {
            config,
            personas,
            pipeline,
            pool,
            actions: BTreeMap::new(),
            adapters: BTreeMap::new(),
            awaiting: BTreeMap::new(),
            contexts: BTreeMap::new(),
            pending_actions: Vec::new(),
            retries: BTreeMap::new(),
            frame_seq: 0,
            echo_seq: 0,
            rng: 0x9E37_79B9_7F4A_7C15,
        }
    }

    /// 处理一条输入，返回要做的出站动作。
    ///
    /// 时间由调用方传入：同一条事件流在不同时刻重放会得到同一份结论。
    pub fn handle(&mut self, inbound: Inbound, now: OffsetDateTime) -> Vec<Outbound> {
        let mut out = match inbound {
            Inbound::Qq { account, incoming } => self.on_qq(&account, incoming, now),
            Inbound::LinkUp { account, actions } => {
                self.actions.insert(account.clone(), actions);
                self.pool.ready(&account);
                vec![Outbound::Log {
                    level: "info",
                    message: format!("账号 {account} 已连接"),
                }]
            }
            Inbound::Adapter(event) => self.on_adapter(event, now),
            Inbound::Tick => self.on_tick(now),
        };
        // 处理一条输入后统一收钩子日志：脚本的 `log()` 与内核日志走同一条出口。
        out.extend(self.drain_hook_logs());
        out
    }

    /// 执行内核自己能做的出站动作：写适配器、断开连接。
    ///
    /// 返回仍需外部执行的部分——QQ 动作要交给某条连接，日志要写到 stderr。分成两段是为了
    /// 让测试只看出站，不必提供 socket。
    pub fn apply(&mut self, out: Vec<Outbound>) -> Vec<Outbound> {
        let mut leftover = Vec::new();
        for item in out {
            match item {
                Outbound::Frame { conn, line } => {
                    match self.adapters.get(&conn) {
                        Some(writer) if writer.send(line).is_ok() => {}
                        // 写不进去说明连接已经不在了：就地清掉，别留一个死把手。
                        _ => {
                            self.adapters.remove(&conn);
                            leftover.push(Outbound::Log {
                                level: "warn",
                                message: format!("适配器连接 {conn} 已不可写，移除"),
                            });
                        }
                    }
                }
                Outbound::CloseAdapter { conn } => {
                    self.adapters.remove(&conn);
                }
                other => leftover.push(other),
            }
        }
        leftover
    }

    /// 当前在线的适配器连接数。诊断用。
    pub fn adapter_count(&self) -> usize {
        self.adapters.len()
    }

    /// 供测试与诊断读取。
    pub fn pipeline(&self) -> &Pipeline {
        &self.pipeline
    }

    /// 把钩子攒下的日志变成出站日志。
    ///
    /// 钩子在里面没有日志出口——它们只写进缓冲区，由这里统一交给主循环写 stderr。
    /// 这样脚本的 `log()` 与内核对日志的口径一致，也不必让 Lua 环境握住一个 IO 句柄。
    pub fn drain_hook_logs(&mut self) -> Vec<Outbound> {
        let mut out = Vec::new();
        for (device, hooks) in self.pipeline.hooks_mut() {
            for log in hooks.drain_logs() {
                out.push(Outbound::Log {
                    level: hook_level(&log.level),
                    message: format!("钩子[{device}]：{}", log.message),
                });
            }
        }
        out
    }

    pub fn pool(&self) -> &Pool {
        &self.pool
    }

    // ---- QQ 侧 ----

    fn on_qq(&mut self, account: &str, incoming: Incoming, now: OffsetDateTime) -> Vec<Outbound> {
        match incoming {
            Incoming::Event(event) => self.on_event(account, *event, now),
            Incoming::Undecodable { reason } => vec![Outbound::Log {
                level: "warn",
                message: format!("{account}：丢弃无法解码的帧（{reason}）"),
            }],
            // 读超时是轮询节奏，不是事件。
            Incoming::Timeout => Vec::new(),
            Incoming::Closed { reason } => {
                self.actions.remove(account);
                self.pool.failed(account, now);
                vec![Outbound::Log {
                    level: "warn",
                    message: format!("账号 {account} 连接结束：{reason}"),
                }]
            }
        }
    }

    fn on_event(&mut self, account: &str, event: ChatEvent, now: OffsetDateTime) -> Vec<Outbound> {
        match event {
            ChatEvent::Message(message) => self.on_message(account, &message, now),
            ChatEvent::Poke(poke) => self.on_poke(account, &poke, now),
            ChatEvent::Status(ChatStatus::Ready) => {
                self.pool.ready(account);
                Vec::new()
            }
            // 平台说本账号不在线：就当这次连接没成，交给退避重连。
            ChatEvent::Status(ChatStatus::Offline) => {
                self.pool.failed(account, now);
                Vec::new()
            }
            // 平台扩展事件：不是错误，什么都不用做。
            ChatEvent::Ignored { .. } => Vec::new(),
        }
    }

    fn on_message(
        &mut self,
        account: &str,
        message: &ChatMessage,
        now: OffsetDateTime,
    ) -> Vec<Outbound> {
        // 先把路由结果取成自有数据，再动管线：借用 `self.config` 的同时不能改管线。
        let targets: Vec<(Device, ScopeConfig, String, bool)> =
            route(&self.config, message, account)
                .into_iter()
                .map(|found| {
                    (
                        found.device.clone(),
                        found.scope.clone(),
                        found.account,
                        found.own,
                    )
                })
                .collect();

        let mut out = Vec::new();
        for (device, scope, account, own) in targets {
            let mut archived = message.into_archive(&account);
            archived.own = own;

            let persona = self.persona_of(&device.persona);
            let roll = self.next_roll();
            let action = self
                .pipeline
                .ingest(&device, &scope, &persona, archived, now, roll);
            out.extend(self.after_action(&device, &scope, action, now));
        }
        out
    }

    fn on_poke(&mut self, account: &str, poke: &ChatPoke, now: OffsetDateTime) -> Vec<Outbound> {
        // 只有拍自己的那些是给这台设备的信号。
        if !poke.targets_self() {
            return Vec::new();
        }
        let Some(group) = poke.group.as_deref() else {
            return Vec::new();
        };

        let devices = self.config.devices.clone();
        let mut out = Vec::new();
        let roll = self.next_roll();
        for device in &devices {
            for scope in &device.scopes {
                if scope.group != group {
                    continue;
                }
                if !scope.receive.iter().any(|candidate| candidate == account) {
                    continue;
                }
                // 自己拍自己不算信号。
                if self.device_owns(device, &poke.sender_id) {
                    continue;
                }
                let persona = self.persona_of(&device.persona);
                let action = self.pipeline.poke(
                    device,
                    scope,
                    &persona,
                    Some(poke.sender_id.as_str()),
                    now,
                    roll,
                );
                out.extend(self.after_action(device, scope, action, now));
            }
        }
        out
    }

    /// 管线给出结论后，把结论变成出站动作。
    fn after_action(
        &mut self,
        device: &Device,
        scope: &ScopeConfig,
        action: PipelineAction,
        now: OffsetDateTime,
    ) -> Vec<Outbound> {
        match action {
            PipelineAction::Deliver(delivery) => self.send_delivery(*delivery, device, scope, now),
            // 积压已记在管线里，回合结束后合并投递。
            PipelineAction::Defer | PipelineAction::Stay => Vec::new(),
        }
    }

    fn send_delivery(
        &mut self,
        delivery: Delivery,
        device: &Device,
        scope: &ScopeConfig,
        now: OffsetDateTime,
    ) -> Vec<Outbound> {
        let Some(conn) = self.adapters.keys().next().copied() else {
            // 没有适配器：作废这一轮的票、留重试点。消息留在归档里，什么都不丢。
            self.pipeline
                .abandon(&delivery.memory, &delivery.ticket, now);
            let seconds = REDELIVER_AFTER.whole_seconds();
            self.schedule_retry(&delivery.memory, &device.id, &scope.group, now);
            return vec![Outbound::Log {
                level: "warn",
                message: format!(
                    "记忆 {} 有未读，但没有适配器在线：{seconds} 秒后重投",
                    delivery.memory
                ),
            }];
        };

        self.frame_seq += 1;
        let id = format!("d-{}", self.frame_seq);
        self.contexts.insert(
            delivery.memory.clone(),
            TurnContext {
                memory: delivery.memory.clone(),
                device: device.id.clone(),
                group: scope.group.clone(),
                send_accounts: scope.send.clone(),
            },
        );

        let frame = delivery.clone().into_frame(id.clone());
        self.awaiting.insert(id.clone(), delivery);

        match crate::adapter::line_for(&Frame::Deliver(frame)) {
            Ok(line) => vec![Outbound::Frame { conn, line }],
            Err(reason) => {
                self.awaiting.remove(&id);
                vec![Outbound::Log {
                    level: "warn",
                    message: format!("投递帧编码失败：{reason}"),
                }]
            }
        }
    }

    // ---- 适配器侧 ----

    fn on_adapter(&mut self, event: AdapterEvent, now: OffsetDateTime) -> Vec<Outbound> {
        match event {
            AdapterEvent::Connected { conn, out, peer } => {
                self.adapters.insert(conn, out);
                // 适配器回来了：把刚才投不出去的那些立刻重投一遍。
                let due: Vec<String> = self.retries.keys().cloned().collect();
                let mut actions = vec![Outbound::Log {
                    level: "info",
                    message: format!("适配器已连接（{peer}）"),
                }];
                for memory in due {
                    actions.extend(self.redeliver(&memory, now));
                }
                actions
            }
            AdapterEvent::Frame { conn, frame } => self.on_frame(conn, frame, now),
            AdapterEvent::Undecodable { conn, reason } => vec![Outbound::Log {
                level: "warn",
                message: format!("适配器连接 {conn}：丢弃无法解码的帧（{reason}）"),
            }],
            AdapterEvent::Closed { conn, reason } => {
                self.adapters.remove(&conn);

                // 连接没了，在途动作的回执不会再回来：各自失败掉，别让模型等着。
                let drained = std::mem::take(&mut self.pending_actions);
                let mut out = Vec::new();
                let mut keep = Vec::new();
                for pending in drained {
                    if pending.conn == conn {
                        out.extend(fail_tool(
                            conn,
                            &pending.id,
                            ErrorCode::Internal,
                            "适配器连接已断开",
                        ));
                    } else {
                        keep.push(pending);
                    }
                }
                self.pending_actions = keep;
                out.push(Outbound::Log {
                    level: "warn",
                    message: format!("适配器连接 {conn} 结束：{reason}"),
                });
                out
            }
        }
    }

    fn on_frame(&mut self, conn: u64, frame: Frame, now: OffsetDateTime) -> Vec<Outbound> {
        match frame {
            Frame::Hello(hello) => self.on_hello(conn, hello),
            Frame::DeliverOk(DeliverOk { id }) => self.on_deliver_ok(&id),
            Frame::DeliverFailed(DeliverFailed { id, reason }) => {
                self.on_deliver_failed(&id, &reason, now)
            }
            Frame::TurnEnd(end) => self.on_turn_end(end, now),
            Frame::ToolCall(call) => self.on_tool_call(conn, call, now),
            Frame::Ping(Ping { id }) => {
                vec![self.frame_out(conn, Frame::Pong(Pong { id }))]
            }
            // `pong` 是内核保活的回音，没有后续动作。
            Frame::Pong(_) => Vec::new(),
            // 适配器不该给内核发其余帧；未知帧在协议里就是要忽略的。
            _ => Vec::new(),
        }
    }

    fn on_hello(&mut self, conn: u64, hello: Hello) -> Vec<Outbound> {
        // 跨机器形态必须验令牌：接入点在网络上，文件权限保护不了它。本机形态（路径 /
        // 命名空间）由操作系统的套接字权限隔离，没有令牌就是没有令牌——不在这里再加一层
        // 「本机也算可信」的判断，因为内核不掌握对端地址，判断就得靠调用方补信息。
        if let Some(expected) = &self.config.adapter.token {
            let presented = hello.token.as_deref().unwrap_or_default();
            if !ct_eq(expected, presented) {
                return vec![
                    self.frame_out(
                        conn,
                        Frame::HelloReject(HelloReject {
                            id: hello.id,
                            version: PROTOCOL_VERSION,
                            reason: "接入令牌不匹配".to_owned(),
                        }),
                    ),
                    Outbound::CloseAdapter { conn },
                ];
            }
        }

        if hello.version != PROTOCOL_VERSION {
            return vec![
                self.frame_out(
                    conn,
                    Frame::HelloReject(HelloReject {
                        id: hello.id,
                        version: PROTOCOL_VERSION,
                        reason: format!(
                            "协议主版本不兼容：适配器 {}，内核 {PROTOCOL_VERSION}",
                            hello.version
                        ),
                    }),
                ),
                Outbound::CloseAdapter { conn },
            ];
        }
        vec![self.frame_out(
            conn,
            Frame::HelloOk(HelloOk {
                id: hello.id,
                version: PROTOCOL_VERSION,
            }),
        )]
    }

    fn on_deliver_ok(&mut self, id: &str) -> Vec<Outbound> {
        let Some(delivery) = self.awaiting.remove(id) else {
            return Vec::new();
        };
        let memory = delivery.memory.clone();
        let group = self
            .contexts
            .get(&memory)
            .map_or_else(String::new, |context| context.group.clone());

        match self.pipeline.confirm(&memory, &group, delivery.watermark) {
            Ok(()) => {
                self.retries.remove(&memory);
                Vec::new()
            }
            Err(error) => vec![Outbound::Log {
                level: "warn",
                message: format!("推进水位失败：{error}"),
            }],
        }
    }

    fn on_deliver_failed(&mut self, id: &str, reason: &str, now: OffsetDateTime) -> Vec<Outbound> {
        let Some(delivery) = self.awaiting.remove(id) else {
            return Vec::new();
        };
        let memory = delivery.memory.clone();
        // 失败不等于消息丢了：票作废、未读保留，到点重投。
        let target = self
            .contexts
            .get(&memory)
            .cloned()
            .map(|c| (c.device, c.group));
        self.pipeline.abandon(&memory, &delivery.ticket, now);
        self.contexts.remove(&memory);
        if let Some((device, group)) = target {
            self.schedule_retry(&memory, &device, &group, now);
        }

        let seconds = REDELIVER_AFTER.whole_seconds();
        vec![Outbound::Log {
            level: "warn",
            message: format!("投递被拒（{reason}）：{memory} 将在 {seconds} 秒后重投"),
        }]
    }

    fn on_turn_end(&mut self, end: TurnEnd, now: OffsetDateTime) -> Vec<Outbound> {
        let memory = end.memory.clone();
        let Some(context) = self.contexts.get(&memory).cloned() else {
            return vec![Outbound::Log {
                level: "warn",
                message: format!("记忆 {memory} 的回合结束但找不到上下文"),
            }];
        };
        let Some((device, scope)) = self.device_and_scope(&context) else {
            return vec![Outbound::Log {
                level: "warn",
                message: format!("记忆 {memory} 的上下文指向不存在的设备或范围"),
            }];
        };

        let persona = self.persona_of(&device.persona);
        let ticket = Ticket::from_wire(&end.ticket);
        // 收尾之后票就作废了，上下文一并清掉——下一次投递会重新写。
        self.contexts.remove(&memory);

        match self
            .pipeline
            .finish(&device, &scope, &persona, &ticket, now)
        {
            Ok(Some(delivery)) => {
                let mut out = vec![Outbound::Log {
                    level: "info",
                    message: format!("记忆 {memory} 有积压，合并投递"),
                }];
                out.extend(self.send_delivery(delivery, &device, &scope, now));
                out
            }
            Ok(None) => Vec::new(),
            Err(error) => vec![Outbound::Log {
                level: "warn",
                message: format!("记忆 {memory} 的回合结束被拒：{error}"),
            }],
        }
    }

    fn on_tool_call(&mut self, conn: u64, call: ToolCall, now: OffsetDateTime) -> Vec<Outbound> {
        let memory = call.memory.clone();
        let ticket = Ticket::from_wire(&call.ticket);

        // 票是唯一的身份来源：不认票就不认这次调用。
        let context = match self.pipeline.authorize(&memory, &ticket, now) {
            Ok(context) => TurnContext::from(context.clone()),
            Err(error) => {
                return fail_tool(conn, &call.id, ticket_error_code(error), error.to_string());
            }
        };

        match call.tool.as_str() {
            "qq_send_message" => self.send_message_tool(conn, call, &context),
            other => fail_tool(
                conn,
                &call.id,
                ErrorCode::ToolNotAllowed,
                format!("内核不认识工具 {other}"),
            ),
        }
    }

    fn send_message_tool(
        &mut self,
        conn: u64,
        call: ToolCall,
        context: &TurnContext,
    ) -> Vec<Outbound> {
        let Some(text) = call.args.get("text").and_then(Value::as_str) else {
            return fail_tool(
                conn,
                &call.id,
                ErrorCode::BadRequest,
                "qq_send_message 需要 text 参数",
            );
        };

        // 发送账号只能从这一轮的 send 列表里挑，不接受模型指定。
        let Some(account) = self.pick_sender(&context.send_accounts) else {
            return fail_tool(
                conn,
                &call.id,
                ErrorCode::AccountUnavailable,
                "这个范围当前没有可用账号",
            );
        };

        self.echo_seq += 1;
        let echo = format!("e-{}", self.echo_seq);
        let (tx, rx) = std::sync::mpsc::channel();
        self.pending_actions.push(PendingAction {
            conn,
            id: call.id,
            reply: rx,
        });

        vec![Outbound::Act {
            account,
            request: ActionRequest {
                action: ChatAction::SendGroupMessage {
                    group: context.group.clone(),
                    text: text.to_owned(),
                },
                echo,
                reply: tx,
            },
        }]
    }

    // ---- 时钟 ----

    fn on_tick(&mut self, now: OffsetDateTime) -> Vec<Outbound> {
        let mut out = Vec::new();

        // 在途动作的回执。先整批取走再逐个判定：`fail_tool` 是自由函数，
        // 但 `frame_out` 仍借 `self`，所以不能在 drain 期间调用。
        let drained = std::mem::take(&mut self.pending_actions);
        let mut keep = Vec::new();
        for pending in drained {
            match pending.reply.try_recv() {
                Ok(Ok(ActionOutcome::Ok { data })) => {
                    out.push(self.frame_out(
                        pending.conn,
                        Frame::ToolResult(ToolResult {
                            id: pending.id,
                            result: Some(data.unwrap_or(Value::Null)),
                            error: None,
                        }),
                    ));
                }
                Ok(Ok(ActionOutcome::Rejected { reason })) => {
                    out.extend(fail_tool(
                        pending.conn,
                        &pending.id,
                        ErrorCode::Internal,
                        format!("上游拒绝：{reason}"),
                    ));
                }
                Ok(Err(error)) => {
                    out.extend(fail_tool(
                        pending.conn,
                        &pending.id,
                        ErrorCode::Internal,
                        error.reason,
                    ));
                }
                Err(TryRecvError::Empty) => keep.push(pending),
                Err(TryRecvError::Disconnected) => {
                    out.extend(fail_tool(
                        pending.conn,
                        &pending.id,
                        ErrorCode::Internal,
                        "动作回执通道关闭",
                    ));
                }
            }
        }
        self.pending_actions = keep;

        // 超时回合。积压与原因保留，超时不代表消息丢了。
        for (memory, _ticket) in self.pipeline.sweep(now) {
            self.contexts.remove(&memory);
            out.push(Outbound::Log {
                level: "warn",
                message: format!("记忆 {memory} 的回合超时作废，积压保留"),
            });
        }

        // 到点的重投。
        let due: Vec<String> = self
            .retries
            .iter()
            .filter(|(_, retry)| now >= retry.due)
            .map(|(memory, _)| memory.clone())
            .collect();
        for memory in due {
            out.extend(self.redeliver(&memory, now));
        }

        out
    }

    fn redeliver(&mut self, memory: &str, now: OffsetDateTime) -> Vec<Outbound> {
        let Some(retry) = self.retries.get(memory) else {
            return Vec::new();
        };
        let (device_id, group) = (retry.device.clone(), retry.group.clone());
        self.retries.remove(memory);

        let Some(device) = self
            .config
            .devices
            .iter()
            .find(|device| device.id == device_id)
            .cloned()
        else {
            return Vec::new();
        };
        let Some(scope) = device
            .scopes
            .iter()
            .find(|scope| scope.group == group)
            .cloned()
        else {
            return Vec::new();
        };

        let persona = self.persona_of(&device.persona);
        let action = self.pipeline.rearrive(&device, &scope, &persona, now);
        self.after_action(&device, &scope, action, now)
    }

    fn schedule_retry(&mut self, memory: &str, device: &str, group: &str, now: OffsetDateTime) {
        let entry = self.retries.entry(memory.to_owned()).or_insert(Retry {
            attempts: 0,
            due: now,
            device: device.to_owned(),
            group: group.to_owned(),
        });
        entry.attempts = entry.attempts.saturating_add(1);
        entry.due = now + REDELIVER_AFTER;
        entry.device = device.to_owned();
        entry.group = group.to_owned();
    }

    // ---- 小工具 ----

    fn frame_out(&self, conn: u64, frame: Frame) -> Outbound {
        match crate::adapter::line_for(&frame) {
            Ok(line) => Outbound::Frame { conn, line },
            Err(reason) => Outbound::Log {
                level: "warn",
                message: format!("帧编码失败（{reason}），连接 {conn} 收不到这一帧"),
            },
        }
    }

    /// 按 `send` 列表顺序挑一个已就绪的账号。第一可用即主用，退到下一个才是轮换。
    fn pick_sender(&self, preferred: &[String]) -> Option<String> {
        preferred
            .iter()
            .find(|account| {
                self.pool
                    .state(account)
                    .is_some_and(|state| state.is_ready())
                    && self.actions.contains_key(*account)
            })
            .cloned()
    }

    /// 回合上下文指向的设备与范围。返回副本：调用方通常马上要改管线或票据表。
    fn device_and_scope(&self, context: &TurnContext) -> Option<(Device, ScopeConfig)> {
        let device = self
            .config
            .devices
            .iter()
            .find(|device| device.id == context.device)?;
        let scope = device
            .scopes
            .iter()
            .find(|scope| scope.group == context.group)?;
        Some((device.clone(), scope.clone()))
    }

    fn persona_of(&self, id: &str) -> String {
        self.personas.text(id).unwrap_or_default().to_owned()
    }

    fn device_owns(&self, device: &Device, user_id: &str) -> bool {
        device.accounts.iter().any(|account| {
            self.config
                .accounts
                .iter()
                .find(|known| &known.id == account)
                .is_some_and(|known| known.self_id == user_id)
        })
    }

    fn next_roll(&mut self) -> f64 {
        // xorshift64*：一次整数运算，够用且不引入依赖。
        self.rng ^= self.rng >> 12;
        self.rng ^= self.rng << 25;
        self.rng ^= self.rng >> 27;
        let value = self.rng.wrapping_mul(0x2545_F491_4F6C_DD1D);
        (value >> 11) as f64 / (1_u64 << 53) as f64
    }
}

/// 常量时间比较令牌。
///
/// 长度不同也走完整比较：提前返回会让攻击者按用时逐字节猜。令牌是内网里的一个共享
/// 字符串，长度差异本身不敏感，但用统一的比较路径省得每次都要论证一遍。
/// 脚本给的级别名映射到出站日志的那几个静态名。
///
/// `Outbound::Log` 的 `level` 是 `&'static str`——主循环只做一次 `eprintln!`，不值得为它
/// 引入分配。未知级别降到 `info`：脚本写错级别不该让日志丢掉。
fn hook_level(level: &str) -> &'static str {
    match level {
        "debug" => "debug",
        "warn" | "warning" => "warn",
        "error" => "error",
        _ => "info",
    }
}

fn ct_eq(expected: &Secret, presented: &str) -> bool {
    let expected = match expected {
        Secret::Literal(value) => value.as_str(),
        // 环境变量形式在启动期就该读出来；这里读不到就当不匹配，不 panic。
        Secret::Env(name) => return std::env::var(name).is_ok_and(|value| value == presented),
    };
    let left = expected.as_bytes();
    let right = presented.as_bytes();
    if left.len() != right.len() {
        return false;
    }
    left.ct_eq(right).into()
}

/// 一次工具调用的失败回执。自由函数：调用方常处在对 `self` 的可变借用里。
fn fail_tool(conn: u64, id: &str, code: ErrorCode, message: impl Into<String>) -> Vec<Outbound> {
    let frame = Frame::ToolResult(ToolResult {
        id: id.to_owned(),
        result: None,
        error: Some(ErrorBody::new(code, message)),
    });
    match crate::adapter::line_for(&frame) {
        Ok(line) => vec![Outbound::Frame { conn, line }],
        Err(reason) => vec![Outbound::Log {
            level: "warn",
            message: format!("错误帧编码失败（{reason}）"),
        }],
    }
}

fn ticket_error_code(error: crate::turn::TicketError) -> ErrorCode {
    match error {
        crate::turn::TicketError::Expired => ErrorCode::TicketExpired,
        crate::turn::TicketError::Mismatch => ErrorCode::TicketMismatch,
    }
}
