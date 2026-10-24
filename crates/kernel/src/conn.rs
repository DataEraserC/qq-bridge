//! 连接池与事件路由。
//!
//! 连接管理是**每账号一个状态机**：失败退避、成功就绪、断开再退避。这一层只有状态与
//! 时间，不含 socket——真正的读写由调用方驱动，所以在没有网络的测试里也能把重连时序跑
//! 完。见 `docs/architecture/03-kernel.md#连接管理`。
//!
//! 路由回答两个问题：这条事件属于哪些设备、对每个设备是不是「自己发的」。后者是不变量一
//! 的执行位置，**必须在钩子之前**——见 `docs/architecture/07-invariants.md`。

use std::collections::BTreeMap;

use time::{Duration, OffsetDateTime};

use crate::config::{Account, Config, Device, ScopeConfig};
use crate::onebot::MessageEvent;

/// 退避的起点与上限。上限固定，否则一次长时间断网会让下一次重连等到天荒地老。
pub const BACKOFF_BASE: Duration = Duration::seconds(1);
pub const BACKOFF_CAP: Duration = Duration::minutes(5);

/// 一条连接的状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkState {
    /// 正在建连（含握手）。
    Connecting,
    /// 已就绪，能收事件、能发动作。
    Ready,
    /// 失败后等下一次尝试。
    Backoff {
        /// 第几次连续失败，从 1 开始。成功一次即清零。
        attempt: u32,
        /// 到点前不再尝试。
        until: OffsetDateTime,
    },
}

impl LinkState {
    pub fn is_ready(&self) -> bool {
        matches!(self, Self::Ready)
    }
}

/// 一条连接的记录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    /// 配置里的账号标识。
    pub account: String,
    pub self_id: String,
    pub state: LinkState,
}

/// 全部账号的连接状态。
#[derive(Debug)]
pub struct Pool {
    /// 账号标识 → 连接。顺序按配置，发送轮换依赖这个顺序。
    order: Vec<String>,
    links: BTreeMap<String, Link>,
}

impl Pool {
    pub fn new(accounts: &[Account]) -> Self {
        let mut links = BTreeMap::new();
        for account in accounts {
            links.insert(
                account.id.clone(),
                Link {
                    account: account.id.clone(),
                    self_id: account.self_id.clone(),
                    // 启动即视为要连：调用方马上会去建连。
                    state: LinkState::Connecting,
                },
            );
        }
        Self {
            order: accounts.iter().map(|account| account.id.clone()).collect(),
            links,
        }
    }

    pub fn state(&self, account: &str) -> Option<&LinkState> {
        self.links.get(account).map(|link| &link.state)
    }

    /// 已就绪的账号，按配置顺序。
    pub fn ready_accounts(&self) -> Vec<&str> {
        self.order
            .iter()
            .filter(|account| self.links[*account].state.is_ready())
            .map(String::as_str)
            .collect()
    }

    /// 到点的退避项，调用方据此逐个重新建连。
    pub fn retry_due(&self, now: OffsetDateTime) -> Vec<&str> {
        self.order
            .iter()
            .filter(|account| match self.links[*account].state {
                LinkState::Backoff { until, .. } => now >= until,
                _ => false,
            })
            .map(String::as_str)
            .collect()
    }

    /// 记录一次建连成功。连续失败计数清零。
    pub fn ready(&mut self, account: &str) {
        if let Some(link) = self.links.get_mut(account) {
            link.state = LinkState::Ready;
        }
    }

    /// 记录一次失败或断开，安排下一次尝试。
    ///
    /// 已就绪的连接断掉也算一次失败：从第 1 次退避重新数，这样掉线一秒后就能重连，而不是
    /// 接着上次的长退避等下去。
    pub fn failed(&mut self, account: &str, now: OffsetDateTime) {
        let Some(link) = self.links.get_mut(account) else {
            return;
        };
        let attempt = match link.state {
            LinkState::Backoff { attempt, .. } => attempt.saturating_add(1),
            _ => 1,
        };
        link.state = LinkState::Backoff {
            attempt,
            until: now + backoff(attempt),
        };
    }

    /// 开始一次新的建连尝试。
    pub fn connecting(&mut self, account: &str) {
        if let Some(link) = self.links.get_mut(account) {
            link.state = LinkState::Connecting;
        }
    }

    /// 某账号的 QQ 号。
    pub fn self_id(&self, account: &str) -> Option<&str> {
        self.links.get(account).map(|link| link.self_id.as_str())
    }
}

