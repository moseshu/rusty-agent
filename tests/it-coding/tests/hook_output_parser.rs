// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.

use serde_json::json;

use ra_coding::hooks::output_parser::*;

#[test]
fn outputs_require_one_complete_json_object() {
    for input in ["", "  ", "[]", "null", "true", "{}{}", "log\n{}", "{}\nlog"] {
        assert!(parse_stop(input).is_none(), "{input:?}");
    }
    let parsed = parse_stop(" \n {} \t").expect("object");
    assert!(parsed.universal.continue_processing);
    assert!(!parsed.should_block);
    assert!(looks_like_json("\n ["));
    assert!(!looks_like_json("log {}"));
}

#[test]
fn pre_tool_rewrite_and_denial_keep_the_original_decision_precedence() {
    let rewrite = parse_pre_tool_use(&json!({"decision":"approve", "hookSpecificOutput":{
        "hookEventName":"PreToolUse", "permissionDecision":"allow", "updatedInput":{"path":"new"}
    }}).to_string()).expect("rewrite");
    assert_eq!(rewrite.updated_input, Some(json!({"path":"new"})));
    assert!(rewrite.invalid_reason.is_none());
    for specific in [
        json!({"permissionDecision":"allow"}),
        json!({"permissionDecision":"ask"}),
        json!({"updatedInput":{}}),
        json!({"permissionDecision":"deny", "permissionDecisionReason":"  "}),
    ] {
        let mut specific = specific;
        specific["hookEventName"] = json!("PreToolUse");
        let output = parse_pre_tool_use(&json!({"hookSpecificOutput":specific}).to_string())
            .expect("typed output");
        assert!(output.invalid_reason.is_some());
        assert!(output.updated_input.is_none());
        assert!(output.block_reason.is_none());
    }
    let deny = parse_pre_tool_use(r#"{"decision":"approve","hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"  denied  "}}"#).expect("denial");
    assert_eq!(deny.block_reason.as_deref(), Some("denied"));
    assert!(deny.invalid_reason.is_none());
    let legacy =
        parse_pre_tool_use(r#"{"decision":"block","reason":" denied "}"#).expect("legacy block");
    assert_eq!(legacy.block_reason.as_deref(), Some("denied"));
    assert!(
        parse_pre_tool_use(r#"{"decision":"approve"}"#)
            .expect("legacy approve")
            .invalid_reason
            .is_some()
    );
}

#[test]
fn permission_decisions_validate_universal_fields_before_retaining_authority() {
    let base = json!({"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow"}}});
    let allowed = parse_permission_request(&base.to_string()).expect("allow");
    assert_eq!(allowed.decision, Some(PermissionRequestDecision::Allow));
    for (key, value) in [
        ("continue", json!(false)),
        ("stopReason", json!("stop")),
        ("suppressOutput", json!(true)),
    ] {
        let mut invalid = base.clone();
        invalid[key] = value;
        let output = parse_permission_request(&invalid.to_string()).expect("typed output");
        assert!(output.invalid_reason.is_some());
        assert!(output.decision.is_none());
    }
    let denied = parse_permission_request(r#"{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"deny","message":"  "}}}"#).expect("deny");
    assert_eq!(
        denied.decision,
        Some(PermissionRequestDecision::Deny {
            message: "PermissionRequest hook denied approval".into()
        })
    );
}

#[test]
fn post_tool_feedback_requires_a_reason_and_preserves_unsupported_field_diagnostics() {
    assert!(
        parse_post_tool_use(r#"{"decision":"block","reason":"fix this"}"#)
            .expect("feedback")
            .should_block
    );
    let invalid =
        parse_post_tool_use(r#"{"decision":"block","reason":"  "}"#).expect("blank reason");
    assert!(!invalid.should_block);
    assert!(invalid.invalid_block_reason.is_some());
    let invalid =
        parse_post_tool_use(r#"{"suppressOutput":true,"decision":"block","reason":"fix this"}"#)
            .expect("suppress");
    assert!(!invalid.should_block);
    assert!(invalid.invalid_reason.is_some());
    let invalid = parse_post_tool_use(
        r#"{"hookSpecificOutput":{"hookEventName":"PostToolUse","updatedMCPToolOutput":{}}}"#,
    )
    .expect("reserved output");
    assert_eq!(
        invalid.invalid_reason.as_deref(),
        Some("PostToolUse hook returned unsupported updatedMCPToolOutput")
    );
}

#[test]
fn startup_compaction_stop_and_interrupt_keep_distinct_output_contracts() {
    assert_eq!(parse_session_start(r#"{"hookSpecificOutput":{"hookEventName":"SessionStart","additionalContext":"context"}}"#).expect("start").additional_context.as_deref(), Some("context"));
    assert_eq!(parse_subagent_start(r#"{"hookSpecificOutput":{"hookEventName":"SubagentStart","additionalContext":"child"}}"#).expect("subagent start").additional_context.as_deref(), Some("child"));
    assert!(
        !parse_pre_compact(r#"{"continue":false}"#)
            .expect("pre compact")
            .universal
            .continue_processing
    );
    assert!(
        parse_post_compact("{}")
            .expect("post compact")
            .invalid_reason
            .is_none()
    );
    for parse in [parse_stop, parse_subagent_stop] {
        assert!(
            parse(r#"{"decision":"block","reason":"continue"}"#)
                .expect("block")
                .should_block
        );
        assert!(
            !parse(r#"{"decision":"block","reason":" "}"#)
                .expect("invalid block")
                .should_block
        );
    }
    assert_eq!(
        parse_interrupt(r#"{"systemMessage":"stopped"}"#)
            .expect("interrupt")
            .system_message
            .as_deref(),
        Some("stopped")
    );
    assert!(parse_interrupt(r#"{"continue":false}"#).is_none());
}

use ra_coding::hooks::output_parser::parse_permission_request;
use ra_coding::hooks::output_parser::parse_user_prompt_submit;

#[test]
fn structured_output_rejects_invalid_shapes_and_types() {
    for stdout in [
        "[]",
        r#"{"systemMessage":123}"#,
        r#"{"hookSpecificOutput":{"hookEventName":"UserPromptSubmit","additionalContext":123}}"#,
        r#"{"hookSpecificOutput":{"additionalContext":"missing event name"}}"#,
        r#"{"hookSpecificOutput":{"hookEventName":"UserPromptSubmit","updatedInput":{}}}"#,
        r#"{"unexpectedField":true}"#,
        "{",
    ] {
        assert!(
            parse_user_prompt_submit(stdout).is_none(),
            "invalid structured output should fail: {stdout}",
        );
    }
}

#[test]
fn permission_request_rejects_reserved_updated_input_field() {
    let parsed = parse_permission_request(
        &json!({
            "continue": true,
            "hookSpecificOutput": {
                "hookEventName": "PermissionRequest",
                "decision": {
                    "behavior": "allow",
                    "updatedInput": {}
                }
            }
        })
        .to_string(),
    )
    .expect("permission request hook output should parse");

    assert_eq!(
        parsed.invalid_reason,
        Some("PermissionRequest hook returned unsupported updatedInput".to_string())
    );
}

#[test]
fn permission_request_rejects_reserved_updated_permissions_field() {
    let parsed = parse_permission_request(
        &json!({
            "continue": true,
            "hookSpecificOutput": {
                "hookEventName": "PermissionRequest",
                "decision": {
                    "behavior": "allow",
                    "updatedPermissions": {}
                }
            }
        })
        .to_string(),
    )
    .expect("permission request hook output should parse");

    assert_eq!(
        parsed.invalid_reason,
        Some("PermissionRequest hook returned unsupported updatedPermissions".to_string())
    );
}

#[test]
fn permission_request_rejects_reserved_interrupt_field() {
    let parsed = parse_permission_request(
        &json!({
            "continue": true,
            "hookSpecificOutput": {
                "hookEventName": "PermissionRequest",
                "decision": {
                    "behavior": "allow",
                    "interrupt": true
                }
            }
        })
        .to_string(),
    )
    .expect("permission request hook output should parse");

    assert_eq!(
        parsed.invalid_reason,
        Some("PermissionRequest hook returned unsupported interrupt:true".to_string())
    );
}
