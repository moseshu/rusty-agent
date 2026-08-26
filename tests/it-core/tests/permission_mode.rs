//! Permission-mode contracts.

use ra_core::permission::{PermissionDecision, PermissionMode, PermissionRule, PermissionScope};
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
