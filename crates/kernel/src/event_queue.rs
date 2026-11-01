//! 入站事件队列（ADR-0027 Phase 2c-2b）。
//!
//! 适配器只把平台事件翻译成 `RecordDraft` 丢进队列；由 core 侧的 drain 线程
//! 统一 `ingest`。适配器不再直接拿 `Bridge` 的锁，也不再有 `try_lock` + backlog。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::engine::Bridge;
use crate::storage::{RecordDraft, Store};

/// 入站队列的写端。可克隆，给每条适配器连接一个。
#[derive(Clone)]
pub struct EventQueue {
    tx: Sender<RecordDraft>,
}

/// 入站队列的读端，只有 core 的 drain 线程持有。
pub struct EventReceiver {
    rx: Receiver<RecordDraft>,
}

impl EventQueue {
    pub fn channel() -> (EventQueue, EventReceiver) {
        let (tx, rx) = channel();
        (EventQueue { tx }, EventReceiver { rx })
    }

    /// 适配器线程调用。队列无界，不阻塞平台读循环；接收端没了就返回 false。
    pub fn submit(&self, draft: RecordDraft) -> bool {
        self.tx.send(draft).is_ok()
    }
}

/// core 的入站 drain 线程。一次只 ingest 一条，保持队列顺序；锁只拿很短的时间。
pub fn drain_events<S: Store + Send + 'static>(
    bridge: Arc<Mutex<Bridge<S>>>,
    events: EventReceiver,
    shutdown: Arc<AtomicBool>,
) {
    while !shutdown.load(Ordering::Relaxed) {
        match events.rx.recv_timeout(Duration::from_millis(50)) {
            Ok(draft) => {
                let mut guard = bridge
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if let Err(error) = guard.ingest(draft) {
                    // 记不下来是真问题，说清楚，别装作没事。
                    eprintln!("事件队列：这条事件没能写进流水：{error}");
                }
            }
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use acb_proto::{Element, RoomId, Sender};

    fn draft(text: &str) -> RecordDraft {
        RecordDraft::message(
            RoomId::new("qq", "group/1").expect("合法"),
            Sender {
                id: "10001".into(),
                name: "老王".into(),
                role: None,
            },
            1,
            vec![Element::text(text)],
            false,
        )
    }

    #[test]
    fn queue_preserves_submission_order() {
        let (queue, receiver) = EventQueue::channel();
        queue.submit(draft("一"));
        queue.submit(draft("二"));

        let first = receiver.rx.recv_timeout(Duration::from_secs(1)).expect("第一条");
        let second = receiver.rx.recv_timeout(Duration::from_secs(1)).expect("第二条");
        assert!(matches!(&first.elements[0], Element::Text { text } if text == "一"));
        assert!(matches!(&second.elements[0], Element::Text { text } if text == "二"));
    }

    #[test]
    fn submit_after_receiver_drop_is_false() {
        let (queue, receiver) = EventQueue::channel();
        drop(receiver);
        assert!(!queue.submit(draft("没人收")));
    }

    #[test]
    fn drain_thread_ingests_and_wakes() {
        use crate::config::Config;
        use crate::outbound::{Outbound, OutboundError, Sent};
        use crate::storage::MemoryStore;
        use acb_proto::{Element, RoomId};
        use std::thread;
        use std::time::Instant;

        struct Silent;
        impl Outbound for Silent {
            fn send(
                &mut self,
                _room: &RoomId,
                _elements: &[Element],
            ) -> Result<Sent, OutboundError> {
                Ok(Sent {
                    platform_id: "p".into(),
                    at: 1,
                })
            }
        }

        let config = Config::parse(
            r#"
                [[connections]]
                id = "qq"
                platform = "qq"
                kind = "onebot11"
                url = "ws://127.0.0.1:3001"
                account = "bot"

                [[personas]]
                id = "p"
                name = "人格"
                text = "你是桥。"

                [[harnesses]]
                id = "dsh"
                name = "DeepSeek"

                [[sessions]]
                id = "main"
                harness = "dsh"
                persona = "p"
                token = "t-main"

                [[sessions.subscriptions]]
                connection = "qq"
                room = "group/1"

                [sessions.subscriptions.wake]
                at_mention = true
                name_mention = false
                poke = false
                accumulate = 1
            "#,
        )
        .expect("配置合法");
        let bridge = Arc::new(Mutex::new(Bridge::new(
            config,
            MemoryStore::new(),
            Box::new(Silent),
        )));
        let (queue, receiver) = EventQueue::channel();
        let shutdown = Arc::new(AtomicBool::new(false));
        let worker = {
            let bridge = Arc::clone(&bridge);
            let shutdown = Arc::clone(&shutdown);
            thread::spawn(move || drain_events(bridge, receiver, shutdown))
        };

        queue.submit(draft("在吗"));
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut got_turn = false;
        while Instant::now() < deadline && !got_turn {
            thread::sleep(Duration::from_millis(10));
            let mut guard = bridge
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if guard.poll_turn().expect("能开轮").is_some() {
                got_turn = true;
            }
        }

        shutdown.store(true, Ordering::Relaxed);
        let _ = queue.submit(draft("收工"));
        worker.join().expect("drain 线程能退出");
        assert!(got_turn, "入队的事件应该被 ingest 并触发一轮");
    }
}
