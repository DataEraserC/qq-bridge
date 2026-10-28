//! 回合调度：订阅 → 唤醒 → 投递 → 工具 → 确认。
//!
//! 一条入站记录进来，走的是同一条路（ADR-0005）：
//! 1. 写流水——永远发生，不管唤不唤醒；
//! 2. 对每个订阅了这个房间的设备，按它的唤醒策略判定要不要叫醒；
//! 3. 要叫醒就排进该设备的队列，设备闲下来就开一轮，把未读打包投给 harness。
//!
//! 设备内严格串行（ADR-0003）：一个设备同一时刻只有一轮，票在开轮时签发、收工时作废。
//!
//! 水位不在这里自动推——只有 agent 调 `ack_messages` 才推（ADR-0010）。唯一例外是自己
//! 发出去的消息：它从来不算未读。这件事有两层保证，缺一不可：
//! - 写进流水的同时就确认掉，否则它会挡住水位前沿；
//! - 投递时按发送账号过滤掉，否则当前面还有没确认的消息时，水位推不过去，
//!   自己的话会被当成未读再投一遍。

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::time::{SystemTime, UNIX_EPOCH};

use qqab_proto::element::flatten_text;
use qqab_proto::{
    Delivery, Element, ErrorCode, Record, RecordKind, RoomId, Sender, Ticket, ToolError, TurnReason,
};
use serde::Deserialize;
use serde_json::{Value, json};
use thiserror::Error;

