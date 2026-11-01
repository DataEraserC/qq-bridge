//! 插件元数据：给 agent / 控制台看的“这东西是干什么的”。
//!
//! 机器码没有安全的运行时内省手段，所以描述由插件作者在 manifest 里声明；
//! core 只做搬运、合并和审计，不靠描述做任何行为判断。
//!
//! 描述不是保证：`effects` 是文档，真正强制执行的是 capability 审计。

use std::borrow::Cow;

use serde_json::{Value, json};

use crate::plugin::{Capability, ExtensionPoint, Health, ProviderId};

/// 插件声明的行为副作用。给 agent 判断“我的消息会不会被延迟/改写”用。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PluginEffect {
    /// 可能让会话醒来。
    MayWake,
    /// 可能选择不叫醒。
    MayStaySilent,
    /// 可能延迟投递。
    MayDelay,
    /// 可能改写正文。
    MayModifyText,
    /// 可能丢弃消息。
    MayDrop,
    /// 会调用桥外部的服务。
    ExternalCall,
    /// 结果不确定（同样的输入可能得到不同结果）。
    Nondeterministic,
}

impl PluginEffect {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MayWake => "may_wake",
            Self::MayStaySilent => "may_stay_silent",
            Self::MayDelay => "may_delay",
            Self::MayModifyText => "may_modify_text",
            Self::MayDrop => "may_drop",
            Self::ExternalCall => "external_call",
            Self::Nondeterministic => "nondeterministic",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "may_wake" => Self::MayWake,
            "may_stay_silent" => Self::MayStaySilent,
            "may_delay" => Self::MayDelay,
            "may_modify_text" => Self::MayModifyText,
            "may_drop" => Self::MayDrop,
            "external_call" => Self::ExternalCall,
            "nondeterministic" => Self::Nondeterministic,
            _ => return None,
        })
    }
}

/// 插件作者声明的描述。所有字段可选，内置 provider 由桥自己填。
#[derive(Debug, Clone, PartialEq)]
pub struct PluginMetadata {
    pub title: Option<Cow<'static, str>>,
    pub summary: Option<Cow<'static, str>>,
    pub description: Option<Cow<'static, str>>,
    pub tags: Cow<'static, [Cow<'static, str>]>,
    pub effects: Cow<'static, [PluginEffect]>,
    pub docs_url: Option<Cow<'static, str>>,
    /// 配置形状（JSON Schema）。只给形状，不暴露实际配置值。
    pub options_schema: Option<Value>,
}

impl PluginMetadata {
    pub const EMPTY: Self = Self {
        title: None,
        summary: None,
        description: None,
        tags: Cow::Borrowed(&[]),
        effects: Cow::Borrowed(&[]),
        docs_url: None,
        options_schema: None,
    };

    pub fn is_empty(&self) -> bool {
        self.title.is_none()
            && self.summary.is_none()
            && self.description.is_none()
            && self.tags.is_empty()
            && self.effects.is_empty()
            && self.docs_url.is_none()
            && self.options_schema.is_none()
    }

    /// 用 manager 的说明补全 descriptor 里缺的字段。descriptor 自己写的优先。
    pub fn merge_missing(&mut self, source: &Self) {
        if self.title.is_none() {
            self.title = source.title.clone();
        }
        if self.summary.is_none() {
            self.summary = source.summary.clone();
        }
        if self.description.is_none() {
            self.description = source.description.clone();
        }
        if self.tags.is_empty() {
            self.tags = source.tags.clone();
        }
        if self.effects.is_empty() {
            self.effects = source.effects.clone();
        }
        if self.docs_url.is_none() {
            self.docs_url = source.docs_url.clone();
        }
        if self.options_schema.is_none() {
            self.options_schema = source.options_schema.clone();
        }
    }

    pub fn to_json(&self) -> Value {
        json!({
            "title": self.title.as_deref(),
            "summary": self.summary.as_deref(),
            "description": self.description.as_deref(),
            "tags": self.tags.iter().map(|tag| tag.as_ref()).collect::<Vec<_>>(),
            "effects": self.effects.iter().map(|effect| effect.as_str()).collect::<Vec<_>>(),
            "docs_url": self.docs_url.as_deref(),
            "options_schema": self.options_schema,
        })
    }
}

impl Default for PluginMetadata {
    fn default() -> Self {
        Self::EMPTY
    }
}

/// 插件从哪来。由 Root 标注，插件不能自报。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PluginSource {
    /// 编译进桥的内置实现。
    Builtin,
    /// trusted.* manager。
    Trusted,
    /// lua.* manager。
    Lua,
    /// 动态库插件（适配器或策略）。
    Dynamic,
}

impl PluginSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Builtin => "builtin",
            Self::Trusted => "trusted",
            Self::Lua => "lua",
            Self::Dynamic => "dynamic",
        }
    }
}

/// Root 目录里的一条。给控制台和 agent 的只读视图。
#[derive(Debug, Clone, PartialEq)]
pub struct PluginInfo {
    pub id: ProviderId,
    pub extension: ExtensionPoint,
    pub metadata: PluginMetadata,
    pub capabilities: Vec<Capability>,
    pub kind: Option<String>,
    pub source: PluginSource,
    pub health: Health,
}

impl PluginInfo {
    pub fn to_json(&self) -> Value {
        json!({
            "id": self.id.to_string(),
            "extension": self.extension.as_str(),
            "capabilities": self.capabilities.iter().map(|capability| capability.as_str()).collect::<Vec<_>>(),
            "kind": self.kind,
            "source": self.source.as_str(),
            "health": health_json(&self.health),
            "metadata": self.metadata.to_json(),
        })
    }
}

fn health_json(health: &Health) -> Value {
    match health {
        Health::Healthy => json!({ "state": "healthy" }),
        Health::Degraded(detail) => json!({ "state": "degraded", "detail": detail }),
        Health::Unhealthy(detail) => json!({ "state": "unhealthy", "detail": detail }),
    }
}