/// 第 `attempt` 次连续失败的等待时长。
///
/// 指数退避、封顶。`attempt` 从 1 开始，所以第一次等一个 base，不是零。
pub fn backoff(attempt: u32) -> Duration {
    // `Duration::saturating_mul` 收 i32：封顶之后乘数多大都不再有意义，直接夹住即可。
    let shift = attempt.saturating_sub(1).min(15) as i32;
    let scaled = BACKOFF_BASE.saturating_mul(1i32 << shift);
    if scaled > BACKOFF_CAP {
        BACKOFF_CAP
    } else {
        scaled
    }
}

/// 一条事件该送到哪些设备的哪些范围。
///
/// 返回的顺序即处理顺序：配置里设备在前、范围在设备内的位置的顺序。同一个事件进入两台
/// 设备时分别归档、分别判定——它们本来是两份记忆。
#[derive(Debug, Clone)]
pub struct Route<'a> {
    pub device: &'a Device,
    pub scope: &'a ScopeConfig,
    /// 这个群在这台设备上收它的账号，即收到这条事件的那条连接。
    ///
    /// 持有 `String` 而不是借调用方的 `&str`：账号标识来自连接状态，生命周期不必与配置
    /// 绑定，调用方也不用为此多活一层。
    pub account: String,
    /// 发送者属于这台设备声明的账号集合（不变量一）。
    pub own: bool,
}

/// 按配置把一条群消息路由到各设备。
///
/// 只返回 `receive` 里含这条连接所属账号、且群号匹配的范围。没被任何范围收下的群消息
/// 直接丢弃——它不在这个部署的管理范围内。
pub fn route<'a>(config: &'a Config, event: &MessageEvent, account: &str) -> Vec<Route<'a>> {
    let mut routes = Vec::new();

    for device in &config.devices {
        let own = device_is_sender(config, device, &event.user_id);

        for scope in &device.scopes {
            if scope.group != event.group_id {
                continue;
            }
            // 收到这条事件的连接，是否在这个范围的接收列表里。
            if !scope.receive.iter().any(|candidate| candidate == account) {
                continue;
            }
            routes.push(Route {
                device,
                scope,
                account: account.to_owned(),
                own,
            });
        }
    }

    routes
}

