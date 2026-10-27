// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Cleanup exceptions preserve exact package, event, target and connector identities.

use ra_coding::{config::HookHandlerConfig, plugins::is_allowlisted_bundled_cleanup_hook};
use ra_core::hook::HookEventName;
use serde_json::{Value, json};

fn handler(value: Value) -> HookHandlerConfig {
    serde_json::from_value(value).expect("valid hook handler")
}

fn cleanup(server: &str) -> HookHandlerConfig {
    handler(json!({"type":"mcp_tool","server":server,"tool":"turn_ended"}))
}

#[test]
fn bundled_cleanup_targets_admit_only_the_original_shutdown_events() {
    for (plugin_id, server) in [
        ("browser@openai-bundled", "node_repl"),
        ("chrome@openai-bundled", "node_repl"),
        ("chrome-dev@openai-bundled", "node_repl"),
        ("chrome-internal@openai-bundled", "node_repl"),
        ("computer-use@openai-bundled", "node_repl"),
        ("unified-computer-use@openai-bundled", "cua_repl"),
    ] {
        let handler = cleanup(server);
        for event in [
            HookEventName::Stop,
            HookEventName::Interrupt,
            HookEventName::SubagentStop,
        ] {
            assert!(is_allowlisted_bundled_cleanup_hook(
                plugin_id, event, None, &handler, None
            ));
        }
        for event in [
            HookEventName::PreToolUse,
            HookEventName::PermissionRequest,
            HookEventName::PostToolUse,
            HookEventName::UserPromptSubmit,
            HookEventName::SessionStart,
            HookEventName::SessionEnd,
            HookEventName::SubagentStart,
            HookEventName::PreCompact,
            HookEventName::PostCompact,
        ] {
            assert!(!is_allowlisted_bundled_cleanup_hook(
                plugin_id, event, None, &handler, None
            ));
        }
    }
}

#[test]
fn package_matcher_handler_and_target_must_match_exactly() {
    let allowed = cleanup("node_repl");
    for plugin_id in [
        "another-plugin@openai-bundled",
        "computer-use@other",
        "computer-use@openai-bundled-alpha",
        "computer-use@fake-openai-bundled",
        "Computer-use@openai-bundled",
    ] {
        assert!(!is_allowlisted_bundled_cleanup_hook(
            plugin_id,
            HookEventName::Stop,
            None,
            &allowed,
            None
        ));
    }
    let plugin_id = "computer-use@openai-bundled";
    for matcher in ["", "*", "unexpected"] {
        assert!(!is_allowlisted_bundled_cleanup_hook(
            plugin_id,
            HookEventName::Stop,
            Some(matcher),
            &allowed,
            None
        ));
    }
    for forbidden in [
        cleanup("other"),
        cleanup("cua_repl"),
        handler(json!({"type":"mcp_tool","server":"node_repl","tool":"other"})),
        handler(json!({"type":"command","command":"echo cleanup"})),
    ] {
        assert!(!is_allowlisted_bundled_cleanup_hook(
            plugin_id,
            HookEventName::Stop,
            None,
            &forbidden,
            None
        ));
    }
}

#[test]
fn bundled_mcp_cleanup_keeps_manifest_input_and_options() {
    let configured = handler(json!({
        "type":"mcp_tool","server":"node_repl","tool":"turn_ended",
        "input":{"untrusted":"manifest-provided input"},
        "timeout":30,"statusMessage":"Cleaning up Computer Use"
    }));
    let original = configured.clone();
    assert!(is_allowlisted_bundled_cleanup_hook(
        "computer-use@openai-bundled",
        HookEventName::Stop,
        None,
        &configured,
        None
    ));
    assert_eq!(configured, original);
}

#[test]
fn app_cleanup_requires_the_registered_connector_and_empty_input() {
    let configured =
        handler(json!({"type":"mcp_tool","server":"codex_apps","tool":"browser.turn_ended"}));
    let plugin_id = "browser@openai-curated-remote";
    for event in [
        HookEventName::Stop,
        HookEventName::Interrupt,
        HookEventName::SubagentStop,
    ] {
        assert!(is_allowlisted_bundled_cleanup_hook(
            plugin_id,
            event,
            None,
            &configured,
            Some("connector_openai_browser")
        ));
    }
    for connector in [
        None,
        Some("connector_other"),
        Some("connector_openai_browser_suffix"),
    ] {
        assert!(!is_allowlisted_bundled_cleanup_hook(
            plugin_id,
            HookEventName::Stop,
            None,
            &configured,
            connector
        ));
    }
    for forbidden in [
        handler(json!({"type":"mcp_tool","server":"other","tool":"browser.turn_ended"})),
        handler(json!({"type":"mcp_tool","server":"codex_apps","tool":"turn_ended"})),
        handler(
            json!({"type":"mcp_tool","server":"codex_apps","tool":"browser.turn_ended","input":{"injected":true}}),
        ),
    ] {
        assert!(!is_allowlisted_bundled_cleanup_hook(
            plugin_id,
            HookEventName::Stop,
            None,
            &forbidden,
            Some("connector_openai_browser")
        ));
    }
    assert!(!is_allowlisted_bundled_cleanup_hook(
        "browser@other",
        HookEventName::Stop,
        None,
        &configured,
        Some("connector_openai_browser")
    ));
}
