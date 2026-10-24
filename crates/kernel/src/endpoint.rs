//! 适配器接入点：内核监听在哪儿，适配器往哪儿连。
//!
//! 内核与适配器**不必在同一台机器上**，也不必在同一个操作系统上。三类接入点覆盖这些情形：
//!
//! | 形态 | 写法 | 跨机器 | 说明 |
//! | --- | --- | --- | --- |
//! | 文件路径 | `/run/user/1000/qqab.sock`、`\\.\pipe\qqab` | 否 | Unix 域套接字 / Windows 命名管道 |
//! | 命名空间 | `local:qqab` | 否 | Linux 抽象命名空间、Windows 命名管道命名空间 |
//! | TCP | `tcp://0.0.0.0:7000` | **是** | 跨机器唯一可行的形态 |
//!
//! 前两类由 `interprocess` 统一到一套 API，同一份代码在 Linux、macOS、Windows 上编译并运行；
//! 第三类是本机没有共享文件系统时唯一的选择，因此它的安全约束单独一条：
//! **监听非回环地址必须配令牌**，否则同网段任何人都能冒充适配器接管投递。
//!
//! 解析与绑定分开：本模块只做「这串字符是什么意思」，不碰 IO，所以配置校验与测试都能用它。

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::PathBuf;

/// 接入点的三种形态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Endpoint {
    /// 文件系统上的套接字：Unix 域套接字，或 `\\.\pipe\<名字>` 形式的命名管道。
    Path(PathBuf),
    /// 平台命名空间：Linux 抽象命名空间，Windows 命名管道。
    Namespaced(String),
    /// TCP。跨机器时唯一可用的形态。
    Tcp { host: String, port: u16 },
}

impl Endpoint {
    /// 解析配置里的 `adapter.socket`。
    ///
    /// 判据是「有没有路径分隔符」，而不是「在哪个平台上」——配置可能被另一台机器上的
    /// 部署者写，本机判断平台会把一份有效的跨平台配置读成另一类。
    pub fn parse(text: &str) -> Result<Self, EndpointError> {
        let text = text.trim();
        if text.is_empty() {
            return Err(EndpointError::Empty);
        }

        for prefix in ["tcp://", "tcp:"] {
            if let Some(rest) = text.strip_prefix(prefix) {
                return parse_tcp(rest);
            }
        }

        if let Some(rest) = text.strip_prefix("local:") {
            return named(rest);
        }

        // `\\.\pipe\name`、`C:\dir\file`、`/run/x.sock`：都按文件路径处理。
        if text.contains('/') || text.contains('\\') {
            return Ok(Self::Path(PathBuf::from(text)));
        }

        named(text)
    }

    /// 这个接入点是否可能被别的机器连上。
    ///
    /// 只有 TCP 且监听非回环地址才算。文件路径与命名空间天然限于本机。
    pub fn reaches_other_machines(&self) -> bool {
        match self {
            Self::Tcp { host, .. } => !is_loopback(host),
            Self::Path(_) | Self::Namespaced(_) => false,
        }
    }

    /// 人类可读的形态，日志与错误信息用。
    pub fn describe(&self) -> String {
        match self {
            Self::Path(path) => format!("路径 {}", path.display()),
            Self::Namespaced(name) => format!("命名空间 {name}"),
            Self::Tcp { host, port } => format!("TCP {host}:{port}"),
        }
    }
}

fn named(rest: &str) -> Result<Endpoint, EndpointError> {
    let name = rest.trim();
    if name.is_empty() {
        return Err(EndpointError::EmptyName);
    }
    if name.contains('\0') {
        return Err(EndpointError::BadName(name.to_owned()));
    }
    Ok(Endpoint::Namespaced(name.to_owned()))
}

fn parse_tcp(rest: &str) -> Result<Endpoint, EndpointError> {
    let rest = rest.trim().trim_end_matches('/');
    let (host, port) = rest.rsplit_once(':').ok_or(EndpointError::MissingPort {
        value: rest.to_owned(),
    })?;
    let port: u16 = port.parse().map_err(|_| EndpointError::BadPort {
        value: port.to_owned(),
    })?;
    if port == 0 {
        return Err(EndpointError::BadPort {
            value: "0".to_owned(),
        });
    }
    let host = host.trim_matches(|c| c == '[' || c == ']');
    if host.is_empty() {
        return Err(EndpointError::MissingHost);
    }
    Ok(Endpoint::Tcp {
        host: host.to_owned(),
        port,
    })
}

/// 回环判断。解析不出 IP 的主机名一律按「非回环」处理——DNS 可能指向任何地方，
/// 把它当本机是安全判断上不该有的乐观。
fn is_loopback(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host.parse::<IpAddr>().is_ok_and(|ip| match ip {
            IpAddr::V4(v4) => v4 == Ipv4Addr::LOCALHOST || v4.is_loopback(),
            IpAddr::V6(v6) => v6 == Ipv6Addr::LOCALHOST || v6.is_loopback(),
        })
}

/// 接入点解析失败的原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EndpointError {
    Empty,
    EmptyName,
    BadName(String),
    MissingHost,
    MissingPort { value: String },
    BadPort { value: String },
}

