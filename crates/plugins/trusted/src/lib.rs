//! 受信任的树外 manager（ADR-0026 Phase 3）。
//!
//! 它不在 core 的保留 namespace `acb` 里，也不在 kernel 内部：只用 kernel 公开的
//! `ManagerPlugin` / `PluginLoader` 契约注册一棵自己的子树，证明树外扩展真能装进来。
//! 现在的载体是 native（编译进 bridge），后续 LuaManager / LuaLoader 走同一套契约。
//!
//! 目前提供一个 wake provider：`trusted.accumulate_only` —— 只在未读攒够时唤醒；
//! `@`、点名字、拍一拍、引用都不单独叫醒。适合「别被每句话打断，攒一批再处理」的会话。

use std::sync::Arc;

use acb_kernel::{
    Capability, ExtensionPoint, Health, LoaderKind, LoaderManifest, ManagerManifest, ManagerPlugin,
    PluginError, PluginLoader, PluginManager, PluginMetadata, PluginSource, ProviderDescriptor,
    ProviderId, ProviderInstance,
    ProviderOptions, WakeContext, WakeDecision, WakePolicy, WakeProvider, PLUGIN_API_VERSION,
};

/// 这个 manager 的 namespace。`acb` 是内置保留名，树外不能用。
pub const NAMESPACE: &str = "trusted";

/// 只在未读攒够时唤醒的 provider id。
pub const ACCUMULATE_ONLY_PROVIDER: &str = "trusted.accumulate_only";

fn accumulate_only_id() -> ProviderId {
    ProviderId::parse(ACCUMULATE_ONLY_PROVIDER).expect("provider id 必须合法")
}

/// `trusted.accumulate_only` 的实现：不读 `@`/昵称/拍/引用，只看未读数。
#[derive(Debug, Default)]
pub struct AccumulateOnlyWake;

impl WakeProvider for AccumulateOnlyWake {
    fn decide(&self, options: &WakePolicy, context: &WakeContext<'_>) -> WakeDecision {
        match options.accumulate {
            Some(threshold) if threshold > 0 && context.unread >= threshold => WakeDecision::Wake,
            _ => WakeDecision::Stay,
        }
    }
}

fn trusted_metadata() -> PluginMetadata {
    PluginMetadata {
        title: Some("trusted 示例 manager".into()),
        summary: Some("树外 native manager：只按攒条数唤醒，演示 provider 注册".into()),
        description: None,
        tags: Vec::new().into(),
        effects: vec![
            acb_kernel::PluginEffect::MayWake,
            acb_kernel::PluginEffect::MayStaySilent,
        ]
        .into(),
        docs_url: None,
        options_schema: None,
    }
}

/// 树外 manager 本体。它只声明一个 wake provider，生命周期都用默认真实现。
#[derive(Debug, Default)]
pub struct TrustedManager;

impl ManagerPlugin for TrustedManager {
    fn manifest(&self) -> ManagerManifest {
        ManagerManifest::new(
            NAMESPACE,
            PLUGIN_API_VERSION,
            vec![Capability::WakeDecision],
        )
        .with_metadata(trusted_metadata())
    }

    fn descriptors(&self) -> Vec<ProviderDescriptor> {
        vec![ProviderDescriptor::new(
            accumulate_only_id(),
            ExtensionPoint::Wake,
            PLUGIN_API_VERSION,
            vec![Capability::WakeDecision],
        )
        .with_metadata(trusted_metadata())
        .with_source(PluginSource::Trusted)]
    }

    fn instantiate(
        &self,
        id: &ProviderId,
        _options: ProviderOptions<'_>,
    ) -> Result<ProviderInstance, PluginError> {
        if id.namespace() == NAMESPACE && id.name() == "accumulate_only" {
            Ok(ProviderInstance::Wake(Arc::new(AccumulateOnlyWake)))
        } else {
            Err(PluginError::UnknownProvider(id.to_string()))
        }
    }

    fn health(&self) -> Health {
        Health::Healthy
    }
}

