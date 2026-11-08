// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Exec approvals through the default coding factory ask PermissionRequest hooks as Codex's
//! `request_approval` does: the retry outside a refusing sandbox included, under `:retry`, and
//! with the model's justification as `description` whenever it was given.
use super::*;

const ALLOW: &str = r#"printf '%s' '{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow"}}}'"#;
const DENY: &str = r#"printf '%s' '{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"deny","message":"blocked by hook"}}}'"#;
const WRITES: &str = "touch denied.txt && printf 'wrote\\n'";

/// Sandbox prompts allowed, so a refused first attempt asks to retry outside the sandbox.
fn write_granular(home: &HooksHome, hooks: &str) {
    std::fs::write(
        home.home.path().join("config.toml"),
        format!(
            "approval_policy = {{ granular = {{ sandbox_approval = true, rules = true, mcp_elicitations = true }} }}\nsandbox_mode = \"read-only\"\n\n{hooks}\n"
        ),
    )
    .unwrap();
}

fn permission_hook(log: &Path, decision: Option<&str>) -> String {
    let command = decision.map_or_else(
        || logging(log),
        |decision| format!("{}; {decision}", logging(log)),
    );
    hook_section("PermissionRequest", &command)
}

fn exec(id: &str, command: &str, extra: Value) -> RunItem {
    let mut arguments = json!({"cmd":command,"justification":null,"prefix_rule":null,"sandbox_permissions":null,"workdir":null,"shell":"/bin/sh","tty":false,"login":false,"yield_time_ms":1000,"timeout_ms":null});
    for (key, value) in extra.as_object().unwrap() {
        arguments[key] = value.clone();
    }
    RunItem::new(
        ItemId::new(id),
        RunItemKind::ToolCall(ToolCall::new(CallId::new(id), "exec_command", arguments)),
    )
}

fn model_output(script: &Script, call: &str) -> Value {
    script
        .last_request()
        .input()
        .iter()
        .find_map(|item| match item {
            ModelInputItem::ToolCallOutput(output) if output.call_id().as_str() == call => {
                Some(output.output().clone())
            }
            _ => None,
        })
        .expect("the model received the call's output")
}

fn permission_run_ids(messages: &[Value]) -> Vec<String> {
    messages
        .iter()
        .filter(|message| {
            message["method"] == "hook/started"
                && message["params"]["run"]["eventName"] == "permissionRequest"
        })
        .map(|message| message["params"]["run"]["id"].as_str().unwrap().to_owned())
        .collect()
}

async fn answer(server: &Server, connection: &Connection, request: &Value, decision: &str) {
    server
        .send(
            connection,
            ra_protocol::frame::JsonRpcResponse::new(
                serde_json::from_value::<ra_protocol::frame::RequestId>(request["id"].clone())
                    .unwrap(),
                json!({"decision": decision}),
            ),
        )
        .await;
}

/// A granted retry runs outside the sandbox without asking the client, under the retry run id.
#[tokio::test]
async fn a_permission_hook_grants_the_sandbox_retry_without_asking_the_client() {
    let home = HooksHome::new();
    let (permission, post) = (home.path("permission.jsonl"), home.path("post.jsonl"));
    write_granular(
        &home,
        &format!(
            "{}\n{}",
            permission_hook(&permission, Some(ALLOW)),
            hook_section("PostToolUse", &logging(&post))
        ),
    );
    let server = home.server();
    let mut connection = server.connect(1).await;
    let thread = start_thread(&server, &mut connection).await;
    home.script.push(Step::Respond(ModelResponse::new(vec![exec(
        "denied",
        WRITES,
        json!({}),
    )])));
    home.script.answer("completed");
    server.run_turn(&mut connection, &thread, "write").await;
    let messages = connection.drain().await;

    assert!(
        !messages
            .iter()
            .any(|message| message["method"] == "item/commandExecution/requestApproval"),
        "the client was not asked"
    );
    let permissions = logged(&permission);
    assert_eq!(permissions.len(), 1);
    assert_eq!(permissions[0]["tool_name"], "Bash");
    assert_eq!(permissions[0]["tool_input"], json!({"command": WRITES}));
    assert!(permissions[0].get("tool_use_id").is_none());
    let run_ids = permission_run_ids(&messages);
    assert_eq!(run_ids.len(), 1);
    assert!(run_ids[0].ends_with(":denied:retry"), "{run_ids:?}");
    let posts = logged(&post);
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0]["tool_use_id"], "denied");
    assert_eq!(posts[0]["tool_response"], "wrote\n");
    assert!(home.workspace.path().join("denied.txt").exists());
}

