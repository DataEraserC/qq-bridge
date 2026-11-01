//! conformance 测试用的最小动态库适配器。
//!
//! 它证明 `.so` 只靠 C ABI + 共用 JSON wire 就能接进桥：报 manifest、
//! 按连接创建实例、发事件、回发送/调用结果、按 shutdown 收尾。

use std::ffi::{CStr, CString, c_char, c_void};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use acb_adapter_api::abi::{ABI_VERSION, AdapterV1, HostV1};
use acb_adapter_api::{
    AdapterMessage, CoreMessage, WireConnection, WireError, WireInstanceConfig, WireManifest,
    WireMethodSpec, WireOutgoingKind, WireOutgoingRecord, WireParamSpec, WirePluginMetadata,
    WireRecord, WireResult, decode_line, encode_line,
};
use acb_proto::{Element, RecordKind, RoomId, Sender};
use serde_json::{Value, json};

struct Fixture {
    host: HostV1,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

/// `HostV1` 里的 ctx 是 core 保证活到插件销毁之后的指针；跨线程回调是协议允许的。
struct SendHost(HostV1);
unsafe impl Send for SendHost {}

impl SendHost {
    fn get(&self) -> HostV1 {
        self.0
    }
}

static TABLE: AdapterV1 = AdapterV1 {
    abi_version: ABI_VERSION,
    manifest: Some(fixture_manifest),
    create: Some(fixture_create),
    invoke: Some(fixture_invoke),
    destroy: Some(fixture_destroy),
    free_string: Some(fixture_free_string),
};

/// 唯一的导出符号。
#[unsafe(no_mangle)]
pub extern "C" fn acb_adapter_entry_v1() -> *const AdapterV1 {
    &TABLE
}

unsafe extern "C" fn fixture_manifest() -> *mut c_char {
    let manifest = WireManifest {
        id: "demo.adapter.fixture".into(),
        platform: "demo".into(),
        kind: "demo".into(),
        transports: vec!["dynamic".into()],
        api_version: 1,
        capabilities: vec![
            "receive_events".into(),
            "send".into(),
            "catalog".into(),
        ],
        extension: "platform_adapter".into(),
        metadata: WirePluginMetadata {
            title: Some("动态库夹具".into()),
            summary: Some("测试用：hello/describe/事件/发送/调用".into()),
            description: None,
            tags: vec!["fixture".into()],
            effects: vec!["external_call".into()],
            docs_url: None,
            options_schema: None,
        },
    };
    let json = serde_json::to_string(&manifest).expect("manifest 可序列化");
    CString::new(json)
        .expect("manifest 没有 NUL")
        .into_raw()
}

unsafe extern "C" fn fixture_create(config_json: *const c_char, host: *const HostV1) -> *mut c_void {
    if config_json.is_null() || host.is_null() {
        return std::ptr::null_mut();
    }
    let config = unsafe { CStr::from_ptr(config_json) }
        .to_string_lossy()
        .into_owned();
    let connection: WireConnection = match decode_line::<WireInstanceConfig>(&config) {
        Ok(WireInstanceConfig::Adapter { connection }) => connection,
        _ => return std::ptr::null_mut(),
    };
    let host = unsafe { *host };
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let thread_host = SendHost(host);
    let thread = thread::spawn(move || {
        // 启动后主动发一条入站事件，证明 core ← 插件的通路。
        thread::sleep(Duration::from_millis(100));
        if thread_stop.load(Ordering::Relaxed) {
            return;
        }
        let host = thread_host.get();
        let Some(submit) = host.submit_event else {
            return;
        };
        let event = AdapterMessage::Event {
            record: WireRecord {
                room: RoomId::new("demo", "group/1").expect("测试房间合法"),
                kind: RecordKind::Message,
                sender: Sender {
                    id: "10001".into(),
                    name: "动态夹具".into(),
                    role: None,
                },
                at: 1_700_000_000_000,
                elements: vec![Element::text("hello from dyn fixture")],
                at_self: false,
                platform_id: Some("dyn-event".into()),
                reply_to_self: false,
            },
        };
        let json = encode_line(&event).expect("事件可序列化");
        let text = CString::new(json).expect("事件没有 NUL");
        unsafe { submit(host.ctx, text.as_ptr()) };
    });

    let fixture = Fixture {
        host,
        stop,
        thread: Some(thread),
    };
    let _ = connection;
    Box::into_raw(Box::new(fixture)) as *mut c_void
}

unsafe extern "C" fn fixture_invoke(handle: *mut c_void, request_json: *const c_char) -> *mut c_char {
    if handle.is_null() || request_json.is_null() {
        return std::ptr::null_mut();
    }
    let fixture = unsafe { &*(handle as *const Fixture) };
    let request = unsafe { CStr::from_ptr(request_json) }
        .to_string_lossy()
        .into_owned();
    let message: CoreMessage = match decode_line(&request) {
        Ok(message) => message,
        Err(_) => return std::ptr::null_mut(),
    };

    let reply = match message {
        CoreMessage::Describe { id } => AdapterMessage::Reply {
            id,
            result: WireResult::Catalog {
                catalog: catalog(),
            },
        },
        CoreMessage::Send { id, .. } => AdapterMessage::Reply {
            id,
            result: WireResult::Sent {
                platform_id: format!("dyn-{id}"),
                at: 1_700_000_000_000,
            },
        },
        CoreMessage::Call {
            id,
            method,
            params,
            ..
        } => {
            let result = match method.as_str() {
                "demo.echo" => WireResult::Call {
                    value: params,
                    record: None,
                },
                "demo.send" => {
                    let text = params
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    WireResult::Call {
                        value: json!({ "message_id": format!("dyn-{id}") }),
                        record: Some(WireOutgoingRecord {
                            kind: WireOutgoingKind::Message,
                            elements: vec![Element::text(text)],
                            platform_id: Some(format!("dyn-{id}")),
                            at: Some(1_700_000_000_000),
                        }),
                    }
                }
                "demo.fail" => WireResult::Error {
                    error: WireError::BadParams {
                        detail: "demo fixture 故意失败".into(),
                    },
                },
                other => WireResult::Error {
                    error: WireError::UnknownMethod {
                        method: other.to_owned(),
                        suggestion: Some("demo.echo".into()),
                    },
                },
            };
            AdapterMessage::Reply { id, result }
        }
        CoreMessage::Decide { id, .. } => AdapterMessage::Reply {
            id,
            result: WireResult::Error {
                error: WireError::Unsupported {
                    detail: "这个 fixture 不是策略 provider".into(),
                },
            },
        },
        CoreMessage::Shutdown => {
            fixture.stop.store(true, Ordering::Relaxed);
            if let Some(log) = fixture.host.log {
                let level = CString::new("info").expect("没 NUL");
                let text = CString::new("fixture 收到 shutdown").expect("没 NUL");
                unsafe { log(fixture.host.ctx, level.as_ptr(), text.as_ptr()) };
            }
            return std::ptr::null_mut();
        }
    };

    let json = encode_line(&reply).expect("回执可序列化");
    CString::new(json).expect("回执没有 NUL").into_raw()
}

unsafe extern "C" fn fixture_destroy(handle: *mut c_void) {
    if handle.is_null() {
        return;
    }
    let mut fixture = unsafe { Box::from_raw(handle as *mut Fixture) };
    fixture.stop.store(true, Ordering::Relaxed);
    if let Some(thread) = fixture.thread.take() {
        let _ = thread.join();
    }
}

unsafe extern "C" fn fixture_free_string(text: *mut c_char) {
    if text.is_null() {
        return;
    }
    drop(unsafe { CString::from_raw(text) });
}

fn catalog() -> Vec<WireMethodSpec> {
    let text_param = WireParamSpec {
        name: "text".into(),
        ty: "string".into(),
        required: true,
        desc: "正文".into(),
        values: Vec::new(),
    };
    vec![
        WireMethodSpec {
            name: "demo.echo".into(),
            summary: "回显收到的参数".into(),
            tags: vec!["send".into()],
            risk: "read".into(),
            scope: "room".into(),
            params: vec![text_param.clone()],
            targets: Vec::new(),
            effect: "read".into(),
            record: "none".into(),
        },
        WireMethodSpec {
            name: "demo.send".into(),
            summary: "假装发一条消息并记账".into(),
            tags: vec!["send".into(), "message".into()],
            risk: "write".into(),
            scope: "room".into(),
            params: vec![text_param],
            targets: Vec::new(),
            effect: "send".into(),
            record: "message".into(),
        },
        WireMethodSpec {
            name: "demo.fail".into(),
            summary: "永远返回参数错误".into(),
            tags: vec!["system".into()],
            risk: "read".into(),
            scope: "room".into(),
            params: Vec::new(),
            targets: Vec::new(),
            effect: "read".into(),
            record: "none".into(),
        },
    ]
}
