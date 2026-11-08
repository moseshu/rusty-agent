// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Input to a running terminal through the default coding factory asks PermissionRequest hooks as
//! Codex's `request_approval` does for `ApprovalAction::WriteStdin`: the hook is told what the
//! review prepared — the terminal, the input, the launching call, the input call, the
//! environment, the launch directory and the authority the terminal retains — under the input
//! call's own run id, and its answer decides without asking the client.
use super::*;

const ALLOW: &str = r#"printf '%s' '{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow"}}}'"#;
const DENY: &str = r#"printf '%s' '{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"deny","message":"blocked by hook"}}}'"#;

fn call(id: &str, name: &str, arguments: Value) -> RunItem {
    RunItem::new(
        ItemId::new(id),
        RunItemKind::ToolCall(ToolCall::new(CallId::new(id), name, arguments)),
    )
}

fn model_text(script: &Script, call: &str) -> String {
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
        .map(
            |output| match ra_core::tool::ToolOutput::from_stored(&output) {
                Ok(Some(stored)) => stored.as_text().unwrap_or_default().to_owned(),
                _ => output.to_string(),
            },
        )
        .expect("the model received the call's output")
}

/// A terminal launched under the read-only sandbox, whose thread then dropped its sandbox: input
/// to it asks, as the terminal retains settings that differ from the current ones.
struct DriftedTerminal {
    home: HooksHome,
    server: Server,
    connection: Connection,
    thread: String,
    session: String,
    permission: PathBuf,
}

impl DriftedTerminal {
    async fn open(decision: Option<&str>) -> Self {
        let home = HooksHome::new();
        let permission = home.path("permission.jsonl");
        let command = decision.map_or_else(
            || logging(&permission),
            |decision| format!("{}; {decision}", logging(&permission)),
        );
        home.write_hooks_with_approval("on-request", &hook_section("PermissionRequest", &command));
        let server = home.server();
        let mut connection = server.connect(1).await;
        let thread = start_thread(&server, &mut connection).await;
        home.script.push(Step::Respond(ModelResponse::new(vec![call(
            "launch",
            "exec_command",
            json!({"cmd": "read line; printf 'got %s\\n' \"$line\"", "yield_time_ms": 10}),
        )])));
        home.script.answer("launched");
        server.run_turn(&mut connection, &thread, "launch").await;
        let text = model_text(&home.script, "launch");
        let session = text
            .split("Session `")
            .nth(1)
            .and_then(|rest| rest.split('`').next())
            .unwrap_or_else(|| panic!("the terminal keeps running: {text}"))
            .to_owned();
        server
            .call(
                &mut connection,
                "thread/settings/update",
                json!({"threadId": thread, "sandboxPolicy": {"type": "dangerFullAccess"}}),
            )
            .await;
        connection.notification("thread/settings/updated").await;
        Self {
            home,
            server,
            connection,
            thread,
            session,
            permission,
        }
    }

    fn input(&self) -> Value {
        json!({"session_id": self.session, "chars": "q\n", "until": "done", "match_text": null,
               "control": null, "yield_time_ms": 2000})
    }

    /// What Codex's `permission_request_payload` gives for this input.
    fn expected_tool_input(&self) -> Value {
        let workspace = self.home.workspace.path().canonicalize().unwrap();
        let cwd = ra_paths::PathUri::from_abs_path(
            &ra_paths::AbsolutePathBuf::from_absolute_path_checked(&workspace).unwrap(),
        );
        json!({
            "session_id": self.session,
            "chars": "q\n",
            "parent_call_id": "launch",
            "approval_id": "input",
            "environment_id": "local",
            "cwd": cwd.to_string(),
            "tty": false,
            "sandbox_permissions": "require_escalated",
            "additional_permissions": null,
        })
    }