/// A refused retry does not run again and does not ask the client; the model reads the hook's
/// message.
#[tokio::test]
async fn a_permission_hook_refuses_the_sandbox_retry_with_its_message() {
    let home = HooksHome::new();
    let (permission, post) = (home.path("permission.jsonl"), home.path("post.jsonl"));
    write_granular(
        &home,
        &format!(
            "{}\n{}",
            permission_hook(&permission, Some(DENY)),
            hook_section("PostToolUse", &logging(&post))
        ),
    );
    let server = home.server();
    let mut connection = server.connect(1).await;
    let thread = start_thread(&server, &mut connection).await;
    home.script.push(Step::Respond(ModelResponse::new(vec![exec(
        "denied",
        WRITES,
        json!({}),
    )])));
    home.script.answer("completed");
    server.run_turn(&mut connection, &thread, "write").await;
    let messages = connection.drain().await;

    assert!(
        !messages
            .iter()
            .any(|message| message["method"] == "item/commandExecution/requestApproval")
    );
    assert_eq!(logged(&permission).len(), 1);
    assert!(logged(&post).is_empty());
    assert!(!home.workspace.path().join("denied.txt").exists());
    assert_eq!(
        model_output(&home.script, "denied"),
        serde_json::to_value(ra_core::tool::ToolOutput::text("blocked by hook")).unwrap()
    );
}

/// Without a decision the client is asked as before, the hook ran once, and an approval runs the
/// retry once.
#[tokio::test]
async fn an_undecided_permission_hook_leaves_the_sandbox_retry_to_the_client() {
    let home = HooksHome::new();
    let (permission, post) = (home.path("permission.jsonl"), home.path("post.jsonl"));
    write_granular(
        &home,
        &format!(
            "{}\n{}",
            permission_hook(&permission, None),
            hook_section("PostToolUse", &logging(&post))
        ),
    );
    let server = home.server();
    let mut connection = server.connect(1).await;
    let thread = start_thread(&server, &mut connection).await;
    home.script.push(Step::Respond(ModelResponse::new(vec![exec(
        "denied",
        WRITES,
        json!({}),
    )])));
    home.script.answer("completed");
    server.turn_start(&mut connection, &thread, "write").await;
    let request = connection
        .until(|message| {
            message["method"] == "item/commandExecution/requestApproval"
                && message.get("id").is_some()
        })
        .await;
    assert_eq!(
        request["params"]["reason"],
        "command failed; retry without sandbox?"
    );
    assert_eq!(logged(&permission).len(), 1);
    answer(&server, &connection, &request, "accept").await;
    connection.notification("turn/completed").await;

    assert_eq!(logged(&permission).len(), 1);
    let posts = logged(&post);
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0]["tool_response"], "wrote\n");
}

