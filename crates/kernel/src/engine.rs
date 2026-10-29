//! 回合调度：订阅 → 唤醒 → 投递 → 工具 → 确认。
//!
//! 一条入站记录进来，走的是同一条路（ADR-0005）：
//! 1. 写流水——永远发生，不管唤不唤醒；
//! 2. 对每个订阅了这个房间的设备，按它的唤醒策略判定要不要叫醒；
//! 3. 要叫醒就排进队列，**该记忆**闲下来就开一轮，把未读打包投给 harness。
//!
//! 串行的单位是**记忆**，不是设备：记忆就是 agent 侧的那份会话。两台设备共用一份记忆时，
//! 它们自动排队——同一时刻只有一轮在改这份记忆（用户裁定，ADR-0003）。默认一设备一记忆，
//! 行为和「按设备排队」完全一样。
//!
//! 一把接入令牌可以代表**一组设备**（同一把令牌配给多台设备）。这一组能说的话，就是这组
//! 设备订阅的房间里最大的那个并集；越界就拒（用户裁定，ADR-0002）。
//!
//! 水位不在这里自动推——只有 agent 调 `ack_messages` 才推（ADR-0010）。唯一例外是自己
//! 发出去的消息：它从来不算未读。这件事有两层保证，缺一不可：
//! - 写进流水的同时就确认掉，否则它会挡住水位前沿；
//! - 投递时按连接的登录账号过滤掉，否则当前面还有没确认的消息时，水位推不过去，
//!   自己的话会被当成未读再投一遍。

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::time::{SystemTime, UNIX_EPOCH};

use acb_proto::element::flatten_text;
use acb_proto::{
    Delivery, Element, ErrorCode, Record, RecordKind, RoomId, Sender, Ticket, ToolError, TurnReason,
};
use serde::Deserialize;
use serde_json::{Value, json};
use thiserror::Error;

use crate::config::{Config, Device, WakeDecision, WakeInput, WakePolicy};
use crate::outbound::{Outbound, OutboundError};
use crate::storage::{
    Cursor, MIN_KEPT, Note, NoteKey, NoteKind, PrunePlan, RecordDraft, StorageError, Store,
    TopicStatus, unread,
};
use crate::tools::{
    AckArgs, DelNoteArgs, EventsArgs, HistoryArgs, NotesArgs, SendArgs, SendToArgs, SetNoteArgs,
    is_known,
};

/// 一轮最多投多少条。剩下的下一轮再投，免得一次塞爆上下文。
pub const MAX_DELIVERY: usize = 200;

/// 查历史时的默认条数与上限。
const DEFAULT_HISTORY: usize = 20;
const MAX_HISTORY: usize = 200;

/// 调度层错误：存储坏了，或者拿了一张不存在的票。
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum BridgeError {
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error("这张票无效或本轮已结束：{0}")]
    UnknownTicket(String),
}

/// 一条入站记录的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ingest {
    /// 记下来了。`woke` 是被这条叫醒的设备（按配置顺序）。
    Recorded { id: u64, woke: Vec<String> },
    /// 没有任何设备订阅这个房间——什么都不做，连流水都不写。
    NotSubscribed { room: RoomId },
}

/// 一次清理里某个房间的战果，给日志用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pruned {
    pub room: RoomId,
    /// 删掉多少条。
    pub deleted: u64,
    /// 还剩多少条。
    pub kept: u64,
}

/// 等着开轮次的（设备，房间）。
#[derive(Debug, Clone)]
struct Pending {
    device: String,
    room: RoomId,
    reason: TurnReason,
}

/// 正在跑的一轮。挂在记忆上：一份记忆同一时刻只有这一轮。
#[derive(Debug, Clone)]
struct ActiveTurn {
    /// 这一轮替哪台设备说话（决定水位、决定默认发到哪个身份）。
    device: String,
    room: RoomId,
    /// 本轮投出去的编号。只有这些能被 `ack_messages` 确认。
    delivered: BTreeSet<u64>,
}

/// 某个房间的一个订阅者，判定唤醒时要的东西。
#[derive(Debug, Clone)]
struct Subscriber {
    device: String,
    /// 这条连接登录的账号：用来认出「这句是我自己说的」。
    account: String,
    wake: WakePolicy,
}