    fn queue_input(&self) {
        self.home
            .script
            .push(Step::Respond(ModelResponse::new(vec![call(
                "input",
                "write_stdin",
                self.input(),
            )])));
        self.home.script.answer("written");
    }
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

fn asked_the_client(messages: &[Value]) -> bool {
    messages
        .iter()
        .any(|message| message["method"] == "item/commandExecution/requestApproval")
}

/// The hook is told what the review prepared, in Codex's shape and nothing more, under the input
/// call's run id; its approval writes the input without asking the client.
#[tokio::test]
#[cfg_attr(
    not(target_os = "macos"),
    ignore = "the legacy command sandbox refuses the default workspace profile"
)]
async fn a_permission_hook_is_told_what_the_input_review_prepared() {
    let mut terminal = DriftedTerminal::open(Some(ALLOW)).await;
    terminal.connection.drain().await;
    terminal.queue_input();
    let thread = terminal.thread.clone();
    terminal
        .server
        .run_turn(&mut terminal.connection, &thread, "input")
        .await;
    let messages = terminal.connection.drain().await;

    let permissions = logged(&terminal.permission);
    assert_eq!(permissions.len(), 1);
    assert_eq!(permissions[0]["tool_name"], "write_stdin");
    assert_eq!(permissions[0]["tool_input"], terminal.expected_tool_input());
    let run_ids = permission_run_ids(&messages);
    assert_eq!(run_ids.len(), 1);
    assert!(run_ids[0].ends_with(":input"), "{run_ids:?}");
    assert!(!asked_the_client(&messages));
    assert!(
        model_text(&terminal.home.script, "input").contains("got q"),
        "{}",
        model_text(&terminal.home.script, "input")
    );
}

/// A hook that refuses keeps the input from the terminal and gives the model its message, without
/// asking the client.
#[tokio::test]
#[cfg_attr(
    not(target_os = "macos"),
    ignore = "the legacy command sandbox refuses the default workspace profile"
)]
async fn a_permission_hook_refuses_input_with_its_message() {
    let mut terminal = DriftedTerminal::open(Some(DENY)).await;
    terminal.connection.drain().await;
    terminal.queue_input();
    let thread = terminal.thread.clone();
    terminal
        .server
        .run_turn(&mut terminal.connection, &thread, "input")
        .await;
    let messages = terminal.connection.drain().await;

    assert_eq!(logged(&terminal.permission).len(), 1);
    assert!(!asked_the_client(&messages));
    assert_eq!(
        model_text(&terminal.home.script, "input"),
        "blocked by hook"
    );
}

/// Without a decision the client is asked as before, about the same input the hook was told of,
/// and its approval writes it.
#[tokio::test]
#[cfg_attr(
    not(target_os = "macos"),
    ignore = "the legacy command sandbox refuses the default workspace profile"
)]
async fn an_undecided_hook_leaves_the_input_to_the_client() {
    let mut terminal = DriftedTerminal::open(None).await;
    terminal.connection.drain().await;
    terminal.queue_input();
    let thread = terminal.thread.clone();
    terminal
        .server
        .turn_start(&mut terminal.connection, &thread, "input")
        .await;
    let request = terminal
        .connection
        .until(|message| {
            message["method"] == "item/commandExecution/requestApproval"
                && message.get("id").is_some()
        })
        .await;
    assert_eq!(request["params"]["kind"], "writeStdin");
    assert_eq!(request["params"]["itemId"], "launch");
    assert_eq!(request["params"]["approvalId"], "input");
    let permissions = logged(&terminal.permission);
    assert_eq!(permissions.len(), 1);
    assert_eq!(permissions[0]["tool_input"], terminal.expected_tool_input());
    terminal
        .server
        .send(
            &terminal.connection,
            ra_protocol::frame::JsonRpcResponse::new(
                serde_json::from_value::<ra_protocol::frame::RequestId>(request["id"].clone())
                    .unwrap(),
                json!({"decision": "accept"}),
            ),
        )
        .await;
    terminal.connection.notification("turn/completed").await;
    assert!(model_text(&terminal.home.script, "input").contains("got q"));
}
