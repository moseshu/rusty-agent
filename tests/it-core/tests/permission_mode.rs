//! Provider-neutral permission decisions, action classes, and rules.

use ra_core::{
    item::AgentId,
    permission::{ActionClass, PermissionDecision, PermissionRule},
    tool::{ToolNamespace, ToolOrigin},
};
use serde_json::json;

#[test]
fn permission_decisions_keep_stable_wire_labels() {
    for (decision, label) in [
        (PermissionDecision::Allow, "allow"),
        (PermissionDecision::Deny, "deny"),
        (PermissionDecision::Ask, "ask"),
    ] {
        assert_eq!(decision.label(), label);
        assert_eq!(decision.to_string(), label);
        assert_eq!(serde_json::to_value(decision).unwrap(), json!(label));
        assert_eq!(
            serde_json::from_value::<PermissionDecision>(json!(label)).unwrap(),
            decision
        );
    }
}

#[test]
fn action_classes_are_open_string_identifiers() {
    let class = ActionClass::new("workspace.inspect");
    assert_eq!(class.as_str(), "workspace.inspect");
    assert_eq!(class.to_string(), "workspace.inspect");
    assert_eq!(
        serde_json::to_value(&class).unwrap(),
        json!("workspace.inspect")
    );
    assert_eq!(
        serde_json::from_value::<ActionClass>(json!("workspace.inspect")).unwrap(),
        class
    );
}

#[test]
fn permission_rules_match_namespaces_and_roundtrip() {
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
fn pinned_permission_rules_match_only_their_executable() {
    let bare = ToolOrigin::new("write_file").expect("bare origin must build");
    let namespaced = ToolOrigin::namespaced(
        ToolNamespace::new("mcp_fs").expect("namespace must build"),
        "write_file",
    )
    .expect("namespaced origin must build");

    let name_rule = PermissionRule::new(PermissionDecision::Allow).with_tool_name("write_file");
    let pinned = PermissionRule::new(PermissionDecision::Allow)
        .with_tool_name("write_file")
        .with_lookup_key(bare.lookup_key().clone());

    assert!(name_rule.matches_origin(&bare));
    assert!(name_rule.matches_origin(&namespaced));
    assert!(pinned.matches_origin(&bare));
    assert!(!pinned.matches_origin(&namespaced));
    assert_eq!(pinned.lookup_key(), Some(bare.lookup_key()));
    assert!(!pinned.matches("write_file", None));

    let value = serde_json::to_value(&pinned).expect("pinned rule must serialize");
    assert_eq!(value["lookup_key"]["name"], "write_file");
    let restored: PermissionRule = serde_json::from_value(value).expect("rule must deserialize");
    assert_eq!(restored, pinned);
    assert!(!restored.matches_origin(&namespaced));
}

#[test]
fn agent_scoped_rules_do_not_match_a_same_named_agent() {
    let origin = ToolOrigin::new("operate").expect("origin must build");
    let first_agent = AgentId::new("first");
    let second_agent = AgentId::new("second");
    let rule = PermissionRule::new(PermissionDecision::Allow)
        .with_tool_name("operate")
        .with_agent_id(first_agent.clone());

    assert!(!rule.matches_origin(&origin));
    assert!(!rule.matches("operate", None));
    assert!(rule.matches_origin_for_agent(&origin, &first_agent));
    assert!(!rule.matches_origin_for_agent(&origin, &second_agent));
    let restored: PermissionRule =
        serde_json::from_value(serde_json::to_value(&rule).unwrap()).unwrap();
    assert_eq!(restored.agent_id(), Some(&first_agent));
}
