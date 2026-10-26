//! Normalized identity and managed authority invariants from upstream discovery.
#![allow(clippy::expect_used)]

use ra_coding::{
    config::{HookHandlerConfig, HookStateToml, HooksFile, MatcherGroup},
    hooks::trust::*,
};
use ra_core::hook::HookEventName;
use ra_protocol::server::protocol::HookTrustStatus;

fn handler() -> HookHandlerConfig {
    serde_json::from_value(
        serde_json::json!({"type":"command", "command":"echo hook", "timeout":600}),
    )
    .expect("handler")
}

#[test]
fn normalized_identity_has_a_fixed_hash_and_excludes_siblings() {
    let config = handler();
    let group = MatcherGroup {
        matcher: Some("Bash".into()),
        hooks: vec![config.clone()],
    };
    let hash = hook_hash(HookEventName::PreToolUse, Some("Bash"), &group, &config);
    assert_eq!(
        hash,
        "sha256:4cda596a4f7b6ea199f0b7e23ce955832c9f5246cb2e7e3bb1dadcf180876d51"
    );
    let mut with_siblings = group.clone();
    with_siblings.hooks.push(
        serde_json::from_value(serde_json::json!({"type":"command", "command":"different"}))
            .expect("sibling"),
    );
    assert_eq!(
        hook_hash(
            HookEventName::PreToolUse,
            Some("Bash"),
            &with_siblings,
            &config
        ),
        hash
    );
    assert_ne!(
        hook_hash(HookEventName::PostToolUse, Some("Bash"), &group, &config),
        hash
    );
    assert_ne!(
        hook_hash(HookEventName::PreToolUse, Some("Edit"), &group, &config),
        hash
    );
}

#[test]
fn parsed_toml_and_json_share_the_same_handler_identity() {
    let json: HooksFile = serde_json::from_str(r#"{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"echo hook", "timeout":600}]}]}}"#).expect("JSON");
    let toml: HooksFile = toml::from_str("[[hooks.PreToolUse]]\nmatcher = 'Bash'\n[[hooks.PreToolUse.hooks]]\ntype = 'command'\ncommand = 'echo hook'\ntimeout = 600").expect("TOML");
    let a = &json.hooks.pre_tool_use[0];
    let b = &toml.hooks.pre_tool_use[0];
    assert_eq!(
        hook_hash(
            HookEventName::PreToolUse,
            a.matcher.as_deref(),
            a,
            &a.hooks[0]
        ),
        hook_hash(
            HookEventName::PreToolUse,
            b.matcher.as_deref(),
            b,
            &b.hooks[0]
        )
    );
}

#[test]
fn managed_and_builtin_authority_preserve_original_precedence() {
    let disabled = HookStateToml {
        enabled: Some(false),
        trusted_hash: Some("old".into()),
    };
    assert_eq!(
        hook_trust_status(false, false, "current", None),
        HookTrustStatus::Untrusted
    );
    assert_eq!(
        hook_trust_status(false, false, "current", Some("old")),
        HookTrustStatus::Modified
    );
    assert_eq!(
        hook_trust_status(false, false, "current", Some("current")),
        HookTrustStatus::Trusted
    );
    for managed in [false, true] {
        for builtin in [false, true] {
            assert_eq!(
                hook_enabled(managed, builtin, Some(&disabled)),
                managed || builtin
            );
            assert!(hook_enabled(managed, builtin, None));
            if builtin {
                assert_eq!(
                    hook_trust_status(managed, builtin, "current", Some("old")),
                    HookTrustStatus::Trusted
                );
            } else if managed {
                assert_eq!(
                    hook_trust_status(managed, builtin, "current", Some("old")),
                    HookTrustStatus::Managed
                );
            }
        }
    }
    assert_eq!(hook_trusted_hash(true, Some(&disabled)), None);
    assert_eq!(hook_trusted_hash(false, Some(&disabled)), Some("old"));
    assert_eq!(hook_trusted_hash(false, None), None);
}

#[test]
fn legacy_plugin_declarations_are_the_same_config_type() {
    let legacy: ra_coding::plugins::hook_config::HooksFile = HooksFile::default();
    let canonical: HooksFile = legacy;
    assert_eq!(canonical, HooksFile::default());
}

#[test]
#[should_panic(expected = "normalized hook identity should serialize to TOML")]
fn json_timeout_above_toml_integer_range_preserves_upstream_hash_panic() {
    let hooks: HooksFile = serde_json::from_str(
        r#"{"hooks":{"PreToolUse":[{"hooks":[{"type":"command","command":"echo hook","timeout":18446744073709551615}]}]}}"#,
    )
    .expect("valid JSON hook declaration");
    let group = &hooks.hooks.pre_tool_use[0];
    let mut normalized = group.hooks[0].clone();
    if let HookHandlerConfig::Command { timeout_sec, .. } = &mut normalized {
        let mut warnings = Vec::new();
        *timeout_sec = Some(ra_coding::hooks::timing::normalize_command_hook(
            HookEventName::PreToolUse,
            *timeout_sec,
            std::path::Path::new("hooks.json"),
            &mut warnings,
        ));
        assert_eq!(*timeout_sec, Some(u64::MAX));
        assert!(warnings.is_empty());
    } else {
        panic!("expected a command handler");
    }
    let _ = hook_hash(HookEventName::PreToolUse, None, group, &normalized);
}

#[cfg(target_pointer_width = "64")]
#[test]
#[should_panic(expected = "normalized hook identity should serialize to TOML")]
fn json_context_limit_above_toml_integer_range_preserves_upstream_hash_panic() {
    let hooks: HooksFile = serde_json::from_str(
        r#"{"hooks":{"PreToolUse":[{"hooks":[{"type":"command","command":"echo hook","timeout":600,"additionalContextLimit":18446744073709551615}]}]}}"#,
    )
    .expect("valid JSON hook declaration");
    let group = &hooks.hooks.pre_tool_use[0];
    let normalized = &group.hooks[0];
    assert!(matches!(
        normalized,
        HookHandlerConfig::Command {
            additional_context_limit: Some(limit),
            ..
        } if *limit == usize::MAX
    ));
    let _ = hook_hash(HookEventName::PreToolUse, None, group, normalized);
}
