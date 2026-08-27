//! Permission-mode contracts.

use async_trait::async_trait;
use ra_core::{
    agent::AgentSpec,
    context::RunContext,
    error::Result,
    item::{AgentId, CallId, ToolApproval},
    permission::{
        PermissionDecision, PermissionMode, PermissionRule, PermissionScope, PermissionUpdate,
        PermissionUpdateDestination, ToolApprovalAllow, ToolApprovalDecision, ToolApprovalDeny,
        ToolApprovalHandler, ToolPermissionContext,
    },
    state::RunId,
};
use serde_json::{Value, json};

fn all_modes() -> &'static [PermissionMode] {
    PermissionMode::ALL
}

#[test]
fn test_permission_mode_01() {
    assert_eq!(PermissionMode::default(), PermissionMode::Default);
}

#[test]
fn test_permission_mode_02() {
    let expected = [
        (PermissionMode::Default, "default"),
        (PermissionMode::AcceptEdits, "acceptEdits"),
        (PermissionMode::BypassPermissions, "bypassPermissions"),
        (PermissionMode::Plan, "plan"),
        (PermissionMode::DontAsk, "dontAsk"),
    ];

    assert_eq!(all_modes(), expected.map(|(mode, _)| mode));

    for (mode, label) in expected {
        assert_eq!(mode.label(), label);
        assert_eq!(
            mode.to_string(),
            label,
            "one value must not have two spellings"
        );
        assert!(
            label
                .chars()
                .all(|character| character.is_ascii_alphanumeric()),
            "permission-mode label `{label}` must be a stable ASCII identifier"
        );

        let value = serde_json::to_value(mode).expect("permission mode must serialize");
        assert_eq!(value, Value::String(label.to_owned()));
        assert_eq!(
            serde_json::from_value::<PermissionMode>(value)
                .expect("permission mode must deserialize"),
            mode
        );
    }
}

#[test]
fn test_permission_mode_03() {
    let unknown = serde_json::from_value::<PermissionMode>(json!("unrestricted"));
    assert!(unknown.is_err());

    for non_canonical_label in ["accept_edits", "bypass_permissions", "dont_ask"] {
        assert!(
            serde_json::from_value::<PermissionMode>(json!(non_canonical_label)).is_err(),
            "only the established public spelling is accepted: {non_canonical_label}"
        );
    }
}

#[test]
fn test_permission_rule_04() {
    let broad_deny = PermissionRule::new(PermissionDecision::Deny);
    let namespaced_allow = PermissionRule::new(PermissionDecision::Allow)
        .with_tool_name("read_file")
        .with_namespace("workspace");

    assert!(broad_deny.matches("exec_command", None));
    assert!(namespaced_allow.matches("read_file", Some("workspace")));
    assert!(!namespaced_allow.matches("read_file", None));
    assert!(!namespaced_allow.matches("write_file", Some("workspace")));
    assert_eq!(namespaced_allow.decision(), PermissionDecision::Allow);
    assert_eq!(namespaced_allow.tool_name(), Some("read_file"));
    assert_eq!(namespaced_allow.namespace(), Some("workspace"));

    let value = serde_json::to_value(&namespaced_allow).expect("rule must serialize");
    assert_eq!(value["decision"], "allow");
    assert_eq!(value["tool_name"], "read_file");
    assert_eq!(value["namespace"], "workspace");
    let restored: PermissionRule = serde_json::from_value(value).expect("rule must deserialize");
    assert_eq!(restored, namespaced_allow);

    assert!(
        serde_json::from_value::<PermissionRule>(json!({
            "decision": "allow",
            "tool_name": "exec_command",
            "prefix": "git status"
        }))
        .is_err()
    );
}

#[test]
fn test_permission_scope_05() {
    assert_eq!(PermissionScope::default(), PermissionScope::Execute);

    for (scope, label) in [
        (PermissionScope::Read, "read"),
        (PermissionScope::Edit, "edit"),
        (PermissionScope::Execute, "execute"),
    ] {
        assert_eq!(serde_json::to_value(scope).unwrap(), json!(label));
        assert_eq!(
            serde_json::from_value::<PermissionScope>(json!(label)).unwrap(),
            scope
        );
    }
}

