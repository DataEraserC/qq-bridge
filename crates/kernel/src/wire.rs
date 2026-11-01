//! 适配器 wire 类型 ↔ kernel 类型（ADR-0027）。
//!
//! wire 协议定义在 `acb-adapter-api`，core 只在这里做一次转换：
//! - 外部适配器构造 `WireMethodSpec` / `WireRecord` / `WireOutgoingRecord`；
//! - core 把它们变成 `MethodSpec` / `RecordDraft` / `OutgoingRecord`，
//!   校验和记账规则不变。
//!
//! 转换失败一律带具体原因——这些错误发生在启动期（目录）或回执解析期，
//! 不能静默降级。

use std::borrow::Cow;
use std::sync::Arc;

use acb_adapter_api::{
    WireConnection, WireError, WireManifest, WireMethodSpec, WireOutgoingKind,
    WireOutgoingRecord, WireParamSpec, WirePluginMetadata, WireRecord, WireTargetSpec,
};

use crate::adapter::{AdapterCapability, AdapterManifest};
use crate::config::Connection;
use crate::plugin_meta::{PluginEffect, PluginMetadata};
use crate::outbound::{
    CallError, Effect, MethodSpec, OutboundError, OutgoingKind, OutgoingRecord, ParamSpec,
    ParamType, RecordMode, Risk, Scope, TargetRole, TargetSpec,
};
use crate::storage::RecordDraft;

impl TryFrom<WireManifest> for AdapterManifest {
    type Error = String;

    fn try_from(manifest: WireManifest) -> Result<Self, Self::Error> {
        let mut capabilities = Vec::with_capacity(manifest.capabilities.len());
        for capability in &manifest.capabilities {
            capabilities.push(match capability.as_str() {
                "receive_events" => AdapterCapability::ReceiveEvents,
                "send" => AdapterCapability::Send,
                "catalog" => AdapterCapability::Catalog,
                other => return Err(format!("不认识的适配器能力：{other}")),
            });
        }
        let transports = manifest
            .transports
            .into_iter()
            .map(Cow::Owned)
            .collect::<Vec<_>>();
        let metadata =
            PluginMetadata::try_from(manifest.metadata).map_err(|detail| format!("metadata：{detail}"))?;
        Ok(AdapterManifest {
            id: Cow::Owned(manifest.id),
            platform: Cow::Owned(manifest.platform),
            kind: Cow::Owned(manifest.kind),
            transports: transports.into(),
            api_version: manifest.api_version,
            capabilities: capabilities.into(),
            metadata,
        })
    }
}

impl TryFrom<WirePluginMetadata> for PluginMetadata {
    type Error = String;

    fn try_from(metadata: WirePluginMetadata) -> Result<Self, Self::Error> {
        let mut effects = Vec::with_capacity(metadata.effects.len());
        for effect in &metadata.effects {
            effects.push(
                PluginEffect::parse(effect)
                    .ok_or_else(|| format!("不认识的插件效果：{effect}"))?,
            );
        }
        Ok(Self {
            title: metadata.title.map(Cow::Owned),
            summary: metadata.summary.map(Cow::Owned),
            description: metadata.description.map(Cow::Owned),
            tags: metadata
                .tags
                .into_iter()
                .map(Cow::Owned)
                .collect::<Vec<_>>()
                .into(),
            effects: effects.into(),
            docs_url: metadata.docs_url.map(Cow::Owned),
            options_schema: metadata.options_schema,
        })
    }
}

impl From<&Connection> for WireConnection {
    fn from(connection: &Connection) -> Self {
        Self {
            id: connection.id.clone(),
            platform: connection.platform.clone(),
            kind: connection.kind.clone(),
            transport: connection.transport.as_str().to_owned(),
            url: connection.url.clone(),
            token: connection.token.clone(),
            account: connection.account.clone(),
            admin_qqs: connection.admin_qqs.clone(),
            options: connection.options.clone(),
        }
    }
}

impl From<WireRecord> for RecordDraft {
    fn from(record: WireRecord) -> Self {
        Self {
            room: record.room,
            kind: record.kind,
            sender: record.sender,
            at: record.at,
            elements: record.elements,
            at_self: record.at_self,
            platform_id: record.platform_id,
            reply_to_self: record.reply_to_self,
        }
    }
}

impl TryFrom<WireMethodSpec> for MethodSpec {
    type Error = String;

    fn try_from(spec: WireMethodSpec) -> Result<Self, Self::Error> {
        let name = spec.name.trim();
        if name.is_empty() {
            return Err("方法名不能为空".into());
        }
        if !name.contains('.') {
            return Err(format!("方法名要写成 resource.method：{name}"));
        }

        let risk = parse_risk(&spec.risk)?;
        let scope = parse_scope(&spec.scope)?;
        let effect = parse_effect(&spec.effect)?;
        let record = parse_record(&spec.record)?;

        let params = spec
            .params
            .into_iter()
            .map(WireParamSpec::try_into)
            .collect::<Result<Vec<ParamSpec>, String>>()?;
        let targets = spec
            .targets
            .into_iter()
            .map(WireTargetSpec::try_into)
            .collect::<Result<Vec<TargetSpec>, String>>()?;
        let tags = spec
            .tags
            .into_iter()
            .map(|tag| Cow::Owned(tag))
            .collect::<Vec<_>>();

        Ok(MethodSpec {
            name: Cow::Owned(name.to_owned()),
            summary: Cow::Owned(spec.summary),
            tags: tags.into(),
            risk,
            scope,
            params: params.into(),
            targets: targets.into(),
            effect,
            record,
        })
    }
}

