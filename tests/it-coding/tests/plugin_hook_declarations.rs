// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
use ra_coding::plugins::hook_config::{HookEventsToml, HookHandlerConfig, HooksFile, MatcherGroup};

#[test]
fn hooks_file_deserializes_existing_json_shape() {
    let parsed: HooksFile = serde_json::from_str(
        r#"{
  "description": "Optional stop-time review gate for Codex Companion.",
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "^Bash$",
        "hooks": [
          {
            "type": "command",
            "command": "python3 /tmp/pre.py",
            "timeout": 10,
            "statusMessage": "checking",
            "additionalContextLimit": 4096
          }
        ]
      }
    ]
  }
}"#,
    )
    .expect("hooks.json should deserialize");

    assert_eq!(
        parsed,
        HooksFile {
            description: Some("Optional stop-time review gate for Codex Companion.".to_string()),
            hooks: HookEventsToml {
                pre_tool_use: vec![MatcherGroup {
                    matcher: Some("^Bash$".to_string()),
                    hooks: vec![HookHandlerConfig::Command {
                        command: "python3 /tmp/pre.py".to_string(),
                        command_windows: None,
                        timeout_sec: Some(10),
                        r#async: false,
                        status_message: Some("checking".to_string()),
                        additional_context_limit: Some(4096),
                    }],
                }],
                ..Default::default()
            },
        }
    );
}

#[test]
fn hooks_file_deserializes_mcp_tool_handler_with_json_inputs() {
    let parsed: HooksFile = serde_json::from_value(serde_json::json!({
        "hooks": {
            "PostToolUse": [{
                "matcher": "Write|Edit",
                "hooks": [{
                    "type": "mcp_tool",
                    "server": "security",
                    "tool": "scan",
                    "input": {
                        "file_path": "${tool_input.file_path}",
                        "include_ignored": false,
                    },
                    "timeout": 30,
                    "statusMessage": "Scanning file",
                }],
            }],
        },
    }))
    .expect("MCP tool hooks should deserialize");

    assert_eq!(
        parsed.hooks.post_tool_use[0].hooks,
        vec![HookHandlerConfig::McpTool {
            server: "security".to_string(),
            tool: "scan".to_string(),
            input: serde_json::Map::from_iter([
                (
                    "file_path".to_string(),
                    serde_json::Value::String("${tool_input.file_path}".to_string()),
                ),
                (
                    "include_ignored".to_string(),
                    serde_json::Value::Bool(false)
                ),
            ]),
            timeout_sec: Some(30),
            status_message: Some("Scanning file".to_string()),
        }]
    );
}

#[test]
fn hooks_file_rejects_mcp_tool_handler_with_null_input() {
    for input in [
        serde_json::json!({ "optional": null }),
        serde_json::json!({ "metadata": { "optional": null } }),
        serde_json::json!({ "values": [null] }),
    ] {
        let error = serde_json::from_value::<HooksFile>(serde_json::json!({
            "hooks": {
                "PostToolUse": [{
                    "hooks": [{
                        "type": "mcp_tool",
                        "server": "security",
                        "tool": "scan",
                        "input": input,
                    }],
                }],
            },
        }))
        .expect_err("literal null MCP hook arguments should be rejected");

        assert!(
            error
                .to_string()
                .contains("MCP hook input must be representable as TOML"),
            "unexpected parse error: {error}"
        );
    }
}

#[test]
fn hooks_file_rejects_events_outside_hooks_object() {
    let error = serde_json::from_str::<HooksFile>(
        r#"{
  "SessionStart": [
    {
      "hooks": [
        {
          "type": "command",
          "command": "python3 /tmp/session_start.py"
        }
      ]
    }
  ]
}"#,
    )
    .expect_err("root-level hook events should be rejected");

    assert!(
        error.to_string().contains("unknown field `SessionStart`"),
        "unexpected parse error: {error}"
    );
}

#[test]
fn hook_handler_omits_unset_additional_context_limit() {
    let handler = HookHandlerConfig::Command {
        command: "python3 /tmp/pre.py".to_string(),
        command_windows: None,
        timeout_sec: None,
        r#async: false,
        status_message: None,
        additional_context_limit: None,
    };

    let serialized = serde_json::to_value(handler).expect("hook handler should serialize");

    assert_eq!(serialized.get("additionalContextLimit"), None);
}

fn assert_windows_override_from_json(field: &str) {
    let command_windows = r"powershell -File C:\enterprise\hooks\pre.ps1";
    let mut value = serde_json::json!({
        "hooks": {
            "PreToolUse": [{
                "matcher": "^Bash$",
                "hooks": [{"type": "command", "command": "bash /enterprise/hooks/pre.sh"}],
            }],
        },
    });
    value["hooks"]["PreToolUse"][0]["hooks"][0][field] = command_windows.into();
    let parsed: HooksFile = serde_json::from_value(value).expect("Windows override JSON");
    assert_eq!(
        parsed.hooks,
        HookEventsToml {
            pre_tool_use: vec![MatcherGroup {
                matcher: Some("^Bash$".into()),
                hooks: vec![HookHandlerConfig::Command {
                    command: "bash /enterprise/hooks/pre.sh".into(),
                    command_windows: Some(command_windows.into()),
                    timeout_sec: None,
                    r#async: false,
                    status_message: None,
                    additional_context_limit: None,
                }],
            }],
            ..Default::default()
        }
    );
    let serialized = serde_json::to_value(parsed).unwrap();
    let handler = &serialized["hooks"]["PreToolUse"][0]["hooks"][0];
    assert_eq!(handler["commandWindows"], command_windows);
    assert!(handler.get("command_windows").is_none());
}

#[test]
fn hook_events_deserialize_windows_override_from_json() {
    assert_windows_override_from_json("command_windows");
}

#[test]
fn hook_events_deserialize_camel_case_windows_override_from_json() {
    assert_windows_override_from_json("commandWindows");
}
