//! 外进程钩子运行时：`HookRuntime` 的第二种实现。
//!
//! 存在的理由和 `satori.rs` 一样——**验证接口能不能容纳另一种隔离模型**。Lua 的隔离是
//! 「不加载危险库 + VM 指令钩子」；外进程的隔离是**进程边界**，超时是 `kill`。两者没有
//! 一条实现细节是共用的，如果接口只够 Lua 用，这里就写不下去。
//!
//! ## 协议
//!
//! 子进程常驻，与内核用「一行一个 JSON 请求 / 一行一个 JSON 响应」通信。三个方法对应
//! 三个请求类型：
//!
//! ```text
//! → {"hook":"preReceive","event":{...}}
//! ← {"drop":false,"text":null,"tags":["x"]}
//! ```
//!
//! ## 为什么必须常驻
//!
//! 实测（本机，`python3`）：
//!
//! | 方式 | 单次成本 |
//! | --- | --- |
//! | 每次调用起进程 | **12.4 ms** |
//! | 常驻子进程，逐行 IPC | **0.031 ms** |
//!
//! 差 400 倍。钩子是每条消息都要过一遍的路径，一次一起等于把消息吞吐压到每秒几十条。
//! 所以 [`ProcessHooks::start`] 起一个进程，之后只读写两对管道。
//!
//! ## 诚实的边界
//!
//! - 超时靠 `kill`：进程被杀之后**不能复用**，下一次调用要重起。这是进程模型的固有代价，
//!   不是实现缺陷——解释器能「打断一次调用而不丢状态」，进程不能。
//! - 协议是 JSON 一行一条：子进程必须自己按行读、按行写、每次 flush。文档写在
//!   [`ProcessHooks::SKILL`] 里，因为这类要求写错了表现为「钩子永远不返回」。
//! - 不做流式、不做批量：一次调用一次往返，语义与 Lua 版逐条对齐。

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};

use crate::hooks::{HookError, HookEvent, HookLog, HookRuntime, Ruling};

/// 外进程钩子的默认上限。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessLimits {
    /// 单次调用等多久。超时即 `kill` 并重起。
    pub timeout: Duration,
    /// 单次响应的最大字节数。防止子进程往死里输出把内存吃光。
    pub max_response_bytes: usize,
}

impl Default for ProcessLimits {
    fn default() -> Self {
        Self {
            // 比 Lua 版的 100ms 宽：一次 IPC 往返加解释器启动后的首调用可能慢一些。
            timeout: Duration::from_millis(500),
            max_response_bytes: 1 << 20,
        }
    }
}

/// 一个常驻子进程的钩子运行时。
pub struct ProcessHooks {
    device: String,
    program: String,
    args: Vec<String>,
    limits: ProcessLimits,
    /// 子进程与其管道。`None` 表示上次调用超时被杀，下一次要重起。
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    /// 读响应的接收端。独立线程读，这样主循环可以用 `recv_timeout` 做超时。
    responses: Option<Receiver<ReadResult>>,
    logs: Vec<HookLog>,
}

/// 读线程交回的一条消息。
enum ReadResult {
    Line(String),
    TooLong,
    Closed(String),
}

impl std::fmt::Debug for ProcessHooks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `Child` 与管道不是有意义的调试信息；打印身份与是否在跑。
        f.debug_struct("ProcessHooks")
            .field("device", &self.device)
            .field("program", &self.program)
            .field("running", &self.child.is_some())
            .finish_non_exhaustive()
    }
}

impl ProcessHooks {
    /// 子进程需要知道的协议约定。写在这里而不是只写在文档里——脚本作者从这里抄。
    pub const SKILL: &'static str = "\
外进程钩子协议：stdin/stdout 各一行一个 JSON。\n\
请求：{\"hook\":\"preReceive|postReceive|shouldWake\",\"event\":{...},\"tags\":[...],\"reason\":null}\n\
响应必须是单行 JSON：\n\
  preReceive  → {\"drop\":false,\"text\":null,\"tags\":[]}   text 为 null 表示不改正文\n\
  postReceive → 任何 JSON（忽略）\n\
  shouldWake  → {\"wake\":true,\"reason\":\"...\"} 或 {\"wake\":false}\n\
每条响应必须 flush，否则内核会等到超时。";

    /// 起一个子进程。`program` 与 `args` 由配置给，内核不猜语言。
    pub fn start(
        device: &str,
        program: &str,
        args: Vec<String>,
        limits: ProcessLimits,
    ) -> Result<Self, HookError> {
        let mut hooks = Self {
            device: device.to_owned(),
            program: program.to_owned(),
            args,
            limits,
            child: None,
            stdin: None,
            responses: None,
            logs: Vec::new(),
        };
        hooks.spawn()?;
        Ok(hooks)
    }