/// The justification reaches the hook as `description` whenever it was given, an empty one
/// included, as Codex's `PermissionRequestPayload::bash` does; an omitted or `null` one does not.
#[tokio::test]
async fn exec_permission_requests_carry_the_justification_as_description() {
    for (justification, expected) in [
        (None, None),
        (Some(Value::Null), None),
        (Some(json!("")), Some("")),
        (
            Some(json!("needs write access")),
            Some("needs write access"),
        ),
    ] {
        let home = HooksHome::new();
        let permission = home.path("permission.jsonl");
        home.write_hooks_with_approval("on-request", &permission_hook(&permission, Some(ALLOW)));
        let server = home.server();
        let mut connection = server.connect(1).await;
        let thread = start_thread(&server, &mut connection).await;
        let mut arguments = json!({"cmd":"printf 'ok\\n'","prefix_rule":null,"sandbox_permissions":"require_escalated","workdir":null,"shell":"/bin/sh","tty":false,"login":false,"yield_time_ms":1000,"timeout_ms":null});
        if let Some(justification) = justification {
            arguments["justification"] = justification;
        }
        home.script
            .push(Step::Respond(ModelResponse::new(vec![RunItem::new(
                ItemId::new("escalated"),
                RunItemKind::ToolCall(ToolCall::new(
                    CallId::new("escalated"),
                    "exec_command",
                    arguments,
                )),
            )])));
        home.script.answer("completed");
        server.run_turn(&mut connection, &thread, "run").await;

        let permissions = logged(&permission);
        assert_eq!(
            permissions.len(),
            1,
            "{expected:?}: {}",
            model_output(&home.script, "escalated")
        );
        let mut input = json!({"command": "printf 'ok\\n'"});
        if let Some(description) = expected {
            input["description"] = json!(description);
        }
        assert_eq!(permissions[0]["tool_input"], input);
    }
}

/// A hook grant is Codex's `Approved` from a hook: the same retry in a later turn is put to the
/// hook again rather than run from a session approval.
#[tokio::test]
async fn a_hook_granted_retry_is_not_remembered_for_the_session() {
    let home = HooksHome::new();
    let permission = home.path("permission.jsonl");
    write_granular(&home, &permission_hook(&permission, Some(ALLOW)));
    let server = home.server();
    let mut connection = server.connect(1).await;
    let thread = start_thread(&server, &mut connection).await;
    for turn in ["first", "second"] {
        home.script.push(Step::Respond(ModelResponse::new(vec![exec(
            turn,
            WRITES,
            json!({}),
        )])));
        home.script.answer("completed");
        server.run_turn(&mut connection, &thread, turn).await;
    }
    let messages = connection.drain().await;

    assert!(
        !messages
            .iter()
            .any(|message| message["method"] == "item/commandExecution/requestApproval")
    );
    assert_eq!(logged(&permission).len(), 2);
    assert_eq!(
        permission_run_ids(&messages)
            .iter()
            .filter(|id| id.ends_with(":retry"))
            .count(),
        2
    );
}

/// An escalation a hook approves runs at once outside the sandbox, asked about once and before
/// the call, with no retry put to the hook or the client. The approval's prepared state reaching
/// the call is covered by the runtime's own tests; this case pins the product behaviour around it.
#[tokio::test]
async fn an_escalation_a_hook_approves_runs_as_approved() {
    let home = HooksHome::new();
    let permission = home.path("permission.jsonl");
    write_granular(&home, &permission_hook(&permission, Some(ALLOW)));
    let server = home.server();
    let mut connection = server.connect(1).await;
    let thread = start_thread(&server, &mut connection).await;
    home.script.push(Step::Respond(ModelResponse::new(vec![exec(
        "escalated",
        WRITES,
        json!({"sandbox_permissions": "require_escalated", "justification": "needs to write"}),
    )])));
    home.script.answer("completed");
    server.run_turn(&mut connection, &thread, "write").await;
    let messages = connection.drain().await;

    assert!(
        !messages
            .iter()
            .any(|message| message["method"] == "item/commandExecution/requestApproval"),
        "the client was not asked"
    );
    let run_ids = permission_run_ids(&messages);
    assert_eq!(run_ids.len(), 1, "asked once, before the call: {run_ids:?}");
    assert!(!run_ids[0].ends_with(":retry"), "{run_ids:?}");
    assert!(
        home.workspace.path().join("denied.txt").exists(),
        "{}",
        model_output(&home.script, "escalated")
    );
}
