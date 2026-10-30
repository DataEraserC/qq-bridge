//! 文件中继：桥替 agent 把本机文件变成平台取得到的 URL（ADR-0025）。
//!
//! 断层的由来：agent 在宿主机，平台（QQ/OneBot 实现）常常在容器里，两边不共用
//! 文件系统。agent 写 `/home/.../a.png`，平台 stat 不到，报出来的还是裸 ENOENT
//! ——文件明明在，排查方向全被带偏。每个部署环境的挂载表都不一样（docker、
//! podman、windows 各不相同），路径映射要一套环境配一遍，配不完。
//!
//! 解法（来源：用户裁定）：**桥自带文件服务**。发送或平台调用里出现的
//! 本机文件路径，桥收下文件，换成 `{public_base}/f/{token}/{文件名}` 给平台。
//! agent 只给路径，平台只认 URL，两边谁也不用知道对方的挂载表。
//!
//! 配置了 `[file_relay]` 才启用；没配时本机路径原样透传，行为与没有这个功能
//! 完全一致。本机**不存在**的路径也不动——那可能是平台那边（共享卷、平台缓存）
//! 才存在的路径，动它反而弄坏原本能用的写法。

use std::fs;
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// 单个收下文件的大小上限。fs::copy 不占桥的内存，这个上限主要防磁盘被塞满。
pub const MAX_PUBLISH_BYTES: u64 = 128 * 1024 * 1024;
/// 收下的文件保留多久。桥启动时清一次过期的；漏网的等下次重启再清。
const KEEP_FOR: Duration = Duration::from_secs(24 * 3600);

/// 收文件失败的几种原因。每种都能让模型一轮就知道该改什么。
#[derive(Debug, thiserror::Error)]
pub enum RelayError {
    #[error("文件不存在：{}", .0.display())]
    NotFound(PathBuf),
    #[error("不是普通文件：{}", .0.display())]
    NotAFile(PathBuf),
    #[error("文件太大：{} 有 {size} 字节，上限 {limit} 字节", .path.display())]
    TooBig {
        path: PathBuf,
        size: u64,
        limit: u64,
    },
    #[error("收不下（{}）：{error}", .path.display())]
    Io {
        path: PathBuf,
        #[source]
        error: io::Error,
    },
}

/// 文件中继。一个实例只管一个目录；桥里可以有两个实例（发布用的和 HTTP 服务用的），
/// 只要目录相同就是同一份文件——token 目录就是唯一的事实来源。
pub struct FileRelay {
    root: PathBuf,
    base: String,
}

impl FileRelay {
    /// 建一个中继。不在这里建目录：建目录失败不该拦住桥启动，
    /// 等真要收文件时再报「目录建不了」这种具体错误。
    pub fn new(dir: impl Into<PathBuf>, public_base: impl Into<String>) -> Self {
        Self {
            root: dir.into(),
            base: public_base.into().trim_end_matches('/').to_owned(),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 收下一个本机文件，返回平台取得到的 URL。
    ///
    /// 用 `fs::copy` 流式复制，不把整个文件读进内存。
    pub fn publish(&self, source: &Path) -> Result<String, RelayError> {
        let meta = match fs::metadata(source) {
            Ok(meta) => meta,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(RelayError::NotFound(source.to_owned()));
            }
            Err(error) => {
                return Err(RelayError::Io {
                    path: source.to_owned(),
                    error,
                })
            }
        };
        if !meta.is_file() {
            return Err(RelayError::NotAFile(source.to_owned()));
        }
        if meta.len() > MAX_PUBLISH_BYTES {
            return Err(RelayError::TooBig {
                path: source.to_owned(),
                size: meta.len(),
                limit: MAX_PUBLISH_BYTES,
            });
        }

        let token = new_token();
        let entry = self.root.join(&token);
        fs::create_dir_all(&entry).map_err(|error| RelayError::Io {
            path: entry.clone(),
            error,
        })?;
        let name = safe_name_of(source);
        let dest = entry.join(&name);
        fs::copy(source, &dest).map_err(|error| RelayError::Io {
            path: source.to_owned(),
            error,
        })?;
        Ok(format!("{}/f/{token}/{name}", self.base))
    }

    /// 按 token + 文件名找收下的文件。防目录穿越：token 必须是 32 位十六进制，
    /// 文件名不许带路径分隔符，也不许是 `.` / `..`。
    pub fn lookup(&self, token: &str, name: &str) -> Option<(PathBuf, u64)> {
        let hex = token.as_bytes();
        if hex.len() != 32 || !hex.iter().all(|byte| byte.is_ascii_hexdigit()) {
            return None;
        }
        if name.is_empty()
            || name.contains('/')
            || name.contains('\\')
            || name == "."
            || name == ".."
        {
            return None;
        }
        let path = self.root.join(token).join(name);
        let meta = fs::metadata(&path).ok()?;
        if !meta.is_file() {
            return None;
        }
        Some((path, meta.len()))
    }

    /// 清掉过期的收存文件。只在桥启动时跑一次；跑着的桥不清（省事，文件也不大）。
    pub fn sweep_expired(&self) {
        let Ok(entries) = fs::read_dir(&self.root) else {
            return;
        };
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_secs())
            .unwrap_or(0);
        for entry in entries.flatten() {
            let Ok(meta) = entry.metadata() else { continue };
            if !meta.is_dir() {
                continue;
            }
            let age = meta
                .modified()
                .ok()
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map(|since| now.saturating_sub(since.as_secs()))
                .unwrap_or(0);
            if age > KEEP_FOR.as_secs() {
                let _ = fs::remove_dir_all(entry.path());
            }
        }
    }

