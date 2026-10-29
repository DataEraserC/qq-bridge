//! 投递记录：消息与平台事件统一用这个形状。
//!
//! 平台事件也走投递（写流水 → 定未读 → 定叫醒），所以它不是另一种东西，只是
//! [`RecordKind::Event`]。拍一拍就是 `event: "poke"`。

use serde::{Deserialize, Serialize};

use crate::element::Element;
use crate::room::RoomId;

/// 谁发的。平台的用户 id 与显示名。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sender {
    pub id: String,
    pub name: String,
}

/// 这条记录是消息还是事件。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RecordKind {
    /// 一条聊天消息。
    Message,
    /// 一个平台事件，`event` 是事件类型（如 `poke`）。
    Event { event: String },
}

/// 投递给 harness 的最小单位。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Record {
    /// 房间内递增编号，水位按它推进。
    pub id: u64,
    pub room: RoomId,
    pub kind: RecordKind,
    pub sender: Sender,
    /// Unix 毫秒。
    pub at: i64,
    pub elements: Vec<Element>,
    /// 是不是冲本账号来的（@ 到我、拍我）。
    #[serde(default)]
    pub at_self: bool,
    /// 这条是不是「引用了我发的某条消息」的回复。
    ///
    /// 和 `at_self` 分开记：引用和 @ 是两种不同的动作，有人会只想要其中一种叫醒。
    /// 判定要查被引用那条是谁发的，所以得先有 [`Record::platform_id`]。
    #[serde(default)]
    pub reply_to_self: bool,
    /// 平台自己的消息编号（QQ 的 `message_id`）。
    ///
    /// 只为一件事存在：别人引用了某条消息时，拿这个编号反查「被引用的是谁发的」。
    /// 我们发出去的消息也带，所以「引用了我」才判得出来。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform_id: Option<String>,
    /// 非空表示「第 N 次投递」。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redelivery: Option<u32>,
    /// 桥附在这条投递上的说明，比如重投时「第 2 次投递；调 ack_messages 确认 #5 之后
    /// 就不再重复投递」。
    ///
    /// **这不是发送者说的话**，所以不能混进 [`Record::elements`]：内容就是内容，系统的
    /// 话另起一行。harness 必须把它呈现出来，否则 agent 永远不知道该停哪条。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl Record {
    /// 是不是一条聊天消息。
    pub fn is_message(&self) -> bool {
        matches!(self.kind, RecordKind::Message)
    }

    /// 是不是指定类型的事件。
    pub fn is_event(&self, kind: &str) -> bool {
        matches!(&self.kind, RecordKind::Event { event } if event == kind)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Record {
        Record {
            id: 42,
            room: RoomId::new("qq", "123").expect("合法"),
            kind: RecordKind::Message,
            sender: Sender {
                id: "10001".into(),
                name: "老王".into(),
            },
            at: 1_793_000_000_000,
            elements: vec![Element::text("在吗")],
            at_self: false,
            reply_to_self: false,
            platform_id: None,
            redelivery: None,
            note: None,
        }
    }

    #[test]
    fn message_record_round_trips() {
        let record = sample();
        let json = serde_json::to_string(&record).expect("可序列化");
        let back: Record = serde_json::from_str(&json).expect("可反序列化");
        assert_eq!(back, record);
    }

    #[test]
    fn kind_is_flat_in_json() {
        let record = sample();
        let value = serde_json::to_value(&record).expect("可序列化");
        assert_eq!(value["kind"]["kind"], "message");
    }

    #[test]
    fn poke_is_an_event_with_kind_poke() {
        let mut record = sample();
        record.kind = RecordKind::Event {
            event: "poke".into(),
        };
        record.at_self = true;
        assert!(record.is_event("poke"));
        assert!(!record.is_message());

        let value = serde_json::to_value(&record).expect("可序列化");
        assert_eq!(value["kind"]["kind"], "event");
        assert_eq!(value["kind"]["event"], "poke");
    }

    #[test]
    fn redelivery_is_omitted_when_absent() {
        let value = serde_json::to_value(sample()).expect("可序列化");
        assert!(value.get("redelivery").is_none());
        assert!(value.get("note").is_none(), "没有说明时不该出现 note 字段");
    }

    #[test]
    fn note_is_a_separate_field_not_content() {
        let mut record = sample();
        record.redelivery = Some(2);
        record.note = Some("第 2 次投递；调 ack_messages 确认 #42 之后就不再重复投递".into());

        let value = serde_json::to_value(&record).expect("可序列化");
        let note = value["note"].as_str().expect("note 是字符串");
        assert!(note.contains("第 2 次投递"), "旁白要写明第几次：{note}");
        assert_eq!(
            value["elements"].as_array().expect("是数组").len(),
            1,
            "说明不能混进内容元素里"
        );
        assert_eq!(value["elements"][0]["text"], "在吗");
    }
}
