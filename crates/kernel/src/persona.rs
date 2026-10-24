//! 人格文本的加载。
//!
//! 配置里的人格是「内联文本」或「文件引用」二选一，校验期已经确定不会有第三种。这一层
//! 把两者都变成一段字符串——投递时原样发给适配器，内核不解析、不渲染。
//!
//! 文件路径相对 **配置文件所在目录** 解析，不是相对进程工作目录。内核通常由 systemd 或
//! nix 启动，工作目录不可预期；按配置文件定位才能让 `config.toml` 与 `personas/` 保持
//! 它们看起来的关系。
//!
//! 读不出来即启动失败。人格缺失的表现是「模型不像它」，那比启动报错难查得多。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::config::{Config, PersonaSource};

/// 已加载的人格文本。
#[derive(Debug)]
pub struct Personas {
    texts: BTreeMap<String, String>,
}

impl Personas {
    /// 读取并加载配置里的全部人格。`base` 是配置文件所在目录。
    pub fn load(config: &Config, base: &Path) -> Result<Self, PersonaError> {
        let mut texts = BTreeMap::new();

        for persona in &config.personas {
            let text = match &persona.source {
                PersonaSource::Inline(text) => text.clone(),
                PersonaSource::File(relative) => {
                    let path = base.join(relative);
                    std::fs::read_to_string(&path).map_err(|source| PersonaError::Read {
                        id: persona.id.clone(),
                        path: path.display().to_string(),
                        detail: source.to_string(),
                    })?
                }
            };
            texts.insert(persona.id.clone(), text);
        }

        Ok(Self { texts })
    }

    /// 按 id 取文本。id 由配置校验保证存在，这里仍返回 `Option`——「找不到」不该被
    /// 悄悄当成空人格投递出去，那会让模型收到一份没有指令的提示词。
    pub fn text(&self, id: &str) -> Option<&str> {
        self.texts.get(id).map(String::as_str)
    }

    pub fn ids(&self) -> impl Iterator<Item = &str> {
        self.texts.keys().map(String::as_str)
    }
}

/// 加载失败的原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PersonaError {
    /// 引用的文件读不出来。
    Read {
        id: String,
        path: String,
        detail: String,
    },
}

impl std::fmt::Display for PersonaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Read { id, path, detail } => {
                write!(f, "人格 {id} 的文件读不出来：{path}（{detail}）")
            }
        }
    }
}

impl std::error::Error for PersonaError {}

/// 配置文件所在目录，用于解析相对路径。
pub fn base_dir(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(text: &str) -> Config {
        Config::parse(text).expect("配置应能解析")
    }

    const HEAD: &str = r#"
[[accounts]]
id = "main"
self_id = "10001"
ws_url = "ws://127.0.0.1:6700"

[[devices]]
id = "alice-phone"
persona = "whale"
memory = "alice"
accounts = ["main"]

  [[devices.scopes]]
  group = "20001"
  receive = ["main"]
  send = ["main"]
"#;

    #[test]
    fn inline_persona_is_returned_verbatim() {
        let text = format!("{HEAD}\n[[personas]]\nid = \"whale\"\ntext = \"你是一只鲸鱼\"\n");
        let personas = Personas::load(&config(&text), Path::new(".")).expect("内联不需要文件");
        assert_eq!(personas.text("whale"), Some("你是一只鲸鱼"));
    }

    #[test]
    fn file_persona_is_read_relative_to_config_dir() {
        let dir = std::env::temp_dir().join(format!("qqab-persona-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("personas")).expect("应能建目录");
        std::fs::write(dir.join("personas/whale.md"), "从文件来的人格").expect("应能写文件");

        let text = format!("{HEAD}\n[[personas]]\nid = \"whale\"\nfile = \"personas/whale.md\"\n");
        let personas = Personas::load(&config(&text), &dir).expect("应能读到文件");
        assert_eq!(personas.text("whale"), Some("从文件来的人格"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_persona_file_is_an_error_not_an_empty_string() {
        let text = format!("{HEAD}\n[[personas]]\nid = \"whale\"\nfile = \"nope.md\"\n");
        let error = Personas::load(&config(&text), Path::new("/nonexistent-dir")).unwrap_err();
        assert!(
            matches!(error, PersonaError::Read { ref id, .. } if id == "whale"),
            "得到 {error:?}"
        );
    }

    #[test]
    fn unknown_persona_id_is_none() {
        let text = format!("{HEAD}\n[[personas]]\nid = \"whale\"\ntext = \"x\"\n");
        let personas = Personas::load(&config(&text), Path::new(".")).expect("应能加载");
        assert_eq!(personas.text("nobody"), None);
    }

    #[test]
    fn base_dir_of_bare_file_name_is_current_dir() {
        assert_eq!(base_dir(Path::new("config.toml")), PathBuf::from("."));
        assert_eq!(
            base_dir(Path::new("/etc/qqab/config.toml")),
            PathBuf::from("/etc/qqab")
        );
    }
}