/// 桥的核心：配置 + 存储 + 出站，加上调度状态。
pub struct Bridge<S: Store> {
    config: Config,
    store: S,
    outbound: Box<dyn Outbound>,
    pending: VecDeque<Pending>,
    /// 键是记忆 id：一份记忆同时只跑一轮。
    active: HashMap<String, ActiveTurn>,
    /// 票 → 记忆。票是开轮时签发的，收工作废。
    by_ticket: HashMap<Ticket, String>,
    ticket_seq: u64,
}

impl<S: Store> Bridge<S> {
    pub fn new(config: Config, store: S, outbound: Box<dyn Outbound>) -> Self {
        Self {
            config,
            store,
            outbound,
            pending: VecDeque::new(),
            active: HashMap::new(),
            by_ticket: HashMap::new(),
            ticket_seq: 0,
        }
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// 只读看存储，给管理页和测试用。
    pub fn store(&self) -> &S {
        &self.store
    }

    /// 哪几份记忆正跑着轮次。
    pub fn busy_memories(&self) -> Vec<String> {
        let mut memories: Vec<String> = self.active.keys().cloned().collect();
        memories.sort();
        memories
    }

    /// 正在跑的轮次替哪些设备说话（给日志与测试看）。
    pub fn busy_devices(&self) -> Vec<String> {
        let mut devices: Vec<String> = self
            .active
            .values()
            .map(|turn| turn.device.clone())
            .collect();
        devices.sort();
        devices
    }

    /// 清一次旧流水，返回真的删了东西的房间。
    ///
    /// `now` 是「现在」的 Unix 毫秒——传进来而不是自己取，是为了让测试能造出「很久以前」。
    ///
    /// 三条规矩：
    /// 1. **只删读过的**：安全线取所有在册订阅者水位的最小值。有一个设备一条都没读到过，
    ///    这个房间这次就一条都不删——未读是投递的原料，删了就是真丢了。
    /// 2. **只删还在订阅的房间**：没人订的房间不归这里管，判据（谁在读）也不成立了。
    /// 3. **每房间至少留一条**：编号靠 `MAX+1` 推，删光了下一个又回到 1，而水位还在几百，
    ///    新消息会被当成已读直接消失（[`MIN_KEPT`]）。
    pub fn prune(&mut self, now: i64) -> Result<Vec<Pruned>, BridgeError> {
        let retention = self.config.retention;
        if !retention.enabled() {
            return Ok(Vec::new());
        }

        let older_than = (retention.max_age_days > 0).then(|| {
            now.saturating_sub((retention.max_age_days as i64).saturating_mul(86_400_000))
        });

        let mut report = Vec::new();
        for room in self.store.rooms()? {
            let devices: Vec<String> = self
                .config
                .subscribers_of(&room)
                .into_iter()
                .map(|device| device.id.clone())
                .collect();
            if devices.is_empty() {
                continue;
            }

            // 安全线：所有在册设备都已读到的那个位置。初值是「一条都没删」的边界。
            let mut below = self.store.latest(&room)?.map_or(0, |latest| latest + 1);
            for device in devices {
                let cursor = Cursor::new(device, room.clone());
                below = below.min(self.store.watermark(&cursor)?);
            }
            if below == 0 {
                continue;
            }

            let before = self.store.count(&room)?;
            let deleted = self.store.prune(
                &room,
                &PrunePlan {
                    below,
                    keep_newest: retention.max_records_per_room.max(MIN_KEPT),
                    older_than,
                },
            )?;
            if deleted > 0 {
                report.push(Pruned {
                    room: room.clone(),
                    deleted,
                    kept: before - deleted,
                });
            }
        }
        Ok(report)
    }

    /// 一条入站记录进来。
    pub fn ingest(&mut self, draft: RecordDraft) -> Result<Ingest, BridgeError> {
        let room = draft.room.clone();
        let subscribers = self.subscribers_of(&room);

        if subscribers.is_empty() {
            return Ok(Ingest::NotSubscribed { room });
        }

        // 「引用的是不是我发的」要查库，所以入库前先判。判据是本房间那条连接的登录账号——
        // 被引用的那条如果是这个号发的，这条就算冲我来的（WakePolicy::reply）。
        let account = self.config.account_of_room(&room);
        let reply_to_self = self.quotes_own_message(&draft, &account)?;
        let draft = draft.with_reply_to_self(reply_to_self);

        let record = self.store.append(draft)?;
        let text = flatten_text(&record.elements);
        let event = match &record.kind {
            RecordKind::Event { event } => Some(event.as_str()),
            RecordKind::Message => None,
        };

        let mut woke = Vec::new();
        for subscriber in subscribers {
            let cursor = Cursor::new(subscriber.device.clone(), room.clone());

            // 平台把我们自己说的话回传回来：记下来，但不是未读，也不叫醒自己。
            if record.sender.id == subscriber.account {
                self.store.ack(&cursor, &[record.id])?;
                continue;
            }

            let pending = self
                .pending_records(&subscriber.device, &room, &subscriber.account)?
                .len() as u32;
            let input = WakeInput {
                sender_id: &record.sender.id,
                text: &text,
                at_self: record.at_self,
                reply_to_self: record.reply_to_self,
                event,
                unread: pending,
            };
            if subscriber.wake.decide(&input) == WakeDecision::Wake {
                woke.push(subscriber.device.clone());
                self.enqueue(subscriber.device, room.clone(), TurnReason::Wake);
            }
        }

        Ok(Ingest::Recorded {
            id: record.id,
            woke,
        })
    }

    /// 这条消息里有没有「引用本合同一个账号发过的消息」。
    ///
    /// 查不到被引用那条（被清理了、对面没给编号）就当不是——宁可少叫醒一次，
    /// 也不要凭编号长相猜。平台把自己的消息编号给全了，这件事才判得准。
    fn quotes_own_message(&self, draft: &RecordDraft, account: &str) -> Result<bool, StorageError> {
        if account.is_empty() {
            return Ok(false);
        }
        for element in &draft.elements {
            let Element::Quote { message_id } = element else {
                continue;
            };
            let Some(sender) = self.store.sender_of(&draft.room, message_id)? else {
                continue;
            };
            if sender.id == account {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// 取下一轮，不管是谁的。没有就返回 `None`。
    pub fn poll_turn(&mut self) -> Result<Option<Delivery>, BridgeError> {
        let index = (0..self.pending.len()).find(|&index| {
            let pending = &self.pending[index];
            !self
                .memory_of(&pending.device)
                .is_some_and(|memory| self.active.contains_key(memory))
        });
        let Some(index) = index else {
            return Ok(None);
        };
        let pending = self.pending.remove(index).expect("刚刚找到的");
        self.open_turn(pending).map(Some)
    }

    /// 只给这一组设备取活。接入点用这个——一条连接只代表它自己那组设备。
    pub fn poll_turn_for(&mut self, devices: &[String]) -> Result<Option<Delivery>, BridgeError> {
        let index = (0..self.pending.len()).find(|&index| {
            let pending = &self.pending[index];
            if !devices.iter().any(|device| device == &pending.device) {
                return false;
            }
            !self
                .memory_of(&pending.device)
                .is_some_and(|memory| self.active.contains_key(memory))
        });
        let Some(index) = index else {
            return Ok(None);
        };
        let pending = self.pending.remove(index).expect("刚刚找到的");
        self.open_turn(pending).map(Some)
    }

    /// 一轮收工。票作废，记忆空出来。
    ///
    /// **不推水位**：没确认的消息仍然算未读，下一轮接着投。
    pub fn end_turn(&mut self, ticket: &Ticket) -> Result<(), BridgeError> {
        let Some(memory) = self.by_ticket.remove(ticket) else {
            return Err(BridgeError::UnknownTicket(ticket.to_string()));
        };
        self.active.remove(&memory);
        Ok(())
    }

    /// 调工具。
    ///
    /// `devices` 是这次接入代表的设备组（同一把令牌）。发到哪儿、读哪儿的判据都从这里来：
    /// 能碰的房间 = 这组设备订阅的并集，越界就拒。
    pub fn tool_call(
        &mut self,
        ticket: &Ticket,
        devices: &[String],
        tool: &str,
        args: Value,
    ) -> Result<Value, ToolError> {
        let memory = self
            .by_ticket
            .get(ticket)
            .cloned()
            .ok_or_else(|| ToolError::new(ErrorCode::Unauthorized, "这张票无效或本轮已结束"))?;
        let (device, room, delivered) = {
            let turn = self.active.get(&memory).expect("有票就一定有活跃轮次");
            (
                turn.device.clone(),
                turn.room.clone(),
                turn.delivered.clone(),
            )
        };

        if !is_known(tool) {
            return Err(ToolError::new(
                ErrorCode::ToolNotAllowed,
                format!("没有这个工具：{tool}"),
            ));
        }

        match tool {
            // 不给目标：接着说当前这一轮的房间，用本轮的身份。
            "send_message" => {
                let args: SendArgs = parse(args)?;
                self.deliver_out(&device, &room, args.elements)
            }
            // 给了目标：在本组订阅范围里挑一台设备去说。撞车了要指明。
            "send_message_to" => {
                let args: SendToArgs = parse(args)?;
                let target = resolve_room(&args.to)?;
                let sender = self.pick_sender(devices, &target, args.device.as_deref())?;
                self.deliver_out(&sender, &target, args.elements)
            }
            "ack_messages" => {
                let args: AckArgs = parse(args)?;
                self.ack(&device, &room, &delivered, args.ids)
            }
            "get_history" => {
                let args: HistoryArgs = parse(args)?;
                self.history(devices, &room, args)
            }
            "list_rooms" => self.list_rooms(devices),
            "get_notes" => {
                let args: NotesArgs = parse(args)?;
                self.read_notes(devices, &room, args)
            }
            "set_note" => {
                let args: SetNoteArgs = parse(args)?;
                self.write_note(devices, &room, args)
            }
            "del_note" => {
                let args: DelNoteArgs = parse(args)?;
                self.delete_note(devices, &room, args)
            }
            "get_platform_events" => {
                let args: EventsArgs = parse(args)?;
                self.events(devices, &room, args)
            }
            other => Err(ToolError::new(
                ErrorCode::ToolNotAllowed,
                format!("没有这个工具：{other}"),
            )),
        }
    }

    // ---- 内部 ----

    /// 设备属于哪份记忆。
    fn memory_of(&self, device: &str) -> Option<&str> {
        self.config
            .devices
            .iter()
            .find(|candidate| candidate.id == device)
            .map(|candidate| candidate.memory.as_str())
    }

    /// 这个设备在这个房间里真正要处理的未读：水位之后、且不是自己说的。
    fn pending_records(
        &self,
        device: &str,
        room: &RoomId,
        account: &str,
    ) -> Result<Vec<Record>, StorageError> {
        let cursor = Cursor::new(device, room.clone());
        let mut records = unread(&self.store, &cursor, MAX_DELIVERY)?;
        records.retain(|record| record.sender.id != account);
        Ok(records)
    }

    fn subscribers_of(&self, room: &RoomId) -> Vec<Subscriber> {
        let account = self.config.account_of_room(room);
        self.config
            .devices
            .iter()
            .filter_map(|device| {
                device
                    .subscriptions
                    .iter()
                    .find(|subscription| &subscription.room == room)
                    .map(|subscription| Subscriber {
                        device: device.id.clone(),
                        account: account.clone(),
                        wake: subscription.wake.clone(),
                    })
            })
            .collect()
    }

    /// 这一组里，谁订了这个房间。
    fn devices_subscribing(&self, devices: &[String], room: &RoomId) -> Vec<&Device> {
        self.config
            .devices
            .iter()
            .filter(|device| devices.iter().any(|id| id == &device.id))
            .filter(|device| {
                device
                    .subscriptions
                    .iter()
                    .any(|subscription| &subscription.room == room)
            })
            .collect()
    }

    /// 发到 `room` 时用哪个身份。同组多台设备都订了同一个房间，就得指明。
    fn pick_sender(
        &self,
        devices: &[String],
        room: &RoomId,
        asked: Option<&str>,
    ) -> Result<String, ToolError> {
        let candidates = self.devices_subscribing(devices, room);
        let names: Vec<&str> = candidates.iter().map(|device| device.id.as_str()).collect();

        match asked {
            Some(wanted) => {
                if names.contains(&wanted) {
                    Ok(wanted.to_owned())
                } else {
                    Err(ToolError::new(
                        ErrorCode::RoomNotSubscribed,
                        format!(
                            "设备 {wanted} 在你这组里没有订阅 {room}；能发的是：{}",
                            join_or_none(&names)
                        ),
                    ))
                }
            }
            None => match names.as_slice() {
                [] => Err(ToolError::new(
                    ErrorCode::RoomNotSubscribed,
                    format!(
                        "你这组设备都没订阅 {room}；能发的是：{}",
                        join_or_none(&self.readable_rooms(devices))
                    ),
                )),
                [only] => Ok((*only).to_owned()),
                many => Err(ToolError::new(
                    ErrorCode::BadRequest,
                    format!(
                        "{room} 同时被这台设备的 {} 订阅，得用 device 指明谁来说",
                        many.join("、")
                    ),
                )),
            },
        }
    }

    /// 这一组能碰的所有房间（给报错信息用）。
    fn readable_rooms(&self, devices: &[String]) -> Vec<String> {
        let mut rooms: Vec<String> = self
            .config
            .devices
            .iter()
            .filter(|device| devices.iter().any(|id| id == &device.id))
            .flat_map(|device| device.subscriptions.iter())
            .map(|subscription| subscription.room.to_string())
            .collect();
        rooms.sort();
        rooms.dedup();
        rooms
    }

    /// 读的工具只要求「本组有人订了这个房间」。
    fn ensure_readable(&self, devices: &[String], room: &RoomId) -> Result<(), ToolError> {
        if self.devices_subscribing(devices, room).is_empty() {
            return Err(ToolError::new(
                ErrorCode::RoomNotSubscribed,
                format!("你这组设备都没订阅 {room}"),
            ));
        }
        Ok(())
    }

    fn enqueue(&mut self, device: String, room: RoomId, reason: TurnReason) {
        let already = self
            .pending
            .iter()
            .any(|pending| pending.device == device && pending.room == room);
        if already {
            return;
        }
        self.pending.push_back(Pending {
            device,
            room,
            reason,
        });
    }

    fn open_turn(&mut self, pending: Pending) -> Result<Delivery, BridgeError> {
        let account = self.config.account_of_room(&pending.room);
        let mut records = self.pending_records(&pending.device, &pending.room, &account)?;

        let mut delivered = BTreeSet::new();
        let cursor = Cursor::new(pending.device.clone(), pending.room.clone());
        for record in &mut records {
            // 计数落在存储里：重启之后同一个 N 接着数，不然看着像从没投过。
            let count = self.store.count_delivery(&cursor, record.id)?;
            if count > 1 {
                record.redelivery = Some(count);
                // 「第 N 次投递」和「怎么让它别再重投」必须跟着记录一起到模型眼前。
                // 这件事放在桥里做，不指望每个 harness 都记得补：harness 的职责只是
                // 把 records 塞进上下文，多一句话都不该由它发明（ADR-0010）。
                //
                // 注意写在 `note` 上，不塞进 elements：elements 是发送者说的话，
                // 系统的旁白混进去会让人（和模型）以为是对方打的字。
                record.note = Some(format!(
                    "第 {} 次投递；调 ack_messages 确认 #{} 之后就不再重复投递",
                    count, record.id
                ));
            }
            delivered.insert(record.id);
        }

        let memory = self
            .memory_of(&pending.device)
            .unwrap_or(&pending.device)
            .to_owned();

        self.ticket_seq += 1;
        let ticket =
            Ticket::new(format!("{}-{}", pending.device, self.ticket_seq)).expect("票面非空");

        self.by_ticket.insert(ticket.clone(), memory.clone());
        self.active.insert(
            memory.clone(),
            ActiveTurn {
                device: pending.device.clone(),
                room: pending.room.clone(),
                delivered,
            },
        );

        Ok(Delivery {
            ticket,
            device: pending.device,
            memory,
            room: pending.room,
            records,
            reason: pending.reason,
        })
    }

    fn deliver_out(
        &mut self,
        device: &str,
        room: &RoomId,
        elements: Vec<Element>,
    ) -> Result<Value, ToolError> {
        if elements.is_empty() {
            return Err(ToolError::new(ErrorCode::BadRequest, "没有内容可发"));
        }
        let account = self.config.account_of_room(room);
        if account.is_empty() {
            return Err(ToolError::new(
                ErrorCode::Internal,
                format!("{room} 所属的连接没有配登录账号，不知道自己是谁"),
            ));
        }

        let sent = self
            .outbound
            .send(room, &elements)
            .map_err(outbound_error)?;

        let record = self
            .store
            .append(
                RecordDraft::message(
                    room.clone(),
                    Sender {
                        id: account.clone(),
                        name: account,
                    },
                    sent.at,
                    elements,
                    false,
                )
                // 记下平台编号：下次有人引用这条，桥才反查得出「是我的话」。
                .with_platform_id(sent.platform_id.clone()),
            )
            .map_err(internal)?;

        // 自己说的话不是未读：写进流水的同时确认掉。
        let cursor = Cursor::new(device, room.clone());
        let watermark = self.store.ack(&cursor, &[record.id]).map_err(internal)?;

        Ok(json!({
            "id": record.id,
            "platform_id": sent.platform_id,
            "watermark": watermark,
        }))
    }

    fn ack(
        &mut self,
        device: &str,
        room: &RoomId,
        delivered: &BTreeSet<u64>,
        ids: Vec<u64>,
    ) -> Result<Value, ToolError> {
        if ids.is_empty() {
            return Err(ToolError::new(ErrorCode::BadRequest, "ids 不能为空"));
        }
        for id in &ids {
            if !delivered.contains(id) {
                return Err(ToolError::new(
                    ErrorCode::BadRequest,
                    format!(
                        "编号 {id} 不是本轮投递给你的记录，不能确认；\
                         先把本轮投给你的都处理完再确认"
                    ),
                ));
            }
        }

        let cursor = Cursor::new(device, room.clone());
        let watermark = self.store.ack(&cursor, &ids).map_err(internal)?;
        for id in &ids {
            self.store.forget_delivery(&cursor, *id).map_err(internal)?;
        }

        Ok(json!({ "watermark": watermark }))
    }

    fn history(
        &self,
        devices: &[String],
        turn_room: &RoomId,
        args: HistoryArgs,
    ) -> Result<Value, ToolError> {
        let room = self.resolve(&args.room, turn_room)?;
        self.ensure_readable(devices, &room)?;

        let limit = args.limit.unwrap_or(DEFAULT_HISTORY).min(MAX_HISTORY);
        let mut records: Vec<Record> = self
            .store
            .since(&room, 0, usize::MAX)
            .map_err(internal)?
            .into_iter()
            .filter(|record| args.before.is_none_or(|before| record.id < before))
            .collect();

        let skip = records.len().saturating_sub(limit);
        records.drain(..skip);
        Ok(json!({ "room": room, "records": records }))
    }

    /// 这组设备订阅的所有房间，按（设备，房间）各列一行。
    ///
    /// 两个设备订同一个房间时会出现两行——这是实话：水位本来就是各记各的。
    fn list_rooms(&self, devices: &[String]) -> Result<Value, ToolError> {
        let mut rooms = Vec::new();
        for id in devices {
            let Some(device) = self.config.device(id) else {
                continue;
            };
            for subscription in &device.subscriptions {
                let room = &subscription.room;
                let account = self.config.account_of_room(room);
                let cursor = Cursor::new(id.clone(), room.clone());
                let watermark = self.store.watermark(&cursor).map_err(internal)?;
                let unread = self
                    .pending_records(id, room, &account)
                    .map_err(internal)?
                    .len();
                rooms.push(json!({
                    "room": room,
                    "device": id,
                    "watermark": watermark,
                    "unread": unread,
                }));
            }
        }
        Ok(json!({ "rooms": rooms }))
    }

    fn read_notes(
        &self,
        devices: &[String],
        turn_room: &RoomId,
        args: NotesArgs,
    ) -> Result<Value, ToolError> {
        let room = self.resolve(&args.room, turn_room)?;
        self.ensure_readable(devices, &room)?;

        let kind = match args.kind.as_deref() {
            Some(text) => Some(parse_note_kind(text)?),
            None => None,
        };
        let notes: Vec<Note> = self
            .store
            .notes_in(&room)
            .map_err(internal)?
            .into_iter()
            .filter(|note| kind.is_none_or(|wanted| note.key.kind == wanted))
            .collect();

        Ok(json!({ "room": room, "notes": notes.iter().map(note_json).collect::<Vec<_>>() }))
    }

    fn write_note(
        &mut self,
        devices: &[String],
        turn_room: &RoomId,
        args: SetNoteArgs,
    ) -> Result<Value, ToolError> {
        let room = self.resolve(&args.room, turn_room)?;
        self.ensure_readable(devices, &room)?;

        let kind = match args.kind.as_deref() {
            Some(text) => parse_note_kind(text)?,
            None => NoteKind::Term,
        };
        let status = match args.status.as_deref() {
            Some("open") => Some(TopicStatus::Open),
            Some("closed") => Some(TopicStatus::Closed),
            Some(other) => {
                return Err(ToolError::new(
                    ErrorCode::BadRequest,
                    format!("status 只能是 open 或 closed，收到 {other}"),
                ));
            }
            None => None,
        };
        if status.is_some() && kind != NoteKind::Topic {
            return Err(ToolError::new(
                ErrorCode::BadRequest,
                "status 只对 topic 有用",
            ));
        }

        let key = NoteKey::new(room.clone(), kind, args.key);
        let note = Note {
            key: key.clone(),
            value: args.value,
            label: args.label,
            status,
            corrected: false,
            updated_at: now_millis(),
        };
        self.store.upsert_note(note).map_err(internal)?;

        Ok(json!({ "ok": true, "kind": kind.as_str(), "key": key.key }))
    }

    fn delete_note(
        &mut self,
        devices: &[String],
        turn_room: &RoomId,
        args: DelNoteArgs,
    ) -> Result<Value, ToolError> {
        let room = self.resolve(&args.room, turn_room)?;
        self.ensure_readable(devices, &room)?;

        let kind = match args.kind.as_deref() {
            Some(text) => parse_note_kind(text)?,
            None => NoteKind::Term,
        };
        let key = NoteKey::new(room, kind, args.key);
        let removed = self.store.delete_note(&key).map_err(internal)?;
        Ok(json!({ "removed": removed }))
    }

    fn events(
        &self,
        devices: &[String],
        turn_room: &RoomId,
        args: EventsArgs,
    ) -> Result<Value, ToolError> {
        let room = self.resolve(&args.room, turn_room)?;
        self.ensure_readable(devices, &room)?;

        let limit = args.limit.unwrap_or(DEFAULT_HISTORY).min(MAX_HISTORY);
        let mut records: Vec<Record> = self
            .store
            .since(&room, 0, usize::MAX)
            .map_err(internal)?
            .into_iter()
            .filter(|record| !record.is_message())
            .collect();

        let skip = records.len().saturating_sub(limit);
        records.drain(..skip);
        Ok(json!({ "room": room, "events": records }))
    }

    /// 参数里没给房间就用本轮的房间；给了就解析。
    fn resolve(&self, given: &Option<String>, turn_room: &RoomId) -> Result<RoomId, ToolError> {
        match given {
            Some(text) => resolve_room(text),
            None => Ok(turn_room.clone()),
        }
    }
}

fn join_or_none<T: AsRef<str>>(names: &[T]) -> String {
    if names.is_empty() {
        "（一个都没有）".to_owned()
    } else {
        names
            .iter()
            .map(|name| name.as_ref())
            .collect::<Vec<_>>()
            .join("、")
    }
}

fn resolve_room(text: &str) -> Result<RoomId, ToolError> {
    RoomId::parse(text).map_err(|error| {
        ToolError::new(
            ErrorCode::BadRequest,
            format!("房间要写成 连接:段/段（如 qq:group/123），收到 {text}（{error}）"),
        )
    })
}

fn parse<T: for<'de> Deserialize<'de>>(args: Value) -> Result<T, ToolError> {
    serde_json::from_value(args)
        .map_err(|error| ToolError::new(ErrorCode::BadRequest, format!("参数不对：{error}")))
}

fn parse_note_kind(text: &str) -> Result<NoteKind, ToolError> {
    NoteKind::parse(text).ok_or_else(|| {
        ToolError::new(
            ErrorCode::BadRequest,
            format!("笔记种类只能是 term / member / topic，收到 {text}"),
        )
    })
}

fn note_json(note: &Note) -> Value {
    json!({
        "kind": note.key.kind.as_str(),
        "key": note.key.key,
        "value": note.value,
        "label": note.label,
        "status": note.status.map(|status| match status {
            TopicStatus::Open => "open",
            TopicStatus::Closed => "closed",
        }),
        "corrected": note.corrected,
        "updated_at": note.updated_at,
    })
}

fn internal(error: impl std::fmt::Display) -> ToolError {
    ToolError::new(ErrorCode::Internal, error.to_string())
}

fn outbound_error(error: OutboundError) -> ToolError {
    match error {
        OutboundError::Rejected(detail) => ToolError::new(ErrorCode::Internal, detail),
        OutboundError::Unsupported(detail) => ToolError::new(ErrorCode::BadRequest, detail),
        OutboundError::Unavailable(detail) => ToolError::new(ErrorCode::Internal, detail),
    }
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}