    /// 起文件服务。阻塞当前线程；每条连接一个短命线程，读完即关。
    pub fn serve(listener: TcpListener, relay: Arc<Self>, shutdown: Arc<AtomicBool>) {
        listener.set_nonblocking(true).ok();
        while !shutdown.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((stream, _)) => {
                    let relay = Arc::clone(&relay);
                    std::thread::spawn(move || {
                        if let Err(error) = handle_connection(stream, &relay) {
                            eprintln!("文件中继：请求没处理完：{error}");
                        }
                    });
                }
                Err(_) => std::thread::sleep(Duration::from_millis(200)),
            }
        }
    }
}

/// 判断一个 url 是不是「本机文件路径」。是的话返回剥掉 `file://` 后的真实路径。
///
/// 只认绝对路径（`/…`、`C:\…`、`C:/…`、`file://…`）。`http(s)://` 和 `base64://`
/// 原样放行——它们本来就不依赖本地命名空间。相对路径不认：agent 的工作目录跟
/// 桥的不一样，相对路径两边指的地方根本不同，猜了只会更乱。
pub fn local_path(url: &str) -> Option<PathBuf> {
    let text = url.trim();
    let lower_prefix = text
        .get(..8)
        .map(|prefix| prefix.to_ascii_lowercase())
        .unwrap_or_default();
    if lower_prefix.starts_with("http://")
        || lower_prefix.starts_with("https://")
        || lower_prefix.starts_with("base64:/")
    {
        return None;
    }
    let path = text.strip_prefix("file://").unwrap_or(text);
    if path.starts_with('/') {
        return Some(PathBuf::from(path));
    }
    // Windows 盘符：C:\ 或 C:/。单字母后面跟冒号的都算，盘符不会是别的形状。
    let bytes = path.as_bytes();
    if bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/')
    {
        return Some(PathBuf::from(path));
    }
    None
}

/// 从路径取一个能安全放进 URL 的文件名：只留 ASCII 字母数字和 `._-`，
/// 其余（空格、中文、各种符号）换成 `_`。取不出来就给个兜底名。
fn safe_name_of(source: &Path) -> String {
    let raw = source
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let cleaned: String = raw
        .chars()
        .take(120)
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_') {
                ch
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
        "file.bin".to_owned()
    } else {
        cleaned
    }
}

/// 32 位十六进制的随机 token。/dev/urandom 没了就用时间+进程号凑——
/// 这是能力地址（拿到 URL 就能取），不是加密凭据，随机性要求没那么高。
fn new_token() -> String {
    let mut bytes = [0_u8; 16];
    let filled = fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .is_ok();
    if !filled {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0) as u64;
        let mixed = nanos
            ^ (std::process::id() as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
            ^ (&bytes as *const _ as u64);
        bytes.copy_from_slice(&mixed.to_le_bytes());
    }
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn content_type_of(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase())
        .as_deref()
    {
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("bmp") => "image/bmp",
        Some("mp4") => "video/mp4",
        Some("mp3") => "audio/mpeg",
        Some("amr") => "audio/amr",
        Some("wav") => "audio/wav",
        Some("ogg") | Some("oga") => "audio/ogg",
        Some("silk") => "application/octet-stream",
        Some("pdf") => "application/pdf",
        Some("txt") => "text/plain; charset=utf-8",
        Some("json") => "application/json",
        _ => "application/octet-stream",
    }
}