/// native 载体：把 `TrustedManager` 装进 Root。
#[derive(Debug, Default)]
pub struct TrustedLoader;

impl PluginLoader for TrustedLoader {
    fn manifest(&self) -> LoaderManifest {
        LoaderManifest::new(
            LoaderKind::Native,
            PLUGIN_API_VERSION,
            vec![ExtensionPoint::Wake],
        )
    }

    fn install(&self, manager: &mut PluginManager) -> Result<(), PluginError> {
        manager.install_manager(Arc::new(TrustedManager))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use acb_kernel::{RoomId, WakeContext};

    fn context<'a>(room: &'a RoomId, unread: u32, at_self: bool) -> WakeContext<'a> {
        WakeContext {
            session: "main",
            room,
            connection: "qq",
            platform: "qq",
            sender_id: "10001",
            text: "在吗",
            at_self,
            reply_to_self: false,
            event: None,
            tags: &[],
            unread,
            now: 0,
        }
    }

    #[test]
    fn loader_installs_the_tree_out_manager() {
        let mut plugins = PluginManager::with_native_builtins();
        plugins
            .install_loader(&TrustedLoader)
            .expect("树外 loader 应当能安装");

        let id = accumulate_only_id();
        plugins
            .resolve_wake(&id, &WakePolicy::default())
            .expect("树外 provider 应当可解析");
        assert!(
            plugins
                .descriptors()
                .iter()
                .any(|descriptor| descriptor.id == id),
            "descriptor 应当进 Root 的全局注册表"
        );
        assert_eq!(
            plugins.manager_health(),
            vec![acb_kernel::ManagerHealth {
                namespace: NAMESPACE.to_owned(),
                health: Health::Healthy,
            }]
        );
    }

    #[test]
    fn accumulate_only_ignores_mentions_and_waits_for_the_threshold() {
        let room = RoomId::new("qq", "group/1").expect("合法");
        let policy = WakePolicy {
            accumulate: Some(50),
            ..WakePolicy::default()
        };

        let provider = AccumulateOnlyWake;
        assert_eq!(
            provider.decide(&policy, &context(&room, 1, true)),
            WakeDecision::Stay,
            "有人 @ 也不单独叫醒"
        );
        assert_eq!(
            provider.decide(&policy, &context(&room, 50, false)),
            WakeDecision::Wake,
            "攒够阈值才醒"
        );

        let no_threshold = WakePolicy {
            accumulate: None,
            ..policy
        };
        assert_eq!(
            provider.decide(&no_threshold, &context(&room, 200, true)),
            WakeDecision::Stay,
            "没有阈值就不醒"
        );
    }

    #[test]
    fn config_can_reference_the_tree_out_provider() {
        let text = r#"
            [[connections]]
            id = "qq"
            platform = "qq"
            kind = "onebot11"
            url = "ws://127.0.0.1:3001"
            account = "10001"

            [[personas]]
            id = "p"
            name = "P"
            text = "你是桥。"

            [[harnesses]]
            id = "dsh"
            name = "DSH"

            [[sessions]]
            id = "main"
            harness = "dsh"
            persona = "p"
            token = "t-main"

            [[sessions.subscriptions]]
            connection = "qq"
            room = "group/1"

            [sessions.subscriptions.wake]
            provider = "trusted.accumulate_only"
        "#;
        let config = acb_kernel::Config::parse(text).expect("配置合法");
        let mut plugins = PluginManager::with_native_builtins();
        plugins
            .install_loader(&TrustedLoader)
            .expect("树外 loader 应当能安装");
        plugins
            .validate_config(&config)
            .expect("配置里引用的 trusted provider 应当能解析");
    }

    #[test]
    fn unknown_ids_in_the_subtree_are_refused() {
        let manager = TrustedManager;
        let unknown = ProviderId::parse("trusted.missing").expect("合法");
        assert!(matches!(
            manager.instantiate(&unknown, ProviderOptions::Wake(&WakePolicy::default())),
            Err(PluginError::UnknownProvider(_))
        ));
    }
}
