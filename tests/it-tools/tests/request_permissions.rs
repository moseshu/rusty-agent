//! `request_permissions`: Codex's tool for asking the user for permissions, its schema and
//! description ported from `shell_spec.rs` and its calls handed to the host's policy.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ra_core::{
    agent::AgentSpec,
    context::RunContext,
    item::{AgentId, CallId},
    permission::ActionClass,
    state::RunId,
    tool::{Tool, ToolConcurrency, ToolContext, ToolOutput},
};
use ra_tools::request_permissions::{RequestPermissionsPolicy, RequestPermissionsTool};
use serde_json::{Value, json};

/// A policy answering every call with `answer`, recording the arguments it was given.
struct ScriptedPolicy {
    answer: Result<String, String>,
    seen: Mutex<Vec<Value>>,
}

#[async_trait]
impl RequestPermissionsPolicy for ScriptedPolicy {
    async fn request_permissions(
        &self,
        context: &ToolContext<'_>,
    ) -> std::result::Result<String, String> {
        self.seen.lock().unwrap().push(context.arguments().clone());
        self.answer.clone()
    }
}

fn tool(answer: Result<String, String>) -> (RequestPermissionsTool, Arc<ScriptedPolicy>) {
    let policy = Arc::new(ScriptedPolicy {
        answer,
        seen: Mutex::new(Vec::new()),
    });
    (
        RequestPermissionsTool::new(Arc::clone(&policy) as Arc<dyn RequestPermissionsPolicy>)
            .expect("the tool builds"),
        policy,
    )
}

async fn observe(tool: &RequestPermissionsTool, arguments: &Value) -> ToolOutput {
    let agent = AgentSpec::builder()
        .id(AgentId::new("asker"))
        .name("Asker")
        .build()
        .expect("an agent");
    let run = RunContext::new(RunId::new("turn-1"), agent.as_ref());
    let call_id = CallId::new("call-1");
    let context = || ToolContext::new(&run, tool, &call_id, arguments);
    match tool.call(context()).await {
        Ok(output) => output,
        Err(error) => tool
            .handle_failure(&context(), &error)
            .await
            .expect("failure shaping must not fail")
            .expect("request_permissions shapes every refusal"),
    }
}

/// Codex's `request_permissions_tool_includes_full_permission_schema`, with its description.
#[test]
fn request_permissions_tool_includes_full_permission_schema() {
    let (tool, _) = tool(Ok(String::new()));

    tool.validate().expect("identity and schema must agree");
    assert_eq!(tool.origin().qualified_name(), "request_permissions");
    assert!(!tool.schema().strict_json_schema());
    assert_eq!(
        tool.schema().description(),
        Some(
            "Request additional filesystem or network permissions from the user and wait for the client to grant a subset of the requested permission profile. Use environment_id to target a specific attached environment; omit it to use the primary environment. Relative filesystem paths resolve against the selected environment cwd. Granted permissions apply automatically to later shell-like commands in the current turn, or for the rest of the session if the client approves them at session scope."
        )
    );
    assert_eq!(
        tool.schema().input_schema(),
        &json!({
            "type": "object",
            "properties": {
                "environment_id": {
                    "type": "string",
                    "description": "Environment id from <environment_context>. Omit to use the primary environment.",
                },
                "permissions": {
                    "type": "object",
                    "description": "Filesystem or network access request.",
                    "properties": {
                        "file_system": {
                            "type": "object",
                            "description": "Filesystem access request.",
                            "properties": {
                                "read": {
                                    "type": "array",
                                    "description": "Absolute paths to grant read access; omit when none are needed.",
                                    "items": {"type": "string"},
                                },
                                "write": {
                                    "type": "array",
                                    "description": "Absolute paths to grant write access; omit when none are needed.",
                                    "items": {"type": "string"},
                                },
                            },
                            "additionalProperties": false,
                        },
                        "network": {
                            "type": "object",
                            "description": "Network access request.",
                            "properties": {
                                "enabled": {
                                    "type": "boolean",
                                    "description": "True requests network access; false or omitted requests none.",
                                },
                            },
                            "additionalProperties": false,
                        },
                    },
                    "additionalProperties": false,
                },
                "reason": {
                    "type": "string",
                    "description": "Optional short explanation for why additional permissions are needed.",
                },
            },
            "required": ["permissions"],
            "additionalProperties": false,
        })
    );
    assert_eq!(tool.options().concurrency(), ToolConcurrency::Exclusive);
    assert_eq!(
        tool.options().action_classes(),
        &[ActionClass::from("read")]
    );
}

/// The policy is handed the call's arguments, and the model is given what it returns.
#[tokio::test]
async fn the_policy_answers_the_call() {
    let (tool, policy) = tool(Ok(r#"{"scope":"turn"}"#.to_owned()));
    let arguments = json!({"permissions": {"network": {"enabled": true}}});

    let output = observe(&tool, &arguments).await;

    assert_eq!(output.as_text(), Some(r#"{"scope":"turn"}"#));
    assert_eq!(*policy.seen.lock().unwrap(), vec![arguments]);
}

/// A refusal reaches the model as its text, as Codex's `RespondToModel` does.
#[tokio::test]
async fn a_refusal_is_shown_to_the_model() {
    let (tool, _) = tool(Err(
        "request_permissions requires at least one permission".to_owned()
    ));

    let output = observe(&tool, &json!({"permissions": {}})).await;

    assert_eq!(
        output.as_text(),
        Some("request_permissions requires at least one permission")
    );
}