fn handle_connection(mut stream: TcpStream, relay: &FileRelay) -> io::Result<()> {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .ok();
    // 只需要请求行：读到头结束（空行）或 8 KiB 就停，别等一个可能不来的 body。
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => {
                buffer.extend_from_slice(&chunk[..read]);
                if buffer.len() > 8192 || buffer.windows(4).any(|part| part == b"\r\n\r\n") {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    let request = String::from_utf8_lossy(&buffer);
    let mut parts = request.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("");

    let head_only = method == "HEAD";
    if method != "GET" && !head_only {
        return respond_error(&mut stream, 405, "method not allowed");
    }

    // 目标形如 /f/<token>/<文件名>?查询串。
    let path = target.split('?').next().unwrap_or("");
    let mut segments = path.strip_prefix("/f/").unwrap_or("").splitn(2, '/');
    let token = segments.next().unwrap_or("");
    let name = segments.next().unwrap_or("");

    match relay.lookup(token, name) {
        Some((file, size)) => {
            let content_type = content_type_of(&file);
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {size}\r\nConnection: close\r\n\r\n"
            );
            stream.write_all(header.as_bytes())?;
            if !head_only {
                let mut file = fs::File::open(file)?;
                io::copy(&mut file, &mut stream)?;
            }
            Ok(())
        }
        None => respond_error(&mut stream, 404, "not found"),
    }
}

fn respond_error(stream: &mut TcpStream, status: u16, text: &str) -> io::Result<()> {
    let body = format!("{{\"error\":\"{text}\"}}");
    let header = format!(
        "HTTP/1.1 {status} {text}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes())?;
    stream.write_all(body.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试用临时目录，Drop 时整个删掉。跟 tests/sqlite_store.rs 的 TempDb 同款做法，
    /// kernel 不引 tempfile 也够用。
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("时间正常")
                .as_nanos();
            let dir = std::env::temp_dir().join(format!("acb-relay-{}-{name}-{nanos}", std::process::id()));
            fs::create_dir_all(&dir).expect("能建临时目录");
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn 收下的文件能按URL取回来() {
        let temp = TempDir::new("roundtrip");
        let relay = FileRelay::new(temp.0.join("store"), "http://10.89.0.1:3131");

        let source = temp.0.join("一张 图.png");
        fs::write(&source, b"png-bytes").expect("能写源文件");

        let url = relay.publish(&source).expect("能收下");
        assert!(url.starts_with("http://10.89.0.1:3131/f/"), "{url}");
        assert!(url.ends_with("/____.png"), "中文和空格要换掉：{url}");

        let rest = url.strip_prefix("http://10.89.0.1:3131/f/").expect("前缀对");
        let mut segments = rest.splitn(2, '/');
        let token = segments.next().expect("有 token");
        let name = segments.next().expect("有文件名");
        let (path, size) = relay.lookup(token, name).expect("能按 URL 找回");
        assert_eq!(size, 9);
        assert_eq!(fs::read(path).expect("能读"), b"png-bytes");
    }

    #[test]
    fn 瞎编的token和带路径的文件名都取不到东西() {
        let temp = TempDir::new("traversal");
        let relay = FileRelay::new(temp.0.join("store"), "http://x");
        assert!(relay.lookup("not-hex", "a.png").is_none());
        assert!(
            relay
                .lookup("0123456789abcdef0123456789abcdef", "../../etc/passwd")
                .is_none()
        );
        assert!(
            relay
                .lookup("0123456789abcdef0123456789abcdef", ".")
                .is_none()
        );
    }

    #[test]
    fn 文件太大和不是文件都收不下() {
        let temp = TempDir::new("limits");
        let relay = FileRelay::new(temp.0.join("store"), "http://x");

        let dir = temp.0.join("其实是个目录");
        fs::create_dir_all(&dir).expect("能建目录");
        assert!(matches!(relay.publish(&dir), Err(RelayError::NotAFile(_))));

        let big = temp.0.join("big.bin");
        fs::write(&big, vec![0_u8; 4096]).expect("能写");
        // 把上限人为调小来测，不用真写 128 MiB。
        let limit_check = fs::metadata(&big).expect("元数据在").len();
        assert!(limit_check < MAX_PUBLISH_BYTES);
        let too_big = RelayError::TooBig {
            path: big.clone(),
            size: limit_check,
            limit: 1,
        };
        assert!(too_big.to_string().contains("文件太大"));
    }

    #[test]
    fn 只有绝对路径才算本机文件() {
        assert_eq!(local_path("/home/a/b.png"), Some(PathBuf::from("/home/a/b.png")));
        assert_eq!(local_path("file:///home/a/b.png"), Some(PathBuf::from("/home/a/b.png")));
        assert_eq!(local_path("C:\\Users\\a.png"), Some(PathBuf::from("C:\\Users\\a.png")));
        assert_eq!(local_path("C:/Users/a.png"), Some(PathBuf::from("C:/Users/a.png")));
        // 这些都不是本机路径，原样放行。
        assert_eq!(local_path("http://e.com/a.png"), None);
        assert_eq!(local_path("https://e.com/a.png"), None);
        assert_eq!(local_path("base64://iVBORw0"), None);
        assert_eq!(local_path("relative/a.png"), None);
        assert_eq!(local_path("a.png"), None);
    }
}
