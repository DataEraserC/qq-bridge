//! 可独立构建的动态库 wake provider 示例。
//!
//! 桥只认一个导出符号 `acb_adapter_entry_v1` 和一张 C 函数表；manifest、create、
//! invoke、destroy 全部通过 `acb-adapter-api` 的 JSON wire 通信。这个示例实现
//! 最简单的一条规则：`at_self` / `reply_to_self`，或者正文包含
//! `options.extra.keyword`（默认 `hello`）就醒，其余保持沉默。
//!
//! 构建与配置见 `examples/README.md`。

use std::ffi::{CStr, CString, c_char, c_void};

use acb_adapter_api::abi::{ABI_VERSION, AdapterV1, HostV1};
use acb_adapter_api::{
    AdapterMessage, CoreMessage, WireError, WireInstanceConfig, WireManifest, WirePluginMetadata,
    WireResult, WireWakeRequest, decode_line, encode_line,
};

struct Example;

static TABLE: AdapterV1 = AdapterV1 {
    abi_version: ABI_VERSION,
    manifest: Some(example_manifest),
    create: Some(example_create),
    invoke: Some(example_invoke),
    destroy: Some(example_destroy),
    free_string: Some(example_free_string),
};

/// 桥用 libloading 找的唯一符号。
#[unsafe(no_mangle)]
pub extern "C" fn acb_adapter_entry_v1() -> *const AdapterV1 {
    &TABLE
}

unsafe extern "C" fn example_manifest() -> *mut c_char {
    let manifest = WireManifest {
        id: "example.wake".into(),
        platform: String::new(),
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
            title: Some("示例 wake provider".into()),
            summary: Some("关键词或 @/回复命中就醒的动态库示例".into()),
            description: Some("演示动态库策略插件的最小 ABI 与 wire".into()),
            tags: vec!["example".into(), "wake".into()],
            effects: vec!["may_wake".into(), "may_stay_silent".into()],
            docs_url: None,
            options_schema: None,
        },
    };
    let json = serde_json::to_string(&manifest).expect("manifest 可序列化");
    CString::new(json).expect("manifest 没有 NUL").into_raw()
}

unsafe extern "C" fn example_create(config_json: *const c_char, _host: *const HostV1) -> *mut c_void {
    if config_json.is_null() {
        return std::ptr::null_mut();
    }
    let config = unsafe { CStr::from_ptr(config_json) }
        .to_string_lossy()
        .into_owned();
    match decode_line::<WireInstanceConfig>(&config) {
        Ok(WireInstanceConfig::Provider { extension, .. }) if extension == "wake" => {
            Box::into_raw(Box::new(Example)) as *mut c_void
        }
        _ => std::ptr::null_mut(),
    }
}

unsafe extern "C" fn example_invoke(handle: *mut c_void, request_json: *const c_char) -> *mut c_char {
    if handle.is_null() || request_json.is_null() {
        return std::ptr::null_mut();
    }
    let text = unsafe { CStr::from_ptr(request_json) }
        .to_string_lossy()
        .into_owned();
    let message: CoreMessage = match decode_line(&text) {
        Ok(message) => message,
        Err(_) => return std::ptr::null_mut(),
    };

    let reply = match message {
        CoreMessage::Decide {
            id,
            point,
            options,
            request,
        } => {
            if point != "wake" {
                AdapterMessage::Reply {
                    id,
                    result: WireResult::Error {
                        error: WireError::Unsupported {
                            detail: format!("这个示例只实现 wake，收到 {point}"),
                        },
                    },
                }
            } else {
                let request: WireWakeRequest = match serde_json::from_value(request) {
                    Ok(request) => request,
                    Err(_) => return std::ptr::null_mut(),
                };
                let keyword = options
                    .get("extra")
                    .and_then(|extra| extra.get("keyword"))
                    .and_then(|value| value.as_str())
                    .unwrap_or("hello");
                let wake = request.at_self
                    || request.reply_to_self
                    || (!keyword.is_empty() && request.text.contains(keyword));
                AdapterMessage::Reply {
                    id,
                    result: WireResult::Decision {
                        decision: if wake { "wake" } else { "stay" }.into(),
                        reason: wake.then(|| format!("示例：@/回复或关键词 {keyword} 命中")),
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
                    detail: "这个示例只实现 wake".into(),
                },
            },
        },
        CoreMessage::Shutdown => return std::ptr::null_mut(),
    };

    let json = encode_line(&reply).expect("回执可序列化");
    CString::new(json).expect("回执没有 NUL").into_raw()
}

unsafe extern "C" fn example_destroy(handle: *mut c_void) {
    if handle.is_null() {
        return;
    }
    drop(unsafe { Box::from_raw(handle as *mut Example) });
}

unsafe extern "C" fn example_free_string(text: *mut c_char) {
    if text.is_null() {
        return;
    }
    drop(unsafe { CString::from_raw(text) });
}
