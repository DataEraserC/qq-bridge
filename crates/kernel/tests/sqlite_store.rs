//! SQLite 存储：重启之后还是同一份东西。
//!
//! 这里不重复内存实现的那套断言（那些在 `storage.rs` 的单元测试里），只盯 SQLite
//! 才会出问题的地方：**写下去的东西读回来还一样吗、关掉再开还在吗、写坏一半会不会留残渣**。

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use acb_kernel::{
    Cursor, Deliveries, Element, Note, NoteKey, NoteKind, Notes, PrunePlan, RecordDraft, RoomId,
    Sender, StorageError, Store, TopicStatus, Transcript, Watermarks, unread,
};

const AT: i64 = 1_793_000_000_000;

/// 临时库文件。析构时把整个目录删掉，测试失败也不留垃圾。
struct TempDb {
    path: PathBuf,
}

impl TempDb {
    fn new(name: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("时间正常")
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("acb-store-{}-{name}-{nanos}", std::process::id()));
        Self {
            path: dir.join("acb.db"),
        }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

fn room(text: &str) -> RoomId {
    RoomId::parse(text).expect("房间合法")
}

fn message(room: &RoomId, text: &str) -> RecordDraft {
    RecordDraft::message(
        room.clone(),
        Sender {
            id: "323622442".into(),
            name: "萨卡班甲鱼".into(),
        },
        AT,
        vec![Element::text(text)],
        false,
    )
}

#[test]
fn restart_keeps_records_and_numbering() {
    let db = TempDb::new("restart-records");
    let qq = room("qq:private/1");

    {
        let mut store = acb_kernel::SqliteStore::open(db.path()).expect("能打开");
        store.append(message(&qq, "第一句")).expect("可写");
        store.append(message(&qq, "第二句")).expect("可写");
    }

    // 关掉再开：同一个文件，等于桥重启。
    let mut store = acb_kernel::SqliteStore::open(db.path()).expect("能再打开");
    let third = store.append(message(&qq, "第三句")).expect("可写");
    assert_eq!(third.id, 3, "重启之后编号要接着数，不能从 1 重来");

    let all = store.since(&qq, 0, 100).expect("可读");
    assert_eq!(all.len(), 3);
    assert_eq!(all[0].id, 1);
    assert_eq!(all[2].sender.name, "萨卡班甲鱼");
    assert_eq!(
        all[2].elements,
        vec![Element::text("第三句")],
        "内容要原样读回来"
    );
    assert_eq!(store.latest(&qq).expect("可读"), Some(3));
}

#[test]
fn records_keep_their_identity_across_platforms() {
    let db = TempDb::new("platforms");
    let mut store = acb_kernel::SqliteStore::open(db.path()).expect("能打开");

    let qq = room("qq:123");
    let discord = room("discord:123");
    assert_eq!(store.append(message(&qq, "qq 的")).expect("可写").id, 1);
    assert_eq!(
        store.append(message(&discord, "dc 的")).expect("可写").id,
        1,
        "不同平台的 123 是两个房间，各自编号"
    );
    assert_eq!(store.since(&qq, 0, 10).expect("可读").len(), 1);
    assert_eq!(store.since(&discord, 0, 10).expect("可读").len(), 1);
}

#[test]
fn restart_keeps_watermark_and_pending_acks() {
    let db = TempDb::new("restart-watermark");
    let qq = room("qq:1");
    let cursor = Cursor::new("main", qq.clone());

    {
        let mut store = acb_kernel::SqliteStore::open(db.path()).expect("能打开");
        for text in ["a", "b", "c"] {
            store.append(message(&qq, text)).expect("可写");
        }
        // 先确认 1 和 3：缺 2，水位只能推到 1，3 记在「乱序已确认」里。
        assert_eq!(store.ack(&cursor, &[1, 3]).expect("可确认"), 1);
    }

    let mut store = acb_kernel::SqliteStore::open(db.path()).expect("能再打开");
    assert_eq!(store.watermark(&cursor).expect("可读"), 1, "水位要留着");
    assert_eq!(
        unread(&store, &cursor, 100).expect("可读").len(),
        2,
        "重启后未读还是 2、3 两条"
    );
    assert_eq!(
        store.ack(&cursor, &[2]).expect("可确认"),
        3,
        "3 是重启前确认过的，补上 2 就该一次推到 3"
    );
    assert!(unread(&store, &cursor, 100).expect("可读").is_empty());
}

#[test]
fn acking_a_missing_record_writes_nothing() {
    let db = TempDb::new("ack-missing");
    let qq = room("qq:1");
    let cursor = Cursor::new("main", qq.clone());
    let mut store = acb_kernel::SqliteStore::open(db.path()).expect("能打开");
    store.append(message(&qq, "只有这一条")).expect("可写");

    let error = store.ack(&cursor, &[1, 9]).expect_err("9 不存在，应当报错");
    assert!(matches!(error, StorageError::UnknownRecord { id: 9, .. }));
    assert_eq!(
        store.watermark(&cursor).expect("可读"),
        0,
        "整批拒绝：不能把 1 写进去、留个半截状态"
    );
}

#[test]
fn elements_survive_the_round_trip() {
    let db = TempDb::new("elements");
    let qq = room("qq:1");
    let elements = vec![
        Element::At {
            target: "3683924053".into(),
            all: false,
        },
        Element::text(" 看图"),
        Element::Image {
            url: "https://example.com/a.png".into(),
            alt: Some("截图".into()),
        },
        Element::Quote {
            message_id: "-1085036204".into(),
        },
        // 平台特有内容原样透传：核心不理解，但存进去再读出来必须一字不差。
        Element::Opaque {
            platform: "qq".into(),
            payload: serde_json::json!({ "type": "mface", "data": { "emoji_id": "abc" } }),
        },
    ];

    let mut store = acb_kernel::SqliteStore::open(db.path()).expect("能打开");
    let draft = RecordDraft::message(
        qq.clone(),
        Sender {
            id: "1".into(),
            name: "老王".into(),
        },
        AT,
        elements.clone(),
        true,
    );
    store.append(draft).expect("可写");
    drop(store);

    let store = acb_kernel::SqliteStore::open(db.path()).expect("能再打开");
    let back = store.since(&qq, 0, 10).expect("可读");
    assert_eq!(back[0].elements, elements, "元素数组要一字不差");
    assert!(back[0].at_self, "「@ 到我」这个判定也要留着");
}

#[test]
fn events_keep_their_type() {
    let db = TempDb::new("events");
    let qq = room("qq:1");
    let mut store = acb_kernel::SqliteStore::open(db.path()).expect("能打开");

    store
        .append(RecordDraft::event(
            qq.clone(),
            Sender {
                id: "1".into(),
                name: "老王".into(),
            },
            AT,
            "poke",
            vec![Element::text("[拍了拍我]")],
            true,
        ))
        .expect("可写");
    drop(store);

    let store = acb_kernel::SqliteStore::open(db.path()).expect("能再打开");
    let back = store.since(&qq, 0, 10).expect("可读");
    assert!(back[0].is_event("poke"), "事件类型不能丢");
    assert!(!back[0].is_message());
    assert!(back[0].at_self);
}

#[test]
fn restart_keeps_notes_with_all_their_parts() {
    let db = TempDb::new("restart-notes");
    let qq = room("qq:1");
    let key = NoteKey::new(qq.clone(), NoteKind::Topic, "部署");

    {
        let mut store = acb_kernel::SqliteStore::open(db.path()).expect("能打开");
        store
            .upsert_note(Note {
                key: key.clone(),
                value: "蓝绿发布，周五不动".into(),
                label: Some("运维".into()),
                status: Some(TopicStatus::Closed),
                corrected: true,
                updated_at: 1_793_000_000,
            })
            .expect("可写");
    }

    let mut store = acb_kernel::SqliteStore::open(db.path()).expect("能再打开");
    let note = store.note(&key).expect("可读").expect("笔记还在");
    assert_eq!(note.value, "蓝绿发布，周五不动");
    assert_eq!(note.label.as_deref(), Some("运维"));
    assert_eq!(note.status, Some(TopicStatus::Closed));
    assert!(note.corrected, "「纠正过」这个标记要留着");
    assert_eq!(note.updated_at, 1_793_000_000);
    assert_eq!(store.notes_in(&qq).expect("可读").len(), 1);

    // 覆盖同一个键是更新，不是插第二条。
    store
        .upsert_note(Note {
            key: key.clone(),
            value: "改主意了".into(),
            label: None,
            status: Some(TopicStatus::Open),
            corrected: false,
            updated_at: 1_793_000_100,
        })
        .expect("可写");
    assert_eq!(store.notes_in(&qq).expect("可读").len(), 1);
    assert_eq!(
        store.note(&key).expect("可读").expect("还在").value,
        "改主意了"
    );

    assert!(store.delete_note(&key).expect("可删"));
    assert!(
        !store.delete_note(&key).expect("再删不报错"),
        "删两次只算删掉一次"
    );
    assert!(store.note(&key).expect("可读").is_none());
}

#[test]
fn delivery_counts_survive_restart() {
    let db = TempDb::new("restart-deliveries");
    let qq = room("qq:1");
    let cursor = Cursor::new("main", qq.clone());

    {
        let mut store = acb_kernel::SqliteStore::open(db.path()).expect("能打开");
        store.append(message(&qq, "喂")).expect("可写");
        assert_eq!(store.count_delivery(&cursor, 1).expect("可写"), 1);
        assert_eq!(store.count_delivery(&cursor, 1).expect("可写"), 2);
    }

    let mut store = acb_kernel::SqliteStore::open(db.path()).expect("能再打开");
    assert_eq!(
        store.count_delivery(&cursor, 1).expect("可写"),
        3,
        "重启之后「第 N 次投递」要接着数"
    );

    // 另一个设备的计数是另一份。
    let other = Cursor::new("second", qq.clone());
    assert_eq!(store.count_delivery(&other, 1).expect("可写"), 1);

    store.forget_delivery(&cursor, 1).expect("可清");
    assert_eq!(
        store.count_delivery(&cursor, 1).expect("可写"),
        1,
        "确认过的记录再投就是重新开始算"
    );
}

#[test]
fn a_newer_schema_is_refused_instead_of_being_written_to() {
    let db = TempDb::new("schema");
    {
        let store = acb_kernel::SqliteStore::open(db.path()).expect("能打开");
        drop(store);
    }

    // 假装这个库是将来某一版程序写的。
    {
        let conn = rusqlite::Connection::open(db.path()).expect("能开");
        conn.execute(
            "UPDATE meta SET value = '99' WHERE key = 'schema_version'",
            [],
        )
        .expect("能改");
    }

    let error = acb_kernel::SqliteStore::open(db.path()).expect_err("应当拒绝打开");
    let message = error.to_string();
    assert!(message.contains("99"), "要说清库是第几版：{message}");
    assert!(
        message.contains(&format!("第 {} 版", acb_kernel::SCHEMA_VERSION)),
        "也要说清程序认识第几版：{message}"
    );
}

#[test]
fn an_empty_file_gets_its_tables_created() {
    let db = TempDb::new("fresh");
    let store = acb_kernel::SqliteStore::open(db.path()).expect("新库能建起来");
    let counts = store.counts().expect("能数");
    assert_eq!(counts.records, 0);
    assert_eq!(counts.notes, 0);
}

#[test]
fn counts_reflect_what_is_in_the_file() {
    let db = TempDb::new("counts");
    let qq = room("qq:1");
    let discord = room("discord:9");
    let mut store = acb_kernel::SqliteStore::open(db.path()).expect("能打开");
    store.append(message(&qq, "一")).expect("可写");
    store.append(message(&qq, "二")).expect("可写");
    store.append(message(&discord, "三")).expect("可写");
    store
        .upsert_note(Note {
            key: NoteKey::new(qq.clone(), NoteKind::Term, "部署"),
            value: "蓝绿".into(),
            label: None,
            status: None,
            corrected: false,
            updated_at: 1,
        })
        .expect("可写");

    let counts = store.counts().expect("能数");
    assert_eq!(counts.records, 3);
    assert_eq!(counts.rooms, 2);
    assert_eq!(counts.notes, 1);
}

/// 编译期确认：SQLite 实现满足桥要的那组约束。
#[test]
fn sqlite_store_satisfies_store_trait() {
    fn assert_store<S: Store>() {}
    assert_store::<acb_kernel::SqliteStore>();
}

#[test]
fn pruning_keeps_numbering_going_and_drops_orphans() {
    let db = TempDb::new("prune");
    let qq = room("qq:private/1");
    let main = Cursor::new("main", qq.clone());
    let mut store = acb_kernel::SqliteStore::open(db.path()).expect("能打开");

    for text in ["1", "2", "3", "4", "5"] {
        store.append(message(&qq, text)).expect("可写");
    }
    store.count_delivery(&main, 1).expect("能记");
    store.count_delivery(&main, 1).expect("能记");
    store.ack(&main, &[1, 2, 3, 4, 5]).expect("可确认");
    assert_eq!(store.count(&qq).expect("能数"), 5);

    let plan = PrunePlan {
        below: 6,
        keep_newest: 2,
        older_than: None,
    };
    assert_eq!(store.prune(&qq, &plan).expect("能清"), 3, "留最新 2 条");

    let left: Vec<u64> = store
        .since(&qq, 0, 100)
        .expect("可读")
        .iter()
        .map(|record| record.id)
        .collect();
    assert_eq!(left, vec![4, 5]);

    // 编号靠 MAX+1 推：清完之后下一句仍然是 6，不会回到 1。
    let next = store.append(message(&qq, "6")).expect("可写");
    assert_eq!(next.id, 6);

    // 指向已删记录的投递计数跟着没了：再记一次还是第 1 次。
    assert_eq!(store.count_delivery(&main, 1).expect("能记"), 1);
}

#[test]
fn a_pruned_room_stays_pruned_after_reopen() {
    let db = TempDb::new("prune-reopen");
    let qq = room("qq:private/1");
    let main = Cursor::new("main", qq.clone());

    {
        let mut store = acb_kernel::SqliteStore::open(db.path()).expect("能打开");
        for text in ["1", "2", "3", "4"] {
            store.append(message(&qq, text)).expect("可写");
        }
        store.ack(&main, &[1, 2, 3, 4]).expect("可确认");
        let plan = PrunePlan {
            below: 5,
            keep_newest: 1,
            older_than: None,
        };
        assert_eq!(store.prune(&qq, &plan).expect("能清"), 3);
    }

    let mut store = acb_kernel::SqliteStore::open(db.path()).expect("能再打开");
    assert_eq!(store.count(&qq).expect("能数"), 1, "删了就是删了，不回来");
    assert_eq!(store.latest(&qq).expect("可读"), Some(4));
    assert_eq!(
        store.append(message(&qq, "5")).expect("可写").id,
        5,
        "重启之后编号还是接着 4 往上"
    );
    assert_eq!(store.rooms().expect("可读"), vec![qq]);
}

#[test]
fn platform_id_survives_a_restart_and_is_findable() {
    let db = TempDb::new("platform-id");
    let qq = room("qq:group/769843907");

    {
        let mut store = acb_kernel::SqliteStore::open(db.path()).expect("能打开");
        // 我们自己发出去的那条：带平台编号。
        let draft = RecordDraft::message(
            qq.clone(),
            Sender {
                id: "3683924053".into(),
                name: "3683924053".into(),
            },
            AT,
            vec![Element::text("只发 192.168 网段")],
            false,
        )
        .with_platform_id("592852179");
        store.append(draft).expect("能写");
        store.append(message(&qq, "只发子网")).expect("能写");
    }

    let store = acb_kernel::SqliteStore::open(db.path()).expect("能再打开");
    let sender = store
        .sender_of(&qq, "592852179")
        .expect("能查")
        .expect("应当查得到");
    assert_eq!(sender.id, "3683924053", "反查要能认出是我方账号发的");
    assert!(
        store.sender_of(&qq, "999").expect("能查").is_none(),
        "没这个编号就给 None，不许瞎猜"
    );
}

#[test]
fn reply_to_self_survives_a_restart() {
    let db = TempDb::new("reply-flag");
    let qq = room("qq:group/1");

    {
        let mut store = acb_kernel::SqliteStore::open(db.path()).expect("能打开");
        let draft = message(&qq, "只发子网").with_reply_to_self(true);
        store.append(draft).expect("能写");
    }

    let store = acb_kernel::SqliteStore::open(db.path()).expect("能再打开");
    let records = store.since(&qq, 0, 10).expect("能读");
    assert!(records[0].reply_to_self, "重启之后「这是在回我」不能丢");
}

/// 老库（第 1 版，没有这两列）能不能就地升上来，而不是让用户删库。
#[test]
fn a_version_one_database_is_migrated_in_place() {
    let db = TempDb::new("migrate-v1");
    if let Some(parent) = db.path().parent() {
        std::fs::create_dir_all(parent).expect("建目录");
    }

    // 手搓一个第 1 版的库：records 表没有 reply_to_self / platform_id。
    {
        let conn = rusqlite::Connection::open(db.path()).expect("能开");
        conn.execute_batch(
            r#"
            CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
            CREATE TABLE records (
                platform TEXT NOT NULL,
                room TEXT NOT NULL,
                record_id INTEGER NOT NULL,
                kind TEXT NOT NULL,
                event TEXT,
                sender_id TEXT NOT NULL,
                sender_name TEXT NOT NULL,
                at INTEGER NOT NULL,
                at_self INTEGER NOT NULL,
                elements TEXT NOT NULL,
                PRIMARY KEY (platform, room, record_id)
            );
            INSERT INTO meta (key, value) VALUES ('schema_version', '1');
            "#,
        )
        .expect("建旧库");
        conn.execute(
            "INSERT INTO records
             (platform, room, record_id, kind, event, sender_id, sender_name, at, at_self, elements)
             VALUES ('qq', 'group/1', 1, 'message', NULL, '10001', '老王', ?1, 0, ?2)",
            rusqlite::params![AT, r#"[{"type":"text","text":"老消息"}]"#],
        )
        .expect("写旧数据");
    }

    let mut store = acb_kernel::SqliteStore::open(db.path()).expect("老库应当能升上来");
    let qq = room("qq:group/1");

    let records = store.since(&qq, 0, 10).expect("老数据还得读得出来");
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].elements, vec![Element::text("老消息")]);
    assert!(!records[0].reply_to_self, "老数据没有这个信息，按不是处理");
    assert!(records[0].platform_id.is_none(), "老数据没有平台编号");

    // 升上来之后新功能要能用。
    let draft = RecordDraft::message(
        qq.clone(),
        Sender {
            id: "3683924053".into(),
            name: "bot".into(),
        },
        AT,
        vec![Element::text("新消息")],
        false,
    )
    .with_platform_id("m-7");
    store.append(draft).expect("升级后能写新列");
    assert!(
        store.sender_of(&qq, "m-7").expect("能查").is_some(),
        "升级后新写的平台编号要查得到"
    );
}