impl TryFrom<WireParamSpec> for ParamSpec {
    type Error = String;

    fn try_from(param: WireParamSpec) -> Result<Self, Self::Error> {
        let name = param.name.trim();
        if name.is_empty() {
            return Err("参数名不能为空".into());
        }
        let ty = parse_param_type(&param.ty)?;
        let values = param
            .values
            .into_iter()
            .map(Cow::Owned)
            .collect::<Vec<_>>();
        Ok(ParamSpec {
            name: Cow::Owned(name.to_owned()),
            ty,
            required: param.required,
            desc: Cow::Owned(param.desc),
            values: values.into(),
        })
    }
}

impl TryFrom<WireTargetSpec> for TargetSpec {
    type Error = String;

    fn try_from(target: WireTargetSpec) -> Result<Self, Self::Error> {
        let param = target.param.trim();
        let room_kind = target.room_kind.trim();
        if param.is_empty() || room_kind.is_empty() {
            return Err("目标参数的 param / room_kind 不能为空".into());
        }
        let role = match target.role.as_str() {
            "room" => TargetRole::Room,
            "member" => TargetRole::Member,
            other => return Err(format!("不认识的目标角色：{other}（room / member）")),
        };
        Ok(TargetSpec {
            param: Cow::Owned(param.to_owned()),
            room_kind: Cow::Owned(room_kind.to_owned()),
            role,
        })
    }
}

impl TryFrom<WireOutgoingRecord> for OutgoingRecord {
    type Error = String;

    fn try_from(record: WireOutgoingRecord) -> Result<Self, Self::Error> {
        let kind = match record.kind {
            WireOutgoingKind::Message => OutgoingKind::Message,
            WireOutgoingKind::Event { event } => OutgoingKind::Event(event),
        };
        Ok(OutgoingRecord {
            kind,
            elements: record.elements,
            platform_id: record.platform_id,
            at: record.at,
        })
    }
}

/// 把 wire 目录整体转成 kernel 目录；方法名重复直接拒绝。
pub fn convert_catalog(specs: Vec<WireMethodSpec>) -> Result<Arc<[MethodSpec]>, String> {
    let mut methods = Vec::with_capacity(specs.len());
    for spec in specs {
        let name = spec.name.trim().to_owned();
        if methods
            .iter()
            .any(|existing: &MethodSpec| existing.name.as_ref() == name.as_str())
        {
            return Err(format!("方法名重复：{name}"));
        }
        methods.push(MethodSpec::try_from(spec)?);
    }
    Ok(methods.into())
}

/// 回执里的错误 → 平台调用错误。
pub fn call_error(error: WireError) -> CallError {
    match error {
        WireError::UnknownMethod { method, suggestion } => {
            CallError::UnknownMethod { method, suggestion }
        }
        WireError::UnsupportedPlatform { detail } | WireError::Unsupported { detail } => {
            CallError::UnsupportedPlatform(detail)
        }
        WireError::NotImplemented { detail } => CallError::NotImplemented(detail),
        WireError::Forbidden { detail } | WireError::Rejected { detail } => {
            CallError::Forbidden(detail)
        }
        WireError::BadParams { detail } => CallError::BadParams(detail),
        WireError::Unavailable { detail } => CallError::Unavailable(detail),
    }
}

/// 回执里的错误 → 发送错误。
pub fn send_error(error: WireError) -> OutboundError {
    match error {
        WireError::Rejected { detail } | WireError::Forbidden { detail } => {
            OutboundError::Rejected(detail)
        }
        WireError::Unsupported { detail }
        | WireError::UnsupportedPlatform { detail }
        | WireError::NotImplemented { detail } => OutboundError::Unsupported(detail),
        WireError::Unavailable { detail } => OutboundError::Unavailable(detail),
        WireError::UnknownMethod { method, .. } => {
            OutboundError::Unsupported(format!("没有发送方法：{method}"))
        }
        WireError::BadParams { detail } => OutboundError::Rejected(detail),
    }
}

fn parse_param_type(text: &str) -> Result<ParamType, String> {
    match text {
        "string" => Ok(ParamType::String),
        "integer" => Ok(ParamType::Integer),
        "bool" | "boolean" => Ok(ParamType::Bool),
        "string_list" => Ok(ParamType::StringList),
        "object" => Ok(ParamType::Object),
        "any" => Ok(ParamType::Any),
        other => Err(format!("不认识的参数类型：{other}")),
    }
}

