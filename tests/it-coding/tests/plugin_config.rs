// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Explicit configuration fixtures; these do not claim configuration-stack or loader coverage.

use std::num::NonZeroUsize;

use ra_coding::config::{
    AppToolApproval, MarketplaceConfig, MarketplaceSourceType, McpServerToolConfig, PluginConfig,
    PluginMcpServerConfig, PluginsConfigToml, PluginsDefaultConfig,
};
use serde_json::json;

#[test]
fn plugin_activation_inherits_defaults_without_explicit_enablement() {
    for default_enabled in [None, Some(false), Some(true)] {
        let default_config = default_enabled.map_or_else(String::new, |enabled| {
            format!("[_default]\nenabled = {enabled}\n")
        });
        let config: PluginsConfigToml = toml::from_str(&format!(
            r#"{default_config}
["empty@market"]
["mcp@market".mcp_servers.example]
enabled = false
["approval@market".mcp_servers.example]
default_tools_approval_mode = "approve"
["enabled@market"]
enabled = true
["disabled@market"]
enabled = false
"#
        ))
        .expect("plugin activation config");
        assert_eq!(
            [
                "empty@market",
                "mcp@market",
                "approval@market",
                "enabled@market",
                "disabled@market"
            ]
            .map(|plugin_id| config.plugins[plugin_id].enabled),
            [None, None, None, Some(true), Some(false)],
        );
        assert_eq!(
            [
                "missing@market",
                "empty@market",
                "mcp@market",
                "approval@market",
                "enabled@market",
                "disabled@market"
            ]
            .map(|plugin_id| config.allows_plugin(plugin_id)),
            match default_enabled {
                None => [true, true, true, true, true, true],
                Some(false) => [false, false, false, false, true, false],
                Some(true) => [true, true, true, true, true, false],
            },
        );
        let round_trip: PluginsConfigToml =
            toml::from_str(&toml::to_string(&config).expect("serialize plugin config"))
                .expect("deserialize plugin config");
        assert_eq!(round_trip, config);
    }
}

#[test]
fn app_tool_approval_restrictions_never_weaken_either_policy() {
    use AppToolApproval::{Approve, Auto, Prompt, Writes};

    let modes = [Approve, Auto, Writes, Prompt];
    let expected = [
        [Approve, Auto, Writes, Prompt],
        [Auto, Auto, Prompt, Prompt],
        [Writes, Prompt, Writes, Prompt],
        [Prompt, Prompt, Prompt, Prompt],
    ];
    for (parent_index, parent) in modes.into_iter().enumerate() {
        for (requested_index, requested) in modes.into_iter().enumerate() {
            assert_eq!(
                parent.restrict_to(requested),
                expected[parent_index][requested_index],
                "parent: {parent:?}, requested: {requested:?}",
            );
        }
    }
}

#[test]
fn omitted_defaults_and_empty_default_table_remain_distinct() {
    let absent: PluginsConfigToml = toml::from_str("").unwrap();
    assert_eq!(absent, PluginsConfigToml::default());
    assert_eq!(absent.default, None);
    assert_eq!(serde_json::to_value(&absent).unwrap(), json!({}));

    let empty: PluginsConfigToml = toml::from_str("[_default]").unwrap();
    assert_eq!(empty.default, Some(PluginsDefaultConfig { enabled: true }));
    assert_eq!(
        serde_json::to_value(&empty).unwrap(),
        json!({"_default": {"enabled": true}})
    );
}

#[test]
fn configuration_values_do_not_validate_or_normalize_plugin_identities() {
    let config: PluginsConfigToml = toml::from_str(
        "[\"unvalidated configured name\"]\nenabled = false\n[\"Case.Name@market\"]",
    )
    .unwrap();
    assert_eq!(config.plugins.len(), 2);
    assert_eq!(
        config.plugins["unvalidated configured name"].enabled,
        Some(false)
    );
    assert_eq!(config.plugins["Case.Name@market"].enabled, None);
}

#[test]
fn server_policy_preserves_explicit_empty_tool_lists_and_nested_settings() {
    let source = r#"
["sample@market".mcp_servers.example]
enabled_tools = []
disabled_tools = ["remove"]
default_tools_approval_mode = "writes"
["sample@market".mcp_servers.example.tools.read]
approval_mode = "approve"
output_token_limit = 17
"#;
    let config: PluginsConfigToml = toml::from_str(source).unwrap();
    let plugin = &config.plugins["sample@market"];
    assert_eq!(plugin.enabled, None);
    let policy = &plugin.mcp_servers["example"];
    assert!(policy.enabled);
    assert!(!policy.has_unsupported_ema_auth);
    assert_eq!(policy.enabled_tools, Some(vec![]));
    assert_eq!(policy.disabled_tools, Some(vec!["remove".into()]));
    assert_eq!(
        policy.default_tools_approval_mode,
        Some(AppToolApproval::Writes)
    );
    assert_eq!(
        policy.tools["read"],
        McpServerToolConfig {
            approval_mode: Some(AppToolApproval::Approve),
            output_token_limit: NonZeroUsize::new(17),
        }
    );
    let serialized = toml::to_string(&config).unwrap();
    assert_eq!(
        toml::from_str::<PluginsConfigToml>(&serialized).unwrap(),
        config
    );
}

#[test]
fn default_server_policy_omits_optional_fields_without_erasing_enablement() {
    let policy: PluginMcpServerConfig = toml::from_str("").unwrap();
    assert_eq!(policy, PluginMcpServerConfig::default());
    assert_eq!(
        serde_json::to_value(policy).unwrap(),
        json!({"enabled": true})
    );
    let plugin: PluginConfig = toml::from_str("").unwrap();
    assert_eq!(serde_json::to_value(plugin).unwrap(), json!({}));
}