impl std::fmt::Display for EndpointError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => f.write_str("接入点不能为空"),
            Self::EmptyName => f.write_str("命名空间形式的接入器缺少名字"),
            Self::BadName(name) => write!(f, "接入点名 {name:?} 含非法字符"),
            Self::MissingHost => f.write_str("TCP 接入点缺少主机"),
            Self::MissingPort { value } => {
                write!(f, "TCP 接入点 {value:?} 缺少端口，应写成 tcp://主机:端口")
            }
            Self::BadPort { value } => write!(f, "TCP 端口 {value:?} 不是 1..=65535 的数字"),
        }
    }
}

impl std::error::Error for EndpointError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unix_path_is_a_path() {
        assert_eq!(
            Endpoint::parse("/run/user/1000/qqab.sock"),
            Ok(Endpoint::Path(PathBuf::from("/run/user/1000/qqab.sock")))
        );
    }

    #[test]
    fn windows_pipe_is_a_path() {
        // 同一份配置在 Windows 上必须仍然合法：`\\.\pipe\` 是路径形态的命名空间。
        assert_eq!(
            Endpoint::parse(r"\\.\pipe\qqab"),
            Ok(Endpoint::Path(PathBuf::from(r"\\.\pipe\qqab")))
        );
        assert_eq!(
            Endpoint::parse(r"C:\qqab\adapter.sock"),
            Ok(Endpoint::Path(PathBuf::from(r"C:\qqab\adapter.sock")))
        );
    }

    #[test]
    fn bare_name_is_a_namespace() {
        // 没有分隔符的名字在所有平台上都能落实：Linux 抽象命名空间、Windows 命名管道。
        assert_eq!(
            Endpoint::parse("qqab"),
            Ok(Endpoint::Namespaced("qqab".to_owned()))
        );
        assert_eq!(
            Endpoint::parse("local:qqab"),
            Ok(Endpoint::Namespaced("qqab".to_owned()))
        );
    }

    #[test]
    fn tcp_forms_parse() {
        for text in [
            "tcp://127.0.0.1:7000",
            "tcp:127.0.0.1:7000",
            "tcp://127.0.0.1:7000/",
        ] {
            assert_eq!(
                Endpoint::parse(text),
                Ok(Endpoint::Tcp {
                    host: "127.0.0.1".to_owned(),
                    port: 7000
                }),
                "{text} 应解析成 TCP"
            );
        }
        // IPv6 字面量带方括号。
        assert_eq!(
            Endpoint::parse("tcp://[::1]:7000"),
            Ok(Endpoint::Tcp {
                host: "::1".to_owned(),
                port: 7000
            })
        );
    }

    #[test]
    fn only_non_loopback_tcp_reaches_other_machines() {
        // 跨机器支持只有一条路：TCP。路径与命名空间天然限于本机。
        assert!(
            !Endpoint::parse("/tmp/x.sock")
                .unwrap()
                .reaches_other_machines()
        );
        assert!(!Endpoint::parse("qqab").unwrap().reaches_other_machines());
        assert!(
            !Endpoint::parse("tcp://127.0.0.1:7000")
                .unwrap()
                .reaches_other_machines()
        );
        assert!(
            !Endpoint::parse("tcp://localhost:7000")
                .unwrap()
                .reaches_other_machines()
        );
        assert!(
            !Endpoint::parse("tcp://[::1]:7000")
                .unwrap()
                .reaches_other_machines()
        );

        assert!(
            Endpoint::parse("tcp://0.0.0.0:7000")
                .unwrap()
                .reaches_other_machines()
        );
        assert!(
            Endpoint::parse("tcp://192.168.1.5:7000")
                .unwrap()
                .reaches_other_machines()
        );
        // 主机名解析不了就按「非回环」——DNS 可能指向任何地方。
        assert!(
            Endpoint::parse("tcp://adapter.example:7000")
                .unwrap()
                .reaches_other_machines()
        );
    }

    #[test]
    fn empty_forms_are_rejected() {
        assert_eq!(Endpoint::parse("   "), Err(EndpointError::Empty));
        assert_eq!(Endpoint::parse("local:"), Err(EndpointError::EmptyName));
    }

    #[test]
    fn bad_tcp_forms_are_rejected() {
        assert_eq!(
            Endpoint::parse("tcp://127.0.0.1"),
            Err(EndpointError::MissingPort {
                value: "127.0.0.1".to_owned()
            })
        );
        assert_eq!(
            Endpoint::parse("tcp://127.0.0.1:70000"),
            Err(EndpointError::BadPort {
                value: "70000".to_owned()
            })
        );
        assert_eq!(
            Endpoint::parse("tcp://127.0.0.1:0"),
            Err(EndpointError::BadPort {
                value: "0".to_owned()
            })
        );
        assert_eq!(
            Endpoint::parse("tcp://:7000"),
            Err(EndpointError::MissingHost)
        );
    }

    #[test]
    fn describe_names_the_form() {
        assert!(
            Endpoint::parse("/tmp/x.sock")
                .unwrap()
                .describe()
                .contains("路径")
        );
        assert!(
            Endpoint::parse("qqab")
                .unwrap()
                .describe()
                .contains("命名空间")
        );
        assert!(
            Endpoint::parse("tcp://10.0.0.1:7000")
                .unwrap()
                .describe()
                .contains("TCP")
        );
    }
}