fn parse_risk(text: &str) -> Result<Risk, String> {
    match text {
        "read" => Ok(Risk::Read),
        "write" => Ok(Risk::Write),
        "destructive" => Ok(Risk::Destructive),
        other => Err(format!("不认识的风险级别：{other}")),
    }
}

fn parse_scope(text: &str) -> Result<Scope, String> {
    match text {
        "room" => Ok(Scope::Room),
        "connection" => Ok(Scope::Connection),
        other => Err(format!("不认识的作用域：{other}")),
    }
}

fn parse_effect(text: &str) -> Result<Effect, String> {
    match text {
        "read" => Ok(Effect::Read),
        "write" => Ok(Effect::Write),
        "send" => Ok(Effect::Send),
        "delete" => Ok(Effect::Delete),
        other => Err(format!("不认识的 effect：{other}")),
    }
}

fn parse_record(text: &str) -> Result<RecordMode, String> {
    match text {
        "none" => Ok(RecordMode::None),
        "message" => Ok(RecordMode::Message),
        "event" => Ok(RecordMode::Event),
        other => Err(format!("不认识的 record 模式：{other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use acb_proto::{Element, RecordKind, RoomId, Sender};

    fn wire_method() -> WireMethodSpec {
        WireMethodSpec {
            name: "demo.echo".into(),
            summary: "回显".into(),
            tags: vec!["send".into()],
            risk: "write".into(),
            scope: "room".into(),
            params: vec![WireParamSpec {
                name: "text".into(),
                ty: "string".into(),
                required: true,
                desc: "正文".into(),
                values: vec!["a".into(), "b".into()],
            }],
            targets: vec![WireTargetSpec {
                param: "group_id".into(),
                room_kind: "group".into(),
                role: "room".into(),
            }],
            effect: "send".into(),
            record: "message".into(),
        }
    }

    #[test]
    fn method_spec_is_converted_with_all_fields() {
        let method = MethodSpec::try_from(wire_method()).expect("能转换");
        assert_eq!(method.name, "demo.echo");
        assert_eq!(method.risk, Risk::Write);
        assert_eq!(method.effect, Effect::Send);
        assert_eq!(method.record, RecordMode::Message);
        assert_eq!(method.params.len(), 1);
        assert_eq!(method.params[0].values.len(), 2);
        assert_eq!(method.targets[0].role, TargetRole::Room);
        assert!(method.has_tag("send"));
    }

    #[test]
    fn bad_names_and_enums_are_rejected() {
        let mut method = wire_method();
        method.name = "echo".into();
        assert!(MethodSpec::try_from(method).is_err());

        let mut method = wire_method();
        method.risk = "danger".into();
        assert!(MethodSpec::try_from(method).is_err());
    }

    #[test]
    fn duplicate_method_names_are_rejected() {
        let specs = vec![wire_method(), wire_method()];
        let error = convert_catalog(specs).expect_err("重复要报错");
        assert!(error.contains("重复"), "{error}");
    }

    #[test]
    fn record_is_converted() {
        let record = WireRecord {
            room: RoomId::new("demo", "group/1").expect("合法"),
            kind: RecordKind::Event {
                event: "poke".into(),
                tags: vec!["poke".into()],
                payload: None,
                summary: None,
            },
            sender: Sender {
                id: "10001".into(),
                name: "老王".into(),
                role: None,
            },
            at: 5,
            elements: vec![Element::text("拍了拍")],
            at_self: true,
            platform_id: None,
            reply_to_self: false,
        };
        let draft = RecordDraft::from(record);
        assert!(draft.at_self);
        assert!(matches!(draft.kind, RecordKind::Event { .. }));
    }

    #[test]
    fn wire_errors_map_to_the_right_taxonomy() {
        assert!(matches!(
            call_error(WireError::UnknownMethod {
                method: "x".into(),
                suggestion: None
            }),
            CallError::UnknownMethod { .. }
        ));
        assert!(matches!(
            call_error(WireError::BadParams { detail: "x".into() }),
            CallError::BadParams(_)
        ));
        assert!(matches!(
            send_error(WireError::Rejected { detail: "x".into() }),
            OutboundError::Rejected(_)
        ));
        assert!(matches!(
            send_error(WireError::Unavailable { detail: "x".into() }),
            OutboundError::Unavailable(_)
        ));
    }

    #[test]
    fn manifest_capabilities_are_converted() {
        let manifest = AdapterManifest::try_from(WireManifest {
            id: "acb.adapter.demo".into(),
            platform: "demo".into(),
            kind: "demo".into(),
            transports: vec!["process".into()],
            api_version: 1,
            capabilities: vec!["receive_events".into(), "send".into()],
            extension: "platform_adapter".into(),
            metadata: acb_adapter_api::WirePluginMetadata::default(),
        })
        .expect("能转换");
        assert_eq!(manifest.kind, "demo");
        assert!(manifest
            .capabilities
            .iter()
            .any(|capability| *capability == AdapterCapability::Send));
    }
}
