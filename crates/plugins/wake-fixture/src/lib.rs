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
    AdapterMessage, CoreMessage, WireAccessRequest, WireClockRequest, WireDeliveryPlan,
    WireDeliveryRequest, WireError, WireInstanceConfig, WireManifest, WirePluginMetadata,
    WireReceiveRequest, WireResult, WireSendRequest, WireTurnErrorAction, WireTurnErrorRequest,
    WireWakeRequest, decode_line, encode_line,
};
use acb_proto::{Element, Omitted};

struct Fixture {
    stop: Arc<AtomicBool>,
}

/// 同一个 cdylib 也可以当 access / receive / clock 夹具用：manifest 的扩展点由环境变量选，
/// 没设就是 wake。集成测试各用一个独立进程，所以不会互相踩环境变量。
fn fixture_extension() -> String {
    std::env::var("ACB_FIXTURE_EXTENSION").unwrap_or_else(|_| "wake".to_owned())
}

fn fixture_capability(extension: &str) -> &'static str {
    match extension {
        "access" => "access_decision",
        "delivery" => "deliver_plan",
        "send" => "send_hook",
        "turn_error" => "turn_error_decision",
        "receive" => "receive_decision",
        "clock" => "clock_decision",
        _ => "wake_decision",
    }
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
    let extension = fixture_extension();
    let manifest = WireManifest {
        id: format!("fixture.{extension}"),
        platform: "fixture".into(),
        kind: String::new(),
        transports: Vec::new(),
        api_version: 1,
        capabilities: vec![fixture_capability(&extension).to_owned()],
        events: Vec::new(),
        event_tags: Vec::new(),
        supports_at: false,
        supports_reply: false,
        extension: extension.clone(),
        metadata: WirePluginMetadata {
            title: Some(format!("{extension} 夹具")),
            summary: Some(format!("测试用 {extension} 策略夹具")),
            description: None,
            tags: vec!["fixture".into(), extension.clone()],
            effects: if extension == "wake" {
                vec!["may_wake".into(), "may_stay_silent".into()]
            } else {
                Vec::new()
            },
            docs_url: None,
            options_schema: None,
        },
    };
    let json = serde_json::to_string(&manifest).expect("manifest 可序列化");
    CString::new(json)
        .expect("manifest 没有 NUL")
        .into_raw()
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
            options,
        } => {
            let reply = match point.as_str() {
                "wake" => {
                    let request: WireWakeRequest = match serde_json::from_value(request) {
                        Ok(request) => request,
                        Err(_) => return std::ptr::null_mut(),
                    };
                    // 简单规则：冲我来，或者话里带关键词（默认 fixture）就醒。
                    let keyword = options
                        .get("extra")
                        .and_then(|extra| extra.get("keyword"))
                        .and_then(|value| value.as_str())
                        .unwrap_or("fixture");
                    let wake = request.at_self || request.text.contains(keyword);
                    WireResult::Decision {
                        decision: if wake { "wake" } else { "stay" }.into(),
                        reason: wake.then(|| format!("fixture：@ 或关键词 {keyword} 命中")),
                    }
                }
                "access" => {
                    let request: WireAccessRequest = match serde_json::from_value(request) {
                        Ok(request) => request,
                        Err(_) => return std::ptr::null_mut(),
                    };
                    // 简单规则：读放行，写/连接级拒绝；测试两种结果都覆盖。
                    let allow = request.action == "read";
                    WireResult::Decision {
                        decision: if allow { "allow" } else { "deny" }.into(),
                        reason: None,
                    }
                }
                "receive" => {
                    let request: WireReceiveRequest = match serde_json::from_value(request) {
                        Ok(request) => request,
                        Err(_) => return std::ptr::null_mut(),
                    };
                    // 简单规则：正文含 drop 就丢，其余照收。
                    let drop = request.text.contains("drop");
                    WireResult::Decision {
                        decision: if drop { "drop" } else { "record" }.into(),
                        reason: None,
                    }
                }
                "delivery" => {
                    let request: WireDeliveryRequest = match serde_json::from_value(request) {
                        Ok(request) => request,
                        Err(_) => return std::ptr::null_mut(),
                    };
                    // 简单规则：保留最后 N 条（默认 5），其余算跳过。
                    let keep_last = options
                        .get("extra")
                        .and_then(|extra| extra.get("keep_last"))
                        .and_then(|value| value.as_u64())
                        .unwrap_or(5) as usize;
                    let keep_from = request.records.len().saturating_sub(keep_last);
                    let keep = request.records[keep_from..]
                        .iter()
                        .map(|record| record.id)
                        .collect();
                    let omitted = if keep_from > 0 {
                        let middle = &request.records[..keep_from];
                        Some(Omitted {
                            from: middle.first().expect("非空").id,
                            to: middle.last().expect("非空").id,
                            count: middle.len(),
                        })
                    } else {
                        None
                    };
                    WireResult::Delivery {
                        plan: Some(WireDeliveryPlan { keep, omitted }),
                    }
                }
                "turn_error" => {
                    let request: WireTurnErrorRequest = match serde_json::from_value(request) {
                        Ok(request) => request,
                        Err(_) => return std::ptr::null_mut(),
                    };
                    // `extra.only = "log" | "default" | "actions"`；不写就按会话动作优先。
                    let chosen = match options
                        .get("extra")
                        .and_then(|extra| extra.get("only"))
                        .and_then(|value| value.as_str())
                    {
                        Some("log") => vec![WireTurnErrorAction::Log],
                        Some("default") => request.default_actions.clone(),
                        _ if request.actions.is_empty() => request.default_actions.clone(),
                        _ => request.actions.clone(),
                    };
                    WireResult::Actions { actions: chosen }
                }
                "send" => {
                    let request: WireSendRequest = match serde_json::from_value(request) {
                        Ok(request) => request,
                        Err(_) => return std::ptr::null_mut(),
                    };
                    if request.phase == "before" {
                        let mut elements = request.elements;
                        elements.push(Element::text(" [fixture-hook]"));
                        WireResult::Elements { elements }
                    } else {
                        // after 只观察，回一个明确的「收到」。
                        WireResult::Decision {
                            decision: "ok".into(),
                            reason: None,
                        }
                    }
                }
                "clock" => {
                    let request: WireClockRequest = match serde_json::from_value(request) {
                        Ok(request) => request,
                        Err(_) => return std::ptr::null_mut(),
                    };
                    // 简单规则：距上次开轮满 60 秒就检查。
                    let due = request.now.saturating_sub(request.last_clock_at) >= 60_000;
                    WireResult::Decision {
                        decision: if due { "due" } else { "skip" }.into(),
                        reason: None,
                    }
                }
                other => WireResult::Error {
                    error: WireError::Unsupported {
                        detail: format!("不支持的策略：{other}"),
                    },
                },
            };
            AdapterMessage::Reply { id, result: reply }
        }
        CoreMessage::Describe { id }
        | CoreMessage::Send { id, .. }
        | CoreMessage::Call { id, .. } => AdapterMessage::Reply {
            id,
            result: WireResult::Error {
                error: WireError::Unsupported {
                    detail: "这个 fixture 没有实现平台适配器动作".into(),
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