#[test]
fn retired_ema_overlay_presence_survives_toml_and_json_round_trips() {
    for endpoint in ["https://resource.example/mcp", "https://other.example/mcp"] {
        let policy: PluginMcpServerConfig =
            toml::from_str(&format!("[ema_auth]\nurl = '{endpoint}'")).unwrap();
        assert!(policy.has_unsupported_ema_auth);
        assert!(
            policy.enabled,
            "the loader, not deserialization, disables the server"
        );
        let serialized = toml::to_string(&policy).unwrap();
        assert_eq!(
            toml::from_str::<PluginMcpServerConfig>(&serialized).unwrap(),
            policy
        );
        let serialized = serde_json::to_value(&policy).unwrap();
        assert_eq!(serialized, json!({"enabled": true, "ema_auth": true}));
        assert_eq!(
            serde_json::from_value::<PluginMcpServerConfig>(serialized).unwrap(),
            policy
        );
    }
}

#[test]
fn retired_ema_overlay_is_detected_even_when_null_or_false() {
    for value in [
        json!(null),
        json!(false),
        json!({}),
        json!([]),
        json!("retired"),
    ] {
        let policy: PluginMcpServerConfig =
            serde_json::from_value(json!({"ema_auth": value})).unwrap();
        assert!(policy.has_unsupported_ema_auth);
    }
}

#[test]
fn output_budget_restriction_preserves_approval_and_chooses_the_stricter_limit() {
    for (initial, requested, expected) in [
        (None, None, None),
        (Some(20), None, Some(20)),
        (None, Some(10), Some(10)),
        (Some(20), Some(10), Some(10)),
        (Some(10), Some(20), Some(10)),
        (Some(10), Some(10), Some(10)),
    ] {
        let mut tool = McpServerToolConfig {
            approval_mode: Some(AppToolApproval::Writes),
            output_token_limit: initial.and_then(NonZeroUsize::new),
        };
        tool.restrict_output_token_limit(requested.and_then(NonZeroUsize::new));
        assert_eq!(
            tool.output_token_limit,
            expected.and_then(NonZeroUsize::new)
        );
        assert_eq!(tool.approval_mode, Some(AppToolApproval::Writes));
    }
}

#[test]
fn invalid_known_policy_fields_are_not_silently_replaced_with_defaults() {
    for source in [
        "enabled = 'true'",
        "enabled_tools = false",
        "disabled_tools = [1]",
        "default_tools_approval_mode = 'always'",
        "[tools.read]\noutput_token_limit = 0",
        "[tools.read]\noutput_token_limit = -1",
        "[tools.read]\noutput_token_limit = 1.5",
        "[tools.read]\napproval_mode = 'Approve'",
    ] {
        assert!(
            toml::from_str::<PluginMcpServerConfig>(source).is_err(),
            "{source}"
        );
    }
    assert!(toml::from_str::<PluginsConfigToml>("[_default]\nenabled = 'false'").is_err());
}

#[test]
fn deserialization_preserves_upstream_unknown_field_tolerance() {
    let plugin: PluginConfig = toml::from_str("future_field = 'ignored'").unwrap();
    assert_eq!(plugin.enabled, None);
    assert!(plugin.mcp_servers.is_empty());
    let policy: PluginMcpServerConfig = toml::from_str("future_field = 'ignored'").unwrap();
    assert_eq!(policy, PluginMcpServerConfig::default());
    let defaults: PluginsDefaultConfig = toml::from_str("future_field = 'ignored'").unwrap();
    assert!(defaults.enabled);
    let tool: McpServerToolConfig = toml::from_str("future_field = 'ignored'").unwrap();
    assert_eq!(tool, McpServerToolConfig::default());
}

#[test]
fn marketplace_configuration_retains_origin_and_activation_metadata() {
    let source = r#"
source_type = "git"
source = "https://example.com/plugins.git"
ref = "stable"
sparse_paths = [".agents/plugins", "plugins/example"]
last_updated = "2026-10-25T00:00:00Z"
last_revision = "abc123"
"#;
    let config: MarketplaceConfig = toml::from_str(source).unwrap();
    assert_eq!(config.source_type, Some(MarketplaceSourceType::Git));
    assert_eq!(
        config.source.as_deref(),
        Some("https://example.com/plugins.git")
    );
    assert_eq!(config.ref_name.as_deref(), Some("stable"));
    assert_eq!(config.last_updated.as_deref(), Some("2026-10-25T00:00:00Z"));
    assert_eq!(config.last_revision.as_deref(), Some("abc123"));
    assert_eq!(
        config.sparse_paths,
        Some(vec![".agents/plugins".into(), "plugins/example".into()])
    );
    let serialized = toml::to_string(&config).unwrap();
    assert_eq!(
        toml::from_str::<MarketplaceConfig>(&serialized).unwrap(),
        config
    );
    assert_eq!(
        toml::from_str::<MarketplaceConfig>("").unwrap(),
        MarketplaceConfig::default()
    );
    let local: MarketplaceConfig =
        toml::from_str("source_type = 'local'\nsource = 'unresolved'").unwrap();
    assert_eq!(local.source_type, Some(MarketplaceSourceType::Local));
    assert_eq!(local.source.as_deref(), Some("unresolved"));
    assert!(toml::from_str::<MarketplaceConfig>("source_type = 'npm'").is_err());
}
