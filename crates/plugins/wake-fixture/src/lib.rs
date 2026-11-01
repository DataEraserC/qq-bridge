//! conformance 测试用的最小动态库 wake provider。
//!
//! 证明策略扩展点与平台适配器共用同一套 C ABI + JSON wire：
//! manifest 声明 `extension = "wake"`，create 收到 `WireInstanceConfig::Provider`，
//! 每次判定走 `CoreMessage::Decide`，返回 `WireResult::Decision`。

use std::ffi::{CStr, CString, c_char, c_void};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use acb_adapter_api::abi::{ABI_VERSION, AdapterV1, HostV1};
use acb_adapter_api::{
    AdapterMessage, CoreMessage, WireError, WireInstanceConfig, WireManifest, WirePluginMetadata,
    WireResult, WireWakeRequest, decode_line, encode_line,
};

struct Fixture {
    stop: Arc<AtomicBool>,
}

static TABLE: AdapterV1 = AdapterV1 {
    abi_version: ABI_VERSION,
    manifest: Some(wake_manifest),
    create: Some(wake_create),
    invoke: Some(wake_invoke),
    destroy: Some(wake_destroy),
    free_string: Some(wake_free_string),
};

#[unsafe(no_mangle)]
pub extern "C" fn acb_adapter_entry_v1() -> *const AdapterV1 {
    &TABLE
}

unsafe extern "C" fn wake_manifest() -> *mut c_char {
    let manifest = WireManifest {
        id: "fixture.wake".into(),
        platform: "fixture".into(),
        kind: String::new(),
        transports: Vec::new(),
        api_version: 1,
        capabilities: vec!["wake_decision".into()],
        events: Vec::new(),
        event_tags: Vec::new(),
        supports_at: false,
        supports_reply: false,
        extension: "wake".into(),
        metadata: WirePluginMetadata {
            title: Some("wake 夹具".into()),
            summary: Some("被 @ 或正文含 fixture 时唤醒".into()),
            description: None,
            tags: vec!["fixture".into(), "wake".into()],
            effects: vec!["may_wake".into(), "may_stay_silent".into()],
            docs_url: None,
            options_schema: None,
        },
    };
    let json = serde_json::to_string(&manifest).expect("manifest 可序列化");
    CString::new(json).expect("manifest 没有 NUL").into_raw()
}

unsafe extern "C" fn wake_create(config_json: *const c_char, _host: *const HostV1) -> *mut c_void {
    if config_json.is_null() {
        return std::ptr::null_mut();
    }
    let config = unsafe { CStr::from_ptr(config_json) }
        .to_string_lossy()
        .into_owned();
    match decode_line::<WireInstanceConfig>(&config) {
        Ok(WireInstanceConfig::Provider { .. }) => {}
        _ => return std::ptr::null_mut(),
    }
    Box::into_raw(Box::new(Fixture {
        stop: Arc::new(AtomicBool::new(false)),
    })) as *mut c_void
}

unsafe extern "C" fn wake_invoke(handle: *mut c_void, request_json: *const c_char) -> *mut c_char {
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
        CoreMessage::Decide {
            id,
            point,
            request,
            ..
        } => {
            if point != "wake" {
                AdapterMessage::Reply {
                    id,
                    result: WireResult::Error {
                        error: WireError::Unsupported {
                            detail: format!("不支持的策略：{point}"),
                        },
                    },
                }
            } else {
                let request: WireWakeRequest = match serde_json::from_value(request) {
                    Ok(request) => request,
                    Err(_) => return std::ptr::null_mut(),
                };
                // 简单规则：冲我来，或者话里带 fixture，就醒。
                let wake = request.at_self || request.text.contains("fixture");
                AdapterMessage::Reply {
                    id,
                    result: WireResult::Decision {
                        decision: if wake { "wake" } else { "stay" }.into(),
                        reason: wake.then(|| "fixture：@ 或关键词命中".to_owned()),
                    },
                }
            }
        }
        CoreMessage::Describe { id }
        | CoreMessage::Send { id, .. }
        | CoreMessage::Call { id, .. } => AdapterMessage::Reply {
            id,
            result: WireResult::Error {
                error: WireError::Unsupported {
                    detail: "这个 fixture 只实现 wake".into(),
                },
            },
        },
        CoreMessage::Shutdown => {
            fixture.stop.store(true, Ordering::Relaxed);
            return std::ptr::null_mut();
        }
    };

    let json = encode_line(&reply).expect("回执可序列化");
    CString::new(json).expect("回执没有 NUL").into_raw()
}

unsafe extern "C" fn wake_destroy(handle: *mut c_void) {
    if handle.is_null() {
        return;
    }
    let fixture = unsafe { Box::from_raw(handle as *mut Fixture) };
    fixture.stop.store(true, Ordering::Relaxed);
}

unsafe extern "C" fn wake_free_string(text: *mut c_char) {
    if text.is_null() {
        return;
    }
    drop(unsafe { CString::from_raw(text) });
}
