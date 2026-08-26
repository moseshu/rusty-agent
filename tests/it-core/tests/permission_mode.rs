//! Permission-mode contracts.

use ra_core::permission::PermissionMode;
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
