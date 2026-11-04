//! Current turn fields and the local transcript on the default product path.

use super::*;

async fn start_with_model(
    server: &Server,
    connection: &mut Connection,
    thread: &str,
    model: &str,
) -> String {
    let result = server
        .call(
            connection,
            "turn/start",
            json!({
                "threadId": thread, "input": [{"type": "text", "text": "hello"}],
                "model": model, "approvalPolicy": "on-request",
            }),
        )
        .await;
    result["turn"]["id"].as_str().unwrap().to_owned()
}

fn assert_fields(input: &Value, model: &str, permission_mode: &str) {
    assert_eq!(input["model"], model, "{input}");
    assert_eq!(input["permission_mode"], permission_mode, "{input}");
}

#[tokio::test]
async fn turn_overrides_and_first_resumed_turn_reach_all_prompt_hooks() {
    let home = HooksHome::new();
    let log = home.path("fields.jsonl");
    home.write_hooks(
        &["SessionStart", "UserPromptSubmit", "Stop"]
            .map(|event| hook_section(event, &logging(&log)))
            .join("\n"),
    );
    let thread = {
        let server = home.server();
        let mut connection = server.connect(1).await;
        let thread = start_thread(&server, &mut connection).await;
        home.script.answer("first");
        start_with_model(&server, &mut connection, &thread, "turn-model").await;
        connection.notification("turn/completed").await;
        let inputs = logged(&log);
        assert_eq!(
            events_of(&inputs),
            ["SessionStart", "UserPromptSubmit", "Stop"]
        );
        for input in &inputs {
            assert_fields(input, "turn-model", "default");
        }
        super::super::unload(&server, &mut connection, &thread).await;
        thread
    };
    std::fs::write(&log, "").unwrap();
    let server = home.server();
    let mut connection = server.connect(1).await;
    server
        .call(
            &mut connection,
            "thread/resume",
            json!({"threadId": thread}),
        )
        .await;
    home.script.answer("resumed");
    start_with_model(&server, &mut connection, &thread, "resumed-model").await;
    connection.notification("turn/completed").await;
    let inputs = logged(&log);
    assert_eq!(
        events_of(&inputs),
        ["SessionStart", "UserPromptSubmit", "Stop"]
    );
    assert_eq!(inputs[0]["source"], "resume");
    for input in &inputs {
        assert_fields(input, "resumed-model", "default");
    }
}

#[tokio::test]
async fn thread_default_updates_do_not_replace_the_active_turn_policy() {
    for interrupt in [false, true] {
        let home = HooksHome::new();
        let log = home.path("fields.jsonl");
        home.write_hooks(
            &["UserPromptSubmit", "Stop", "Interrupt"]
                .map(|event| hook_section(event, &logging(&log)))
                .join("\n"),
        );
        let server = home.server();
        let mut connection = server.connect(1).await;
        let thread = start_thread(&server, &mut connection).await;
        let gate = Arc::new(Notify::new());
        // A second step captures the policy published while the first model call was running.
        home.script.push(Step::After(
            Arc::clone(&gate),
            command_response("step-command", false),
        ));
        let finish = Arc::new(Notify::new());
        home.script
            .push(Step::After(Arc::clone(&finish), final_message("done")));
        let turn = start_with_model(&server, &mut connection, &thread, "live-model").await;
        home.script.wait_for_calls(1).await;
        server
            .call(
                &mut connection,
                "thread/settings/update",
                json!({
                    "threadId":thread, "approvalPolicy":"never",
                }),
            )
            .await;
        if interrupt {
            interrupt_turn(&server, &mut connection, &thread, &turn).await;
        } else {
            gate.notify_one();
            home.script.wait_for_calls(2).await;
            finish.notify_one();
            connection.notification("turn/completed").await;
        }
        let inputs = logged(&log);
        assert_fields(&inputs[0], "live-model", "default");
        assert_fields(inputs.last().unwrap(), "live-model", "default");
        assert_eq!(
            inputs.last().unwrap()["hook_event_name"],
            if interrupt { "Interrupt" } else { "Stop" }
        );
    }
}