#[test]
fn test_tool_approval_decision_06() {
    let update = PermissionUpdate::add_rules(
        PermissionUpdateDestination::Session,
        [PermissionRule::new(PermissionDecision::Allow)
            .with_tool_name("read_file")
            .with_namespace("workspace")],
    );
    let decision = ToolApprovalDecision::from(
        ToolApprovalAllow::new()
            .with_updated_input(json!({"path": "README.md"}))
            .with_updated_permissions([update.clone()]),
    );

    assert!(decision.is_allowed());
    assert_eq!(decision.label(), "allow");
    assert_eq!(decision.to_string(), "allow");
    assert_eq!(decision.updated_input(), Some(&json!({"path": "README.md"})));
    assert_eq!(decision.updated_permissions(), [update]);
    assert_eq!(decision.denial_message(), None);
    assert!(!decision.interrupts_run());
    assert_eq!(
        serde_json::to_value(&decision).unwrap(),
        json!({
            "behavior": "allow",
            "updated_input": {"path": "README.md"},
            "updated_permissions": [{
                "type": "addRules",
                "destination": "session",
                "rules": [{
                    "decision": "allow",
                    "tool_name": "read_file",
                    "namespace": "workspace"
                }]
            }]
        })
    );
    let restored: ToolApprovalDecision =
        serde_json::from_value(serde_json::to_value(&decision).unwrap()).unwrap();
    assert_eq!(restored, decision);

    let denied = ToolApprovalDecision::from(
        ToolApprovalDeny::new("The operation is outside the workspace").with_interrupt(true),
    );
    assert!(!denied.is_allowed());
    assert_eq!(denied.label(), "deny");
    assert_eq!(
        denied.denial_message(),
        Some("The operation is outside the workspace")
    );
    assert!(denied.interrupts_run());
    assert!(denied.updated_permissions().is_empty());
    assert_eq!(denied.updated_input(), None);
    assert_eq!(
        serde_json::to_value(&denied).unwrap(),
        json!({
            "behavior": "deny",
            "message": "The operation is outside the workspace",
            "interrupt": true
        })
    );
    let restored: ToolApprovalDecision =
        serde_json::from_value(serde_json::to_value(&denied).unwrap()).unwrap();
    assert_eq!(restored, denied);
}