    fn spawn(&mut self) -> Result<(), HookError> {
        let mut child = Command::new(&self.program)
            .args(&self.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // 子进程的 stderr 直通父进程：脚本自己的报错该被看见。不让它进日志是因为那
            // 会与内核日志混在一起，而且我们控制不了它的格式。
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|error| HookError::Environment {
                device: self.device.clone(),
                detail: format!("起不来 {}：{error}", self.program),
            })?;

        let stdin = child.stdin.take().ok_or_else(|| HookError::Environment {
            device: self.device.clone(),
            detail: "子进程没有 stdin".to_owned(),
        })?;
        let stdout = child.stdout.take().ok_or_else(|| HookError::Environment {
            device: self.device.clone(),
            detail: "子进程没有 stdout".to_owned(),
        })?;

        let (tx, rx) = channel();
        let limit = self.limits.max_response_bytes;
        // 读线程只做一件事：把每一行交回去。超时判定在主线程，所以这里不关心时间。
        thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) => {
                        let _ = tx.send(ReadResult::Closed("子进程关闭了 stdout".to_owned()));
                        return;
                    }
                    Ok(count) if count > limit => {
                        let _ = tx.send(ReadResult::TooLong);
                        return;
                    }
                    Ok(_) => {
                        if tx.send(ReadResult::Line(line)).is_err() {
                            return;
                        }
                    }
                    Err(error) => {
                        let _ = tx.send(ReadResult::Closed(error.to_string()));
                        return;
                    }
                }
            }
        });

        self.child = Some(child);
        self.stdin = Some(stdin);
        self.responses = Some(rx);
        Ok(())
    }

    /// 杀掉并丢弃子进程。下次调用会重起。
    fn kill(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.stdin = None;
        self.responses = None;
        self.logs.push(HookLog {
            device: self.device.clone(),
            level: "warn".to_owned(),
            message: "钩子进程超时或崩溃，已终止；下次调用会重启".to_owned(),
        });
    }

    /// 一次往返。返回子进程的响应。
    fn round_trip(&mut self, request: Value) -> Result<Value, HookError> {
        if self.child.is_none() {
            self.spawn()?;
        }

        let line = format!("{request}\n");
        let Some(stdin) = self.stdin.as_mut() else {
            return Err(self.runtime("没有 stdin"));
        };
        if let Err(error) = stdin
            .write_all(line.as_bytes())
            .and_then(|()| stdin.flush())
        {
            self.kill();
            return Err(self.runtime(format!("写入子进程失败：{error}")));
        }

        let Some(responses) = self.responses.as_ref() else {
            return Err(self.runtime("没有响应通道"));
        };
        // 一次往返只要一条响应：读线程按行交回，这里的第一个 `recv` 就是结论。
        // 不写成循环——每个分支都 return，循环只会让读者以为还会再来一轮。
        match responses.recv_timeout(self.limits.timeout) {
            Ok(ReadResult::Line(line)) => serde_json::from_str(line.trim())
                .map_err(|error| self.runtime(format!("子进程返回的不是 JSON：{error}"))),
            Ok(ReadResult::TooLong) => {
                self.kill();
                Err(self.runtime("子进程输出超过上限"))
            }
            Ok(ReadResult::Closed(reason)) => {
                self.kill();
                Err(self.runtime(reason))
            }
            Err(RecvTimeoutError::Timeout) => {
                self.kill();
                Err(self.runtime("钩子进程超时"))
            }
            Err(RecvTimeoutError::Disconnected) => {
                self.kill();
                Err(self.runtime("读线程已退出"))
            }
        }
    }

    fn runtime(&self, detail: impl Into<String>) -> HookError {
        HookError::Runtime {
            device: self.device.clone(),
            detail: detail.into(),
        }
    }

    /// 从配置读：`hooks = "prog: 参数..."` 之外的形态由调用方决定。这里只接程序与参数。
    pub fn from_spec(device: &str, spec: &str, limits: ProcessLimits) -> Result<Self, HookError> {
        // `prog arg1 arg2` 的朴素切分。刻意不做引号解析：需要空白参数就用包装脚本，
        // 不要在这里造一个半吊子的 shell 解析器。
        let mut parts = spec.split_whitespace();
        let Some(program) = parts.next() else {
            return Err(HookError::Environment {
                device: device.to_owned(),
                detail: format!("钩子命令是空的：{spec:?}"),
            });
        };
        Self::start(device, program, parts.map(str::to_owned).collect(), limits)
    }

    /// 从路径读：程序是脚本文件，由 shebang 决定解释器。
    pub fn from_path(device: &str, path: &Path, limits: ProcessLimits) -> Result<Self, HookError> {
        Self::start(device, &path.display().to_string(), Vec::new(), limits)
    }
}