use crate::config::{Config, WakeDecision, WakeInput, WakePolicy};
use crate::outbound::{Outbound, OutboundError};
use crate::storage::{
    Cursor, Note, NoteKey, NoteKind, RecordDraft, StorageError, Store, TopicStatus, unread,
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

/// 等着开轮次的（设备，房间）。
#[derive(Debug, Clone)]
struct Pending {
    device: String,
    room: RoomId,
    reason: TurnReason,
}

/// 正在跑的一轮。
#[derive(Debug, Clone)]
struct ActiveTurn {
    room: RoomId,
    /// 本轮投出去的编号。只有这些能被 `ack_messages` 确认。
    delivered: BTreeSet<u64>,
}

/// 桥的核心：配置 + 存储 + 出站，加上调度状态。
pub struct Bridge<S: Store> {
    config: Config,
    store: S,
    outbound: Box<dyn Outbound>,
    pending: VecDeque<Pending>,
    active: HashMap<String, ActiveTurn>,
    by_ticket: HashMap<Ticket, String>,
    /// 每条记录被投递过几次，用来标「第 N 次投递」。
    deliveries: HashMap<(String, RoomId, u64), u32>,
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
            deliveries: HashMap::new(),
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

    /// 有没有设备正在跑轮次。
    pub fn busy_devices(&self) -> Vec<String> {
        let mut devices: Vec<String> = self.active.keys().cloned().collect();
        devices.sort();
        devices
    }

    /// 一条入站记录进来。
    pub fn ingest(&mut self, draft: RecordDraft) -> Result<Ingest, BridgeError> {
        let room = draft.room.clone();
        let subscribers = self.subscribers_of(&room);

        if subscribers.is_empty() {
            return Ok(Ingest::NotSubscribed { room });
        }

        let record = self.store.append(draft)?;
        let text = flatten_text(&record.elements);
        let event = match &record.kind {
            RecordKind::Event { event } => Some(event.as_str()),
            RecordKind::Message => None,
        };

        let mut woke = Vec::new();
        for (device, account, wake) in subscribers {
            let cursor = Cursor::new(device.clone(), room.clone());

            // 平台把我们自己说的话回传回来：记下来，但不是未读，也不叫醒自己。
            if record.sender.id == account {
                self.store.ack(&cursor, &[record.id])?;
                continue;
            }

            let pending = self.pending_records(&device, &room, &account)?.len() as u32;
            let input = WakeInput {
                sender_id: &record.sender.id,
                text: &text,
                at_self: record.at_self,
                event,
                unread: pending,
            };
            if wake.decide(&input) == WakeDecision::Wake {
                woke.push(device.clone());
                self.enqueue(device, room.clone(), TurnReason::Wake);
            }
        }

        Ok(Ingest::Recorded {
            id: record.id,
            woke,
        })
    }

    /// 取下一轮。没有就返回 `None`。
    ///
    /// 这是 harness 那侧「要活干」的入口：有设备空着、队列里有活，就开一轮。
    pub fn poll_turn(&mut self) -> Result<Option<Delivery>, BridgeError> {
        let index = self
            .pending
            .iter()
            .position(|pending| !self.active.contains_key(&pending.device));
        let Some(index) = index else {
            return Ok(None);
        };
        let pending = self.pending.remove(index).expect("刚刚找到的");
        self.open_turn(pending).map(Some)
    }

    /// 只给某个设备取活。接入点用这个——一条连接只服务一个设备，不能把别人的活领走。
    pub fn poll_turn_for(&mut self, device: &str) -> Result<Option<Delivery>, BridgeError> {
        if self.active.contains_key(device) {
            return Ok(None);
        }
        let index = self
            .pending
            .iter()
            .position(|pending| pending.device == device);
        let Some(index) = index else {
            return Ok(None);
        };
        let pending = self.pending.remove(index).expect("刚刚找到的");
        self.open_turn(pending).map(Some)
    }

    /// 一轮收工。票作废，设备空出来。
    ///
    /// **不推水位**：没确认的消息仍然算未读，下一轮接着投。
    pub fn end_turn(&mut self, ticket: &Ticket) -> Result<(), BridgeError> {
        let Some(device) = self.by_ticket.remove(ticket) else {
            return Err(BridgeError::UnknownTicket(ticket.to_string()));
        };
        self.active.remove(&device);
        Ok(())
    }

    /// 调工具。票不对直接拒。
    pub fn tool_call(
        &mut self,
        ticket: &Ticket,
        tool: &str,
        args: Value,
    ) -> Result<Value, ToolError> {
        let device = self
            .by_ticket
            .get(ticket)
            .cloned()
            .ok_or_else(|| ToolError::new(ErrorCode::Unauthorized, "这张票无效或本轮已结束"))?;
        let (room, delivered) = {
            let turn = self.active.get(&device).expect("有票就一定有活跃轮次");
            (turn.room.clone(), turn.delivered.clone())
        };

        if !is_known(tool) {
            return Err(ToolError::new(
                ErrorCode::ToolNotAllowed,
                format!("没有这个工具：{tool}"),
            ));
        }

        match tool {
            "send_message" => {
                let args: SendArgs = parse(args)?;
                let subscription = self.subscription(&device, &room)?;
                let account = subscription.send_account.clone();
                self.deliver_out(&device, &room, &account, args.elements)
            }
            "send_message_to" => {
                let args: SendToArgs = parse(args)?;
                let target = resolve_room(&args.to)?;
                let subscription = self.subscription(&device, &target)?;
                let account = subscription.send_account.clone();
                self.deliver_out(&device, &target, &account, args.elements)
            }
            "ack_messages" => {
                let args: AckArgs = parse(args)?;
                self.ack(&device, &room, &delivered, args.ids)
            }
            "get_history" => {
                let args: HistoryArgs = parse(args)?;
                self.history(&device, &room, args)
            }
            "list_rooms" => self.list_rooms(&device),
            "get_notes" => {
                let args: NotesArgs = parse(args)?;
                self.read_notes(&device, &room, args)
            }
            "set_note" => {
                let args: SetNoteArgs = parse(args)?;
                self.write_note(&device, &room, args)
            }
            "del_note" => {
                let args: DelNoteArgs = parse(args)?;
                self.delete_note(&device, &room, args)
            }
            "get_platform_events" => {
                let args: EventsArgs = parse(args)?;
                self.events(&device, &room, args)
            }
            other => Err(ToolError::new(
                ErrorCode::ToolNotAllowed,
                format!("没有这个工具：{other}"),
            )),
        }
    }

    // ---- 内部 ----

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

    fn subscribers_of(&self, room: &RoomId) -> Vec<(String, String, WakePolicy)> {
        let mut found = Vec::new();
        for device in &self.config.devices {
            if let Some(subscription) = device
                .subscriptions
                .iter()
                .find(|subscription| &subscription.room == room)
            {
                found.push((
                    device.id.clone(),
                    subscription.send_account.clone(),
                    subscription.wake.clone(),
                ));
            }
        }
        found
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
        let account = self
            .config
            .subscription(&pending.device, &pending.room)
            .map(|subscription| subscription.send_account.clone())
            .unwrap_or_default();
        let mut records = self.pending_records(&pending.device, &pending.room, &account)?;

        let mut delivered = BTreeSet::new();
        for record in &mut records {
            let key = (pending.device.clone(), pending.room.clone(), record.id);
            let count = self.deliveries.entry(key).or_insert(0);
            *count += 1;
            if *count > 1 {
                record.redelivery = Some(*count);
                // 「第 N 次投递」和「怎么让它别再重投」必须跟着记录一起到模型眼前。
                // 这件事放在桥里做，不指望每个 harness 都记得补：harness 的职责只是
                // 把 records 塞进上下文，多一句话都不该由它发明（ADR-0010）。
                record.elements.push(Element::text(format!(
                    "（第 {} 次投递；调 ack_messages 确认 #{} 之后就不再重复投递）",
                    *count, record.id
                )));
            }
            delivered.insert(record.id);
        }

        self.ticket_seq += 1;
        let ticket =
            Ticket::new(format!("{}-{}", pending.device, self.ticket_seq)).expect("票面非空");

        self.by_ticket
            .insert(ticket.clone(), pending.device.clone());
        self.active.insert(
            pending.device.clone(),
            ActiveTurn {
                room: pending.room.clone(),
                delivered,
            },
        );

        Ok(Delivery {
            ticket,
            device: pending.device,
            room: pending.room,
            records,
            reason: pending.reason,
        })
    }

    fn subscription(
        &self,
        device: &str,
        room: &RoomId,
    ) -> Result<&crate::config::Subscription, ToolError> {
        self.config.subscription(device, room).ok_or_else(|| {
            ToolError::new(
                ErrorCode::RoomNotSubscribed,
                format!("设备 {device} 没有订阅 {room}"),
            )
        })
    }

    fn deliver_out(
        &mut self,
        device: &str,
        room: &RoomId,
        account: &str,
        elements: Vec<Element>,
    ) -> Result<Value, ToolError> {
        if elements.is_empty() {
            return Err(ToolError::new(ErrorCode::BadRequest, "没有内容可发"));
        }

        let sent = self
            .outbound
            .send(device, room, account, &elements)
            .map_err(outbound_error)?;

        let record = self
            .store
            .append(RecordDraft::message(
                room.clone(),
                Sender {
                    id: account.to_owned(),
                    name: account.to_owned(),
                },
                sent.at,
                elements,
                false,
            ))
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
            self.deliveries
                .remove(&(device.to_owned(), room.clone(), *id));
        }

        Ok(json!({ "watermark": watermark }))
    }

    fn history(
        &self,
        device: &str,
        turn_room: &RoomId,
        args: HistoryArgs,
    ) -> Result<Value, ToolError> {
        let room = self.resolve(&args.room, turn_room)?;
        self.subscription(device, &room)?;

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

    fn list_rooms(&self, device: &str) -> Result<Value, ToolError> {
        let Some(entry) = self
            .config
            .devices
            .iter()
            .find(|candidate| candidate.id == device)
        else {
            return Err(ToolError::new(
                ErrorCode::Unauthorized,
                format!("没有这个设备：{device}"),
            ));
        };

        let mut rooms = Vec::new();
        for subscription in &entry.subscriptions {
            let cursor = Cursor::new(device, subscription.room.clone());
            let watermark = self.store.watermark(&cursor).map_err(internal)?;
            let unread = self
                .pending_records(device, &subscription.room, &subscription.send_account)
                .map_err(internal)?
                .len();
            rooms.push(json!({
                "room": subscription.room,
                "send_account": subscription.send_account,
                "watermark": watermark,
                "unread": unread,
            }));
        }
        Ok(json!({ "rooms": rooms }))
    }

    fn read_notes(
        &self,
        device: &str,
        turn_room: &RoomId,
        args: NotesArgs,
    ) -> Result<Value, ToolError> {
        let room = self.resolve(&args.room, turn_room)?;
        self.subscription(device, &room)?;

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
        device: &str,
        turn_room: &RoomId,
        args: SetNoteArgs,
    ) -> Result<Value, ToolError> {
        let room = self.resolve(&args.room, turn_room)?;
        self.subscription(device, &room)?;

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
        device: &str,
        turn_room: &RoomId,
        args: DelNoteArgs,
    ) -> Result<Value, ToolError> {
        let room = self.resolve(&args.room, turn_room)?;
        self.subscription(device, &room)?;

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
        device: &str,
        turn_room: &RoomId,
        args: EventsArgs,
    ) -> Result<Value, ToolError> {
        let room = self.resolve(&args.room, turn_room)?;
        self.subscription(device, &room)?;

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

fn resolve_room(text: &str) -> Result<RoomId, ToolError> {
    RoomId::parse(text).map_err(|error| {
        ToolError::new(
            ErrorCode::BadRequest,
            format!("房间要写成 平台:房间号，收到 {text}（{error}）"),
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