/// 发言者是否属于这台设备的账号集合。
///
/// 设备声明的是账号**标识**，事件里给的是 QQ 号，所以要先翻一次。
fn device_is_sender(config: &Config, device: &Device, sender_id: &str) -> bool {
    device
        .accounts
        .iter()
        .filter_map(|account| config.accounts.iter().find(|known| &known.id == account))
        .any(|known| known.self_id == sender_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_792_561_540).expect("合法时间戳")
    }

    fn accounts() -> Vec<Account> {
        vec![
            Account {
                id: "main".to_owned(),
                self_id: "10001".to_owned(),
                ws_url: "ws://127.0.0.1:6700".to_owned(),
                token: None,
            },
            Account {
                id: "backup".to_owned(),
                self_id: "10002".to_owned(),
                ws_url: "ws://127.0.0.1:6701".to_owned(),
                token: None,
            },
        ]
    }

    #[test]
    fn pool_starts_connecting_and_reports_ready() {
        let mut pool = Pool::new(&accounts());
        assert_eq!(pool.state("main"), Some(&LinkState::Connecting));
        assert!(pool.ready_accounts().is_empty());

        pool.ready("main");
        assert_eq!(pool.ready_accounts(), vec!["main"]);
        // 顺序跟着配置，不按字母。
        pool.ready("backup");
        assert_eq!(pool.ready_accounts(), vec!["main", "backup"]);
    }

    #[test]
    fn backoff_grows_then_caps() {
        assert_eq!(backoff(1), Duration::seconds(1));
        assert_eq!(backoff(2), Duration::seconds(2));
        assert_eq!(backoff(3), Duration::seconds(4));
        // 封顶，不会一直翻倍。
        assert_eq!(backoff(30), BACKOFF_CAP);
        assert_eq!(backoff(1000), BACKOFF_CAP);
    }

    #[test]
    fn failure_schedules_retry_and_clears_on_success() {
        let mut pool = Pool::new(&accounts());
        pool.failed("main", now());
        assert_eq!(
            pool.state("main"),
            Some(&LinkState::Backoff {
                attempt: 1,
                until: now() + Duration::seconds(1),
            })
        );
        // 还没到点。
        assert!(pool.retry_due(now()).is_empty());
        assert_eq!(pool.retry_due(now() + Duration::seconds(2)), vec!["main"]);

        // 连续失败：计数递增、等待变长。
        pool.failed("main", now());
        assert!(matches!(
            pool.state("main"),
            Some(LinkState::Backoff { attempt: 2, .. })
        ));

        pool.ready("main");
        // 成功一次即清零：下次断线从 1 秒重新数。
        pool.failed("main", now());
        assert!(matches!(
            pool.state("main"),
            Some(LinkState::Backoff { attempt: 1, .. })
        ));
    }

    #[test]
    fn ready_link_dropping_restarts_short_backoff() {
        // 稳定跑了很久的连接掉线，不该接着旧的长退避等。
        let mut pool = Pool::new(&accounts());
        for _ in 0..8 {
            pool.failed("main", now());
        }
        pool.ready("main");
        pool.failed("main", now());

        assert_eq!(
            pool.state("main"),
            Some(&LinkState::Backoff {
                attempt: 1,
                until: now() + Duration::seconds(1),
            })
        );
    }

    #[test]
    fn accounts_reconnect_independently() {
        let mut pool = Pool::new(&accounts());
        pool.ready("main");
        pool.failed("backup", now());
        // 一条断了不影响另一条。
        assert_eq!(pool.ready_accounts(), vec!["main"]);
        assert!(
            pool.retry_due(now() + Duration::seconds(2))
                .contains(&"backup")
        );
    }

    fn message(group: &str, sender: &str) -> MessageEvent {
        MessageEvent {
            self_id: "10001".to_owned(),
            group_id: group.to_owned(),
            message_id: "7451".to_owned(),
            user_id: sender.to_owned(),
            sender_name: None,
            text: "在吗".to_owned(),
            at_self: false,
            time: 0,
        }
    }

    const DOC: &str = include_str!("../../../crates/kernel/tests/fixtures/documented.toml");

    fn config() -> Config {
        Config::parse(DOC).expect("文档里的配置示例必须能解析")
    }

    #[test]
    fn routes_only_to_scopes_that_receive_that_account() {
        let config = config();
        let routes = route(&config, &message("20001", "30002"), "main");
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].device.id, "alice-phone");
        assert_eq!(routes[0].scope.group, "20001");
        assert_eq!(routes[0].account, "main");
        assert!(!routes[0].own, "群友不是设备账号");

        // backup 收的是 20003，不是 20001。
        assert!(route(&config, &message("20001", "30002"), "backup").is_empty());
    }

    #[test]
    fn unsubscribed_group_is_dropped() {
        let config = config();
        assert!(route(&config, &message("29999", "30002"), "main").is_empty());
    }

    #[test]
    fn own_sender_is_marked_per_device() {
        // backup 是 alice-pc 的账号；它发言时，对 alice-phone 也是「自己」吗？
        // 不是：alice-phone 的账号集合是 main + backup，所以才算。
        let config = config();
        let routes = route(&config, &message("20001", "10002"), "main");
        assert_eq!(routes.len(), 1);
        assert!(routes[0].own, "10002 属于该设备声明的账号集合");

        let routes = route(&config, &message("20003", "10001"), "backup");
        assert_eq!(routes.len(), 1);
        // alice-pc 的账号集合只有 backup，main 的 10001 对它来说是群友。
        assert!(!routes[0].own);
    }

    #[test]
    fn one_message_can_route_to_two_devices() {
        // 两台设备各用不同账号收同一个群：两份记忆各归档一份。
        let text = r#"
[[accounts]]
id = "main"
self_id = "10001"
ws_url = "ws://127.0.0.1:6700"

[[accounts]]
id = "backup"
self_id = "10002"
ws_url = "ws://127.0.0.1:6701"

[[personas]]
id = "p"
text = "x"

[[devices]]
id = "phone"
persona = "p"
memory = "alice"
accounts = ["main"]
  [[devices.scopes]]
  group = "20001"
  receive = ["main"]
  send = ["main"]

[[devices]]
id = "pc"
persona = "p"
memory = "bob"
accounts = ["backup"]
  [[devices.scopes]]
  group = "20001"
  receive = ["backup"]
  send = ["backup"]
"#;
        let config = Config::parse(text).expect("应能解析");

        let from_main = route(&config, &message("20001", "30002"), "main");
        assert_eq!(from_main.len(), 1);
        assert_eq!(from_main[0].device.memory, "alice");

        let from_backup = route(&config, &message("20001", "30002"), "backup");
        assert_eq!(from_backup.len(), 1);
        assert_eq!(from_backup[0].device.memory, "bob");
    }
}