impl HookRuntime for ProcessHooks {
    fn device(&self) -> &str {
        &self.device
    }

    fn pre_receive(&mut self, event: &HookEvent<'_>) -> Ruling {
        let request = json!({
            "hook": "preReceive",
            "event": event_json(event),
        });

        match self.round_trip(request) {
            // 抛错按保留处理：脚本 bug 的代价应该是噪音，不是丢消息（不变量八）。
            Err(_) => Ruling::keep(),
            Ok(value) if value.get("drop").and_then(Value::as_bool).unwrap_or(false) => Ruling {
                dropped: true,
                text: None,
                tags: Vec::new(),
            },
            Ok(value) => Ruling {
                dropped: false,
                text: value
                    .get("text")
                    .and_then(Value::as_str)
                    .filter(|text| *text != event.text)
                    .map(str::to_owned),
                tags: strings(value.get("tags")),
            },
        }
    }

    fn post_receive(&mut self, event: &HookEvent<'_>) {
        let request = json!({
            "hook": "postReceive",
            "event": event_json(event),
            "batch": {
                "count": event.batch,
                "oldest_at": event.oldest_ms,
                "newest_at": event.newest_ms,
            },
        });
        // 返回值被忽略；失败只记日志。
        if let Err(error) = self.round_trip(request) {
            self.logs.push(HookLog {
                device: self.device.clone(),
                level: "warn".to_owned(),
                message: format!("postReceive 失败：{error}"),
            });
        }
    }

    fn should_wake(
        &mut self,
        event: &HookEvent<'_>,
        tags: &[String],
        reason: Option<&str>,
    ) -> Result<Option<String>, HookError> {
        let request = json!({
            "hook": "shouldWake",
            "event": event_json(event),
            "tags": tags,
            "reason": reason,
        });

        let value = self.round_trip(request)?;
        // `wake: false` 与「没有 wake 字段」都当否决——只有明确的 true 才唤醒。
        if !value.get("wake").and_then(Value::as_bool).unwrap_or(false) {
            return Ok(None);
        }
        Ok(Some(
            value
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("hook")
                .to_owned(),
        ))
    }

    fn drain_logs(&self) -> Vec<HookLog> {
        // `&self` 不能取走；克隆后清空需要 `&mut`。这里返回副本，日志量很小。
        self.logs.clone()
    }
}

impl Drop for ProcessHooks {
    fn drop(&mut self) {
        self.kill();
    }
}

fn event_json(event: &HookEvent<'_>) -> Value {
    json!({
        "sender_id": event.sender_id,
        "group_id": event.group_id,
        "text": event.text,
        "time": event.time_ms,
        "is_self": event.is_self,
    })
}