/// A refusal carries no policy update, and the type system is what says so: the approval builders
/// live on `ToolApprovalAllow` and cannot be reached through `deny`. This pins the two properties
/// that a future merge of the branches would break silently.
#[test]
fn test_tool_approval_decision_09() {
    assert_eq!(
        serde_json::to_value(ToolApprovalDecision::allow()).unwrap(),
        json!({"behavior": "allow"}),
        "an approval that changes nothing must not invent wire fields"
    );
    assert_eq!(
        serde_json::from_value::<ToolApprovalDecision>(json!({"behavior": "allow"})).unwrap(),
        ToolApprovalDecision::allow()
    );
    assert_eq!(
        serde_json::to_value(ToolApprovalDecision::deny("no")).unwrap(),
        json!({"behavior": "deny", "message": "no"}),
        "the default is to continue the run, so `interrupt` stays off the wire"
    );

    // A JSON null is a real replacement input, distinct from an omitted field that preserves the
    // original invocation. The distinction prevents an approval from silently running unreviewed
    // arguments after a host requested a replacement.
    let null_input = ToolApprovalDecision::allow_with_input(Value::Null);
    assert_eq!(null_input.updated_input(), Some(&Value::Null));
    assert_ne!(null_input, ToolApprovalDecision::allow());
    let text = serde_json::to_string(&null_input).unwrap();
    assert_eq!(text, r#"{"behavior":"allow","updated_input":null}"#);
    assert_eq!(
        serde_json::from_str::<ToolApprovalDecision>(&text).unwrap(),
        null_input,
        "every representable decision must survive a round trip"
    );

    for unknown in [
        json!({"behavior": "allow", "unexpected": true}),
        json!({"behavior": "deny", "message": "no", "unexpected": true}),
        // A refusal cannot carry policy updates; the approval's field must not leak into it.
        json!({"behavior": "deny", "message": "no", "updated_permissions": []}),
    ] {
        assert!(
            serde_json::from_value::<ToolApprovalDecision>(unknown.clone()).is_err(),
            "unknown approval field must be rejected, not dropped: {unknown}"
        );
    }
    assert!(serde_json::from_value::<ToolApprovalDecision>(json!({"behavior": "ask"})).is_err());
}

#[test]
fn test_permission_update_07() {
    let updates = [
        (
            PermissionUpdate::add_rules(
                PermissionUpdateDestination::Session,
                [PermissionRule::new(PermissionDecision::Ask)],
            ),
            PermissionUpdateDestination::Session,
        ),
        (
            PermissionUpdate::replace_rules(PermissionUpdateDestination::LocalSettings, []),
            PermissionUpdateDestination::LocalSettings,
        ),
        (
            PermissionUpdate::remove_rules(
                PermissionUpdateDestination::ProjectSettings,
                [PermissionRule::new(PermissionDecision::Deny)],
            ),
            PermissionUpdateDestination::ProjectSettings,
        ),
        (
            PermissionUpdate::set_mode(
                PermissionUpdateDestination::UserSettings,
                PermissionMode::DontAsk,
            ),
            PermissionUpdateDestination::UserSettings,
        ),
    ];

    for (update, destination) in updates {
        assert_eq!(update.destination(), destination);
        let restored: PermissionUpdate =
            serde_json::from_value(serde_json::to_value(&update).unwrap()).unwrap();
        assert_eq!(restored, update);
    }

    assert_eq!(
        PermissionUpdateDestination::ProjectSettings.to_string(),
        "projectSettings"
    );
    assert!(
        serde_json::from_value::<PermissionUpdate>(json!({
            "type": "addRules",
            "rules": []
        }))
        .is_err(),
        "an update without a destination must not be given one"
    );
    assert!(
        serde_json::from_value::<PermissionUpdate>(json!({
            "type": "addRules",
            "destination": "session",
            "rules": [],
            "behavior": "allow"
        }))
        .is_err(),
        "a qualifier a newer version added must fail loudly, not widen the update"
    );
}

/// Reports what it read back through its return value rather than asserting in place: an
/// assertion inside the callback passes just as quietly when the callback is never run.
struct EchoingApprovalHandler;

#[async_trait]
impl ToolApprovalHandler for EchoingApprovalHandler {
    async fn decide(
        &self,
        approval: &ToolApproval,
        permission: &ToolPermissionContext,
        context: &RunContext,
    ) -> Result<ToolApprovalDecision> {
        Ok(ToolApprovalDecision::allow_with_input(json!({
            "run_id": context.run_id().as_str(),
            "agent_id": context.agent_id().as_str(),
            "call_id": approval.call_id().as_str(),
            "tool_name": approval.tool_name(),
            "arguments": approval.arguments(),
            "namespace": approval.namespace(),
            "permission_context": {
                "suggestions": permission.suggestions(),
                "tool_use_id": permission.tool_use_id().as_str(),
                "agent_id": permission.agent_id().map(AgentId::as_str),
                "blocked_path": permission.blocked_path(),
                "decision_reason": permission.decision_reason(),
                "title": permission.title(),
                "display_name": permission.display_name(),
                "description": permission.description(),
            },
        })))
    }
}

#[tokio::test]
async fn test_tool_approval_handler_08() {
    let agent = AgentSpec::builder()
        .id(ra_core::item::AgentId::new("agent-approval-handler"))
        .name("Approval handler")
        .build()
        .unwrap();
    let context = RunContext::new(RunId::new("run-approval-handler"), &agent);
    let approval = ToolApproval::new(
        CallId::new("call-approval-handler"),
        "exec_command",
        json!({"command": "git status"}),
    )
    .with_namespace("workspace");
    let permission = ToolPermissionContext::for_approval(&approval)
        .with_agent_id(AgentId::new("child-agent"))
        .with_blocked_path("/outside/workspace")
        .with_decision_reason("outside_workspace")
        .with_title("Approval needed to run a command")
        .with_display_name("Run command")
        .with_description("git status will inspect the working tree.");

    let decision = EchoingApprovalHandler
        .decide(&approval, &permission, &context)
        .await
        .unwrap();

    assert!(decision.is_allowed());
    assert_eq!(
        decision.updated_input(),
        Some(&json!({
            "run_id": "run-approval-handler",
            "agent_id": "agent-approval-handler",
            "call_id": "call-approval-handler",
            "tool_name": "exec_command",
            "arguments": {"command": "git status"},
            "namespace": "workspace",
            "permission_context": {
                "suggestions": [],
                "tool_use_id": "call-approval-handler",
                "agent_id": "child-agent",
                "blocked_path": "/outside/workspace",
                "decision_reason": "outside_workspace",
                "title": "Approval needed to run a command",
                "display_name": "Run command",
                "description": "git status will inspect the working tree.",
            },
        })),
        "the handler must see the live run context, pending record, and renderable approval context"
    );
}

#[test]
fn test_tool_permission_context_10() {
    let suggestion = PermissionUpdate::add_rules(
        PermissionUpdateDestination::LocalSettings,
        [PermissionRule::new(PermissionDecision::Allow)
            .with_tool_name("exec_command")
            .with_namespace("workspace")],
    );
    let approval = ToolApproval::new(
        CallId::new("call-permission-context"),
        "exec_command",
        json!({"command": "git status"}),
    );
    let permission = ToolPermissionContext::for_approval(&approval)
        .with_suggestions([suggestion.clone()])
        .with_agent_id(AgentId::new("child-agent"))
        .with_blocked_path("/workspace/../outside")
        .with_decision_reason("outside_workspace")
        .with_title("Approval needed to access a path outside the workspace")
        .with_display_name("Run command")
        .with_description("The command can inspect a path outside the workspace.");

    assert_eq!(permission.suggestions(), [suggestion]);
    assert_eq!(permission.tool_use_id().as_str(), "call-permission-context");
    assert_eq!(
        permission.agent_id().map(AgentId::as_str),
        Some("child-agent")
    );
    assert_eq!(permission.blocked_path(), Some("/workspace/../outside"));
    assert_eq!(permission.decision_reason(), Some("outside_workspace"));
    assert_eq!(
        permission.title(),
        Some("Approval needed to access a path outside the workspace")
    );
    assert_eq!(permission.display_name(), Some("Run command"));
    assert_eq!(
        permission.description(),
        Some("The command can inspect a path outside the workspace.")
    );

    let value = serde_json::to_value(&permission).expect("permission context must serialize");
    assert_eq!(
        value,
        json!({
            "suggestions": [{
                "type": "addRules",
                "destination": "localSettings",
                "rules": [{
                    "decision": "allow",
                    "tool_name": "exec_command",
                    "namespace": "workspace"
                }]
            }],
            "tool_use_id": "call-permission-context",
            "agent_id": "child-agent",
            "blocked_path": "/workspace/../outside",
            "decision_reason": "outside_workspace",
            "title": "Approval needed to access a path outside the workspace",
            "display_name": "Run command",
            "description": "The command can inspect a path outside the workspace."
        })
    );
    let restored: ToolPermissionContext =
        serde_json::from_value(value).expect("permission context must deserialize");
    assert_eq!(restored, permission);

    assert!(permission.matches_approval(&approval));
    let different_approval = ToolApproval::new(
        CallId::new("call-other"),
        "exec_command",
        json!({"command": "git status"}),
    );
    assert!(!permission.matches_approval(&different_approval));

    let newer_context = serde_json::from_value::<ToolPermissionContext>(json!({
        "tool_use_id": "call-permission-context",
        "title": "Approval needed",
        "renderer_hint": {"emphasis": "danger"}
    }))
    .expect("a newer context field must be retained");
    assert_eq!(
        newer_context.unknown().get("renderer_hint"),
        Some(&json!({"emphasis": "danger"}))
    );
    assert_eq!(
        serde_json::to_value(&newer_context).unwrap()["renderer_hint"],
        json!({"emphasis": "danger"}),
        "an older runtime must write newer UI data back unchanged"
    );
}

/// The correlation ID has one source of truth: `for_approval` copies it from the pending record
/// and there is no setter. This pins the other half — the wire form cannot supply an absent one
/// either. Every other field on this type carries `#[serde(default)]`, so adding one here for
/// symmetry would look right and silently turn a missing ID into a `CallId` matching no call.
#[test]
fn test_tool_permission_context_11() {
    assert!(
        serde_json::from_value::<ToolPermissionContext>(json!({"title": "Approval needed"}))
            .is_err(),
        "a context without a correlation ID must fail to load, not default to an empty one"
    );
    assert!(
        serde_json::from_value::<ToolPermissionContext>(json!({"tool_use_id": null})).is_err(),
        "an explicit null must not stand in for the pending call's ID"
    );
    assert!(
        serde_json::from_str::<ToolPermissionContext>(
            r#"{"tool_use_id":"call-a","tool_use_id":"call-b"}"#
        )
        .is_err(),
        "a second correlation ID must not quietly override the first"
    );

    // The mirror image: the ID alone is enough, so the seven optional fields really are optional
    // on the way in and a persisted context stays pairable with its record.
    let approval = ToolApproval::new(CallId::new("call-a"), "exec_command", json!({}));
    let restored =
        serde_json::from_value::<ToolPermissionContext>(json!({"tool_use_id": "call-a"}))
            .expect("a context carrying only its correlation ID must load");
    assert!(restored.matches_approval(&approval));
    assert_eq!(restored.suggestions(), []);
    assert_eq!(restored.agent_id(), None);
    assert_eq!(restored.title(), None);
}