#[tokio::test]
async fn local_transcript_is_materialized_and_visible_to_lifecycle_hooks() {
    let mut home = HooksHome::new();
    home.store = Arc::new(ra_session::RolloutThreadDirectory::new(
        home.path("rollouts"),
    ));
    let log = home.path("transcripts.jsonl");
    let transcript = home.path("last-transcript.jsonl");
    let command = format!(
        r#"{}; hook_transcript_file=$(sed -n 's/.*"transcript_path" *: *"\([^"]*\)".*/\1/p' '{}' | tail -n 1); cat "$hook_transcript_file" > '{}'"#,
        logging(&log),
        log.display(),
        transcript.display()
    );
    home.write_hooks(
        &[
            "SessionStart",
            "UserPromptSubmit",
            "Stop",
            "Interrupt",
            "SessionEnd",
        ]
        .map(|event| hook_section(event, &command))
        .join("\n"),
    );
    let server = home.server();
    let mut connection = server.connect(1).await;
    let thread = start_thread(&server, &mut connection).await;
    home.script.answer("first answer");
    server
        .run_turn(&mut connection, &thread, "first prompt")
        .await;
    let inputs = logged(&log);
    assert_eq!(
        events_of(&inputs),
        ["SessionStart", "UserPromptSubmit", "Stop"]
    );
    let path = PathBuf::from(inputs[0]["transcript_path"].as_str().unwrap());
    assert!(path.exists());
    for input in &inputs {
        assert_eq!(input["transcript_path"], inputs[0]["transcript_path"]);
    }
    let copied = ra_session::RolloutReader::open(&transcript)
        .read_all()
        .await
        .unwrap();
    assert!(
        !copied.is_empty(),
        "the hook reads a materialized transcript"
    );
    assert!(matches!(
        copied[0].payload().unwrap(),
        ra_session::RolloutPayload::SessionMeta(_)
    ));
    home.script
        .push(Step::After(Arc::new(Notify::new()), final_message("never")));
    let turn = server
        .turn_start(&mut connection, &thread, "interrupt this prompt")
        .await;
    home.script.wait_for_calls(2).await;
    interrupt_turn(&server, &mut connection, &thread, &turn).await;
    assert!(
        std::fs::read_to_string(&transcript)
            .unwrap()
            .contains("interrupt this prompt")
    );
    super::super::unload(&server, &mut connection, &thread).await;
    let inputs = logged(&log);
    assert_eq!(inputs.last().unwrap()["hook_event_name"], "SessionEnd");
    assert_eq!(
        inputs.last().unwrap()["transcript_path"],
        inputs[0]["transcript_path"]
    );
    assert!(
        std::fs::read_to_string(&transcript)
            .unwrap()
            .contains("interrupt this prompt")
    );
}

fn command_response(call_id: &str, escalated: bool) -> ModelResponse {
    ModelResponse::new(vec![RunItem::new(
        ItemId::new(call_id),
        RunItemKind::ToolCall(ToolCall::new(
            CallId::new(call_id),
            "exec_command",
            json!({
                "cmd":"echo hook-fields", "justification": "test approval", "prefix_rule":null,
                "sandbox_permissions": if escalated { Some("require_escalated") } else { None },
                "workdir":null, "shell":"/bin/sh", "tty":null, "login":false,
                "yield_time_ms":1000, "timeout_ms":null,
            }),
        )),
    )])
}

#[tokio::test]
async fn tool_and_approval_resume_hooks_use_the_turn_override() {
    let home = HooksHome::new();
    let log = home.path("approval-fields.jsonl");
    home.write_hooks(
        &[
            "SessionStart",
            "UserPromptSubmit",
            "PreToolUse",
            "PermissionRequest",
            "PostToolUse",
            "Stop",
        ]
        .map(|event| hook_section(event, &logging(&log)))
        .join("\n"),
    );
    let server = home.server();
    let mut connection = server.connect(1).await;
    let thread = start_thread(&server, &mut connection).await;
    home.script
        .push(Step::Respond(command_response("approved-command", true)));
    home.script.answer("done");
    let turn = start_with_model(&server, &mut connection, &thread, "approval-model").await;
    let request = connection
        .until(|message| {
            message["method"] == "item/commandExecution/requestApproval"
                && message.get("id").is_some()
        })
        .await;
    server
        .send(
            &connection,
            ra_protocol::frame::JsonRpcResponse::new(
                serde_json::from_value::<ra_protocol::frame::RequestId>(request["id"].clone())
                    .unwrap(),
                json!({"decision":"accept"}),
            ),
        )
        .await;
    connection.notification("turn/completed").await;
    let inputs = logged(&log);
    assert_eq!(
        events_of(&inputs),
        [
            "SessionStart",
            "UserPromptSubmit",
            "PreToolUse",
            "PermissionRequest",
            "PostToolUse",
            "Stop"
        ]
    );
    for input in &inputs {
        assert_fields(input, "approval-model", "default");
    }
    for input in inputs.iter().skip(1) {
        assert_eq!(input["turn_id"], turn);
    }
}