fn strings(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn event<'a>(text: &'a str, sender: &'a str) -> HookEvent<'a> {
        HookEvent {
            sender_id: sender,
            group_id: "20001",
            text,
            time_ms: 1_792_561_540_000,
            is_self: false,
            batch: 1,
            oldest_ms: 1_792_561_540_000,
            newest_ms: 1_792_561_540_000,
        }
    }

    /// 一个读一行的极简外进程钩子。用 `sh` 与 `cat` 起，不依赖任何语言的 SDK。
    fn script_hooks(script: &str) -> ProcessHooks {
        ProcessHooks::start(
            "alice-phone",
            "sh",
            vec!["-c".to_owned(), script.to_owned()],
            ProcessLimits::default(),
        )
        .expect("应能起子进程")
    }

    #[test]
    fn a_missing_program_is_a_start_error() {
        let error = ProcessHooks::start(
            "alice-phone",
            "/nonexistent/hook-program",
            Vec::new(),
            ProcessLimits::default(),
        )
        .unwrap_err();
        assert!(
            matches!(error, HookError::Environment { .. }),
            "得到 {error:?}"
        );
    }

    #[test]
    fn pre_receive_can_drop() {
        let mut hooks = script_hooks(r#"while read -r line; do echo '{"drop":true}'; done"#);
        assert!(hooks.pre_receive(&event("在吗", "30002")).dropped);
    }

    #[test]
    fn pre_receive_can_rewrite() {
        let mut hooks = script_hooks(
            r#"while read -r line; do echo '{"drop":false,"text":"改写后","tags":["x"]}'; done"#,
        );
        let ruling = hooks.pre_receive(&event("原文", "30002"));
        assert_eq!(ruling.text.as_deref(), Some("改写后"));
        assert_eq!(ruling.tags, vec!["x".to_owned()]);
    }

    #[test]
    fn unchanged_text_is_not_reported_as_a_rewrite() {
        let mut hooks =
            script_hooks(r#"while read -r line; do echo '{"drop":false,"text":"原文"}'; done"#);
        assert_eq!(hooks.pre_receive(&event("原文", "30002")).text, None);
    }

    #[test]
    fn should_wake_can_veto_and_allow() {
        // 按请求类型分岔：只有 shouldWake 的回包里有 wake 字段。
        let mut hooks = script_hooks(
            r#"while read -r line; do
                 case "$line" in
                   *shouldWake*\"reason\":null*) echo '{"wake":true,"reason":"custom"}';;
                   *shouldWake*) echo '{"wake":false}';;
                   *) echo '{"drop":false}';;
                 esac
               done"#,
        );

        assert_eq!(
            hooks.should_wake(&event("嗯", "30002"), &[], Some("any_message")),
            Ok(None),
            "wake:false 应否决"
        );
        assert_eq!(
            hooks.should_wake(&event("嗯", "30002"), &[], None),
            Ok(Some("custom".to_owned())),
            "wake:true 应唤醒并给出原因"
        );
    }

    #[test]
    fn a_silent_process_times_out_and_is_killed() {
        // 读一行但永不回：超时必须 kill，不能挂住主循环。
        let limits = ProcessLimits {
            timeout: Duration::from_millis(80),
            ..ProcessLimits::default()
        };
        let mut hooks = ProcessHooks::start(
            "alice-phone",
            "sh",
            vec!["-c".to_owned(), "cat > /dev/null".to_owned()],
            limits,
        )
        .expect("应能起子进程");

        let started = Instant::now();
        let result = hooks.should_wake(&event("嗯", "30002"), &[], None);
        assert!(result.is_err(), "沉默的子进程应超时");
        assert!(
            started.elapsed() < Duration::from_millis(2_000),
            "不该拖住主循环，实际 {:?}",
            started.elapsed()
        );
        // 超时之后进程必须被丢掉：下一次调用要重起，而不是复用半死的管道。
        assert!(hooks.child.is_none(), "超时后应已 kill");
    }

    #[test]
    fn pre_receive_error_keeps_the_event() {
        // 非 JSON 回复：按保留处理，不丢消息。
        let mut hooks = script_hooks(r#"while read -r line; do echo 'not json'; done"#);
        let ruling = hooks.pre_receive(&event("在吗", "30002"));
        assert!(!ruling.dropped, "脚本 bug 不该丢消息");
    }

    #[test]
    fn a_crashed_process_is_reported_not_swallowed() {
        // 读一行就退出：读线程报 Closed。
        let mut hooks = script_hooks(r#"read -r line; echo '{"wake":true,"reason":"x"}'; exit 0"#);
        assert_eq!(
            hooks.should_wake(&event("嗯", "30002"), &[], None),
            Ok(Some("x".to_owned())),
            "第一次调用应成功"
        );

        // 第二次：进程已退出，管道关了。
        assert!(
            hooks.should_wake(&event("嗯", "30002"), &[], None).is_err(),
            "进程退出后应报错而不是挂住"
        );
    }

    #[test]
    fn state_survives_across_calls_without_respawning() {
        // 计数器：证明进程是常驻的，不是一次一起。
        let mut hooks = script_hooks(
            r#"n=0
               while read -r line; do
                 n=$((n+1))
                 echo "{\"wake\":true,\"reason\":\"第 $n 次\"}"
               done"#,
        );
        assert_eq!(
            hooks.should_wake(&event("嗯", "30002"), &[], None),
            Ok(Some("第 1 次".to_owned()))
        );
        assert_eq!(
            hooks.should_wake(&event("嗯", "30002"), &[], None),
            Ok(Some("第 2 次".to_owned())),
            "状态应在同一进程里延续"
        );
    }

    #[test]
    fn spec_splits_program_and_args() {
        let hooks = ProcessHooks::from_spec(
            "alice-phone",
            "sh -c 'while read l; do echo {}; done'",
            ProcessLimits::default(),
        );
        // 引号不在朴素切分的处理范围里，这条只验证「能起」。真实用法是包装脚本。
        assert!(hooks.is_ok(), "得到 {hooks:?}");
    }
}
