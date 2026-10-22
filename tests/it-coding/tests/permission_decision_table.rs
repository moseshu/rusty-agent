//! Installed coding policies compared with the frozen legacy decision table.

#[path = "../../support/configured_coding_host.rs"]
mod configured_host;
#[path = "permission_decision_table/inventory.rs"]
mod inventory;

use ra_coding::{
    CodingHost, CodingPermissionMode, CodingProfile, host_backed_surface,
    thread_settings::CodingThreadSettingsAdapter,
};
use ra_core::{item::AgentId, permission::PermissionDecision, prompt::PromptRole, tool::Tool};
use ra_protocol::server::{
    ThreadSettingsAdapter,
    protocol::{AskForApproval, SandboxPolicy, ThreadPolicyOverrides},
};
use ra_runtime::runner::RunConfig;
use serde_json::{Value, json};
use std::sync::Arc;
use tempfile::TempDir;

const BASELINE: &str = include_str!("../fixtures/permission_decision_table.json");

fn adapter(workspace: &TempDir) -> CodingThreadSettingsAdapter {
    CodingThreadSettingsAdapter::new(
        workspace.path(),
        SandboxPolicy::WorkspaceWrite {
            writable_roots: Vec::new(),
            network_access: false,
            exclude_tmpdir_env_var: false,
            exclude_slash_tmp: false,
        },
    )
    .unwrap()
}

#[tokio::test]
async fn built_in_permission_decision_table_preserves_the_legacy_contract() {
    let workspace = TempDir::new().unwrap();
    let services = Arc::new(configured_host::Services::default());
    let host = services.install(CodingHost::open(workspace.path()).unwrap());
    let surface = host_backed_surface(&PromptRole::Main, &host, CodingProfile::Full)
        .await
        .unwrap();
    let mut tools: Vec<(String, Arc<dyn Tool>)> = surface
        .tool_surface()
        .tools()
        .into_iter()
        .map(|tool| {
            (
                format!("coding/{}", tool.origin().qualified_name()),
                tool.clone(),
            )
        })
        .collect();
    tools.extend(inventory::additional_tools(&workspace).await);
    tools.sort_by(|a, b| a.0.cmp(&b.0));
    let actual = table(&workspace, &tools);
    let baseline: Value = serde_json::from_str(BASELINE).unwrap();
    assert_eq!(actual, baseline["rows"]);
    assert_eq!(baseline["exceptions"].as_array().unwrap().len(), 3);
    assert!(
        tools.len() >= 32,
        "all built-in adapters must be represented"
    );
    assert!(
        actual
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["action_classes"] == json!([]))
    );
    assert!(
        actual
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["action_classes"] == json!(["edit", "read"]))
    );
}

fn table(workspace: &TempDir, tools: &[(String, Arc<dyn Tool>)]) -> Value {
    let adapter = adapter(workspace);
    let mut rows = Vec::new();
    for mode in CodingPermissionMode::ALL {
        for never in [false, true] {
            let config = adapter.initialize(RunConfig::new().with_permission_policy(mode.policy()));
            let config = adapter
                .prepare(
                    &config,
                    &ThreadPolicyOverrides::new().with_approval_policy(if never {
                        AskForApproval::Never
                    } else {
                        AskForApproval::OnRequest
                    }),
                )
                .unwrap();
            for (name, tool) in tools {
                let options = tool.options();
                for fallback in [
                    PermissionDecision::Allow,
                    PermissionDecision::Ask,
                    PermissionDecision::Deny,
                ] {
                    rows.push(json!({
                        "mode": mode, "approval_policy": if never { "never" } else { "on-request" },
                        "tool": name, "action_classes": options.action_classes(),
                        "tool_approval_policy": options.approval(), "approval_result": fallback,
                        "decision": config.permission().evaluate(options.action_classes(), tool.origin(), &AgentId::new("baseline"), fallback),
                    }));
                }
            }
        }
    }
    json!(rows)
}

#[test]
fn installing_and_updating_settings_preserves_base_policies() {
    let workspace = TempDir::new().unwrap();
    let adapter = adapter(&workspace);
    assert!(
        adapter
            .initialize(RunConfig::new())
            .permission()
            .policy()
            .is_none()
    );
    for mode in CodingPermissionMode::ALL {
        let base = mode.policy();
        let config = adapter.initialize(RunConfig::new().with_permission_policy(base.clone()));
        assert_eq!(config.permission().policy(), Some(&base));
        let never = adapter
            .prepare(
                &config,
                &ThreadPolicyOverrides::new().with_approval_policy(AskForApproval::Never),
            )
            .unwrap();
        assert_eq!(
            never.permission().policy(),
            Some(&base.clone().with_prompts(false))
        );
        let restored = adapter
            .prepare(
                &never,
                &ThreadPolicyOverrides::new().with_approval_policy(AskForApproval::OnRequest),
            )
            .unwrap();
        assert_eq!(restored.permission().policy(), Some(&base));
    }
}

#[test]
fn coding_modes_keep_labels_display_and_serde_spellings() {
    assert_eq!(
        CodingPermissionMode::default(),
        CodingPermissionMode::Default
    );
    for (mode, label) in CodingPermissionMode::ALL.into_iter().zip([
        "default",
        "acceptEdits",
        "bypassPermissions",
        "plan",
        "dontAsk",
    ]) {
        assert_eq!(mode.label(), label);
        assert_eq!(mode.to_string(), label);
        assert_eq!(serde_json::to_value(mode).unwrap(), json!(label));
        assert_eq!(
            serde_json::from_value::<CodingPermissionMode>(json!(label)).unwrap(),
            mode
        );
    }
    for invalid in ["accept_edits", "bypass_permissions", "dont_ask", "unknown"] {
        assert!(serde_json::from_value::<CodingPermissionMode>(json!(invalid)).is_err());
    }
}

#[test]
fn coding_host_explicitly_selects_product_run_limits() {
    let workspace = TempDir::new().unwrap();
    let config = CodingHost::open(workspace.path())
        .unwrap()
        .build_run_config();
    assert_eq!(config.max_turns(), None);
    assert_eq!(config.max_function_tool_concurrency(), Some(8));
    assert_eq!(RunConfig::new().max_function_tool_concurrency(), None);
}
