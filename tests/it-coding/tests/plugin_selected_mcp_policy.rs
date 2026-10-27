//! Selected-plugin policy composition preserves declaration restrictions.
use ra_coding::{
    config::{AppToolApproval, McpServerAuth, McpServerConfig, PluginMcpServerConfig},
    plugins::apply_configured_plugin_mcp_server_policies,
};
use serde_json::json;
use std::collections::HashMap;

#[test]
fn configured_selected_policy_intersects_tools_approval_and_output_limits() {
    let declared: McpServerConfig = serde_json::from_value(json!({
        "command": "demo",
        "enabled_tools": ["read", "write"],
        "disabled_tools": ["unsafe"],
        "default_tools_approval_mode": "writes",
        "tools": { "read": { "approval_mode": "auto", "output_token_limit": 12 } }
    }))
    .unwrap();
    let policy: PluginMcpServerConfig = serde_json::from_value(json!({
        "enabled": true,
        "enabled_tools": ["read", "extra"],
        "disabled_tools": ["write", "unsafe"],
        "default_tools_approval_mode": "auto",
        "tools": {
            "read": { "output_token_limit": 20 },
            "extra": { "approval_mode": "approve", "output_token_limit": 5 }
        }
    }))
    .unwrap();
    let mut servers = HashMap::from([("demo".into(), declared)]);
    apply_configured_plugin_mcp_server_policies(
        &HashMap::from([("demo".into(), policy)]),
        &mut servers,
    );
    let result = &servers["demo"];
    assert_eq!(
        result.enabled_tools.as_deref(),
        Some(&["read".to_string()][..])
    );
    assert_eq!(
        result.disabled_tools.as_deref(),
        Some(&["unsafe".to_string(), "write".to_string()][..])
    );
    assert_eq!(
        result.default_tools_approval_mode,
        Some(AppToolApproval::Prompt)
    );
    assert_eq!(result.tools["read"].output_token_limit.unwrap().get(), 12);
    assert_eq!(
        result.tools["extra"].approval_mode,
        Some(AppToolApproval::Writes)
    );
}

#[test]
fn selected_policy_cannot_reenable_declarations_or_enterprise_auth_markers() {
    for (declared_enabled, policy_json) in [
        (false, json!({"enabled":true})),
        (true, json!({"enabled":true,"ema_auth":false})),
    ] {
        let mut servers = HashMap::from([(
            "demo".into(),
            serde_json::from_value::<McpServerConfig>(
                json!({"command":"demo","enabled":declared_enabled}),
            )
            .unwrap(),
        )]);
        let policy: PluginMcpServerConfig = serde_json::from_value(policy_json).unwrap();
        apply_configured_plugin_mcp_server_policies(
            &HashMap::from([("demo".into(), policy)]),
            &mut servers,
        );
        assert!(!servers["demo"].enabled);
        if declared_enabled {
            assert!(matches!(servers["demo"].auth, McpServerAuth::EmaAuth));
        }
    }
}
