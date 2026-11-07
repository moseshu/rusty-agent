// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Exact exec hook payloads through the default coding factory and app-server.
use super::*;
use ra_core::sandbox::token_truncation::{TruncationPolicy, formatted_truncate_text};

fn exec(id: &str, command: &str, wait: u64, escalated: bool) -> RunItem {
    RunItem::new(
        ItemId::new(id),
        RunItemKind::ToolCall(ToolCall::new(
            CallId::new(id),
            "exec_command",
            json!({"cmd":command,"justification":escalated.then_some("test approval"),"prefix_rule":null,"sandbox_permissions":escalated.then_some("require_escalated"),"workdir":null,"shell":"/bin/sh","tty":false,"login":false,"yield_time_ms":wait,"timeout_ms":null}),
        )),
    )
}
fn stdin(id: &str, session: &str, chars: &str, wait: u64) -> RunItem {
    RunItem::new(
        ItemId::new(id),
        RunItemKind::ToolCall(ToolCall::new(
            CallId::new(id),
            "write_stdin",
            json!({"session_id":session,"chars":chars,"until":"done","match_text":null,"control":null,"yield_time_ms":wait}),
        )),
    )
}
async fn turn(
    home: &HooksHome,
    server: &Server,
    connection: &mut Connection,
    thread: &str,
    items: Vec<RunItem>,
) {
    home.script.push(Step::Respond(ModelResponse::new(items)));
    home.script.answer("completed");
    server.run_turn(connection, thread, "execute").await;
}
fn session_for(script: &Script, call: &str) -> String {
    let request = script.last_request();
    let text = request
        .input()
        .iter()
        .find_map(|item| match item {
            ModelInputItem::ToolCallOutput(output) if output.call_id().as_str() == call => output
                .output()["blocks"][0]["text"]
                .as_str()
                .map(str::to_owned),
            _ => None,
        })
        .expect("model received the session output");
    text.split("Session `")
        .nth(1)
        .unwrap_or_else(|| panic!("no session: {text}"))
        .split('`')
        .next()
        .unwrap()
        .to_owned()
}
fn assert_completion(payload: &Value, call: &str, command: &str, response: &str) {
    assert_eq!(payload["tool_name"], "Bash");
    assert_eq!(payload["tool_use_id"], call);
    assert_eq!(payload["tool_input"], json!({"command":command}));
    assert_eq!(payload["tool_response"], response);
}

#[tokio::test]
async fn one_shot_preserves_raw_newlines_empty_output_and_both_streams() {
    for (command, expected) in [
        ("printf 'finished\\n'", Some("finished\n")),
        ("true", Some("")),
        ("printf 'alpha\\n'; printf 'beta\\n' >&2", None),
    ] {
        let home = HooksHome::new();
        let log = home.path("post.jsonl");
        home.write_hooks(&hook_section("PostToolUse", &logging(&log)));
        let server = home.server();
        let mut connection = server.connect(1).await;
        let thread = start_thread(&server, &mut connection).await;
        turn(
            &home,
            &server,
            &mut connection,
            &thread,
            vec![exec("original", command, 1000, false)],
        )
        .await;
        let payloads = logged(&log);
        assert_eq!(payloads.len(), 1);
        if let Some(expected) = expected {
            assert_completion(&payloads[0], "original", command, expected);
        } else {
            let response = payloads[0]["tool_response"].as_str().unwrap();
            assert!(response.contains("alpha\n") && response.contains("beta\n"));
            assert!(!response.contains("--- stderr ---"));
            assert_eq!(payloads[0]["tool_use_id"], "original");
        }
    }
}

#[tokio::test]
async fn running_polls_skip_hooks_and_completion_is_owned_once_by_original_exec() {
    let home = HooksHome::new();
    let post = home.path("post.jsonl");
    let pre = home.path("pre.jsonl");
    let wrong = home.path("wrong.jsonl");
    let deny = home.path("deny-pre");
    let bash = home.path("bash.jsonl");
    home.write_hooks(&format!("{}\n{}\n[[hooks.PreToolUse]]\nmatcher = '^write_stdin$'\n[[hooks.PreToolUse.hooks]]\ntype = 'command'\ncommand = '''{}; exit 2'''\n[[hooks.PostToolUse]]\nmatcher = '^write_stdin$'\n[[hooks.PostToolUse.hooks]]\ntype = 'command'\ncommand = '''{}'''\n[[hooks.PostToolUse]]\nmatcher = '^Bash$'\n[[hooks.PostToolUse.hooks]]\ntype = 'command'\ncommand = '''{}'''\n", hook_section("PostToolUse", &logging(&post)), hook_section("PreToolUse", &format!("if [ -f '{}' ]; then {}; exit 2; fi", deny.display(), logging(&pre))), logging(&wrong), logging(&wrong), logging(&bash)));
    let server = home.server();
    let mut connection = server.connect(1).await;
    let thread = start_thread(&server, &mut connection).await;
    let command = "printf 'initial\\n'; read l; printf 'finished\\n'";
    turn(
        &home,
        &server,
        &mut connection,
        &thread,
        vec![exec("original", command, 50, false)],
    )
    .await;
    let session = session_for(&home.script, "original");
    assert!(logged(&post).is_empty());
    turn(
        &home,
        &server,
        &mut connection,
        &thread,
        vec![stdin("running", &session, "", 10)],
    )
    .await;
    assert!(logged(&post).is_empty());
    // A catch-all denial after launch must not stop write_stdin.
    std::fs::write(&deny, "deny polls if the pre hook runs").unwrap();
    turn(
        &home,
        &server,
        &mut connection,
        &thread,
        vec![stdin("complete", &session, "q\n", 1000)],
    )
    .await;
    let payloads = logged(&post);
    assert_eq!(payloads.len(), 1);
    assert_completion(&payloads[0], "original", command, "finished\n");
    assert!(logged(&pre).is_empty());
    assert!(logged(&wrong).is_empty());
    assert_eq!(logged(&bash).len(), 1);
    turn(
        &home,
        &server,
        &mut connection,
        &thread,
        vec![stdin("repeat", &session, "", 1000)],
    )
    .await;
    assert_eq!(logged(&post).len(), 1);
}

#[tokio::test]
async fn parallel_sessions_complete_in_reverse_order_with_separate_metadata() {
    let home = HooksHome::new();
    let log = home.path("post.jsonl");
    home.write_hooks(&hook_section("PostToolUse", &logging(&log)));
    let server = home.server();
    let mut connection = server.connect(1).await;
    let thread = start_thread(&server, &mut connection).await;
    let a = "read l; printf 'first\\n'";
    let b = "read l; printf 'second\\n'";
    turn(
        &home,
        &server,
        &mut connection,
        &thread,
        vec![exec("first", a, 10, false), exec("second", b, 10, false)],
    )
    .await;
    let first = session_for(&home.script, "first");
    let second = session_for(&home.script, "second");
    assert!(logged(&log).is_empty());
    turn(
        &home,
        &server,
        &mut connection,
        &thread,
        vec![stdin("second-poll", &second, "q\n", 1000)],
    )
    .await;
    turn(
        &home,
        &server,
        &mut connection,
        &thread,
        vec![stdin("first-poll", &first, "q\n", 1000)],
    )
    .await;
    let payloads = logged(&log);
    assert_eq!(payloads.len(), 2);
    assert_completion(&payloads[0], "second", b, "second\n");
    assert_completion(&payloads[1], "first", a, "first\n");
}

#[tokio::test]
async fn concurrent_polls_on_one_session_emit_one_completion() {
    let home = HooksHome::new();
    let log = home.path("post.jsonl");
    home.write_hooks(&hook_section("PostToolUse", &logging(&log)));
    let server = home.server();
    let mut connection = server.connect(1).await;
    let thread = start_thread(&server, &mut connection).await;
    let command = "read l; printf 'finished\\n'";
    turn(
        &home,
        &server,
        &mut connection,
        &thread,
        vec![exec("original", command, 10, false)],
    )
    .await;
    let session = session_for(&home.script, "original");
    turn(
        &home,
        &server,
        &mut connection,
        &thread,
        vec![
            stdin("writer", &session, "q\n", 1000),
            stdin("poller", &session, "", 1000),
        ],
    )
    .await;
    let payloads = logged(&log);
    assert_eq!(payloads.len(), 1);
    assert_completion(&payloads[0], "original", command, "finished\n");
}

#[tokio::test]
async fn approval_resumed_exec_emits_actual_output_once() {
    let home = HooksHome::new();
    let log = home.path("post.jsonl");
    home.write_hooks_with_approval("on-request", &format!("{}\n{}", hook_section("PostToolUse", &logging(&log)), hook_section("PermissionRequest", r#"printf '%s' '{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow"}}}'"#)));
    let server = home.server();
    let mut connection = server.connect(1).await;
    let thread = start_thread(&server, &mut connection).await;
    let command = "printf 'approved\\n'";
    turn(
        &home,
        &server,
        &mut connection,
        &thread,
        vec![exec("original", command, 1000, true)],
    )
    .await;
    let payloads = logged(&log);
    assert_eq!(payloads.len(), 1);
    assert_completion(&payloads[0], "original", command, "approved\n");
}

#[tokio::test]
async fn approval_resumed_stdin_reports_the_original_command() {
    let home = HooksHome::new();
    let log = home.path("post.jsonl");
    let permissions = home.path("permissions.jsonl");
    home.write_hooks_with_approval("on-request", &format!("{}\n{}", hook_section("PostToolUse", &logging(&log)), hook_section("PermissionRequest", &format!("{}; printf '%s' '{{\"hookSpecificOutput\":{{\"hookEventName\":\"PermissionRequest\",\"decision\":{{\"behavior\":\"allow\"}}}}}}'", logging(&permissions)))));
    let server = home.server();
    let mut connection = server.connect(1).await;
    let thread = start_thread(&server, &mut connection).await;
    let command = "read l; printf 'approved-input\\n'";
    turn(
        &home,
        &server,
        &mut connection,
        &thread,
        vec![exec("original", command, 10, false)],
    )
    .await;
    let session = session_for(&home.script, "original");
    server
        .call(
            &mut connection,
            "thread/settings/update",
            json!({"threadId":thread,"sandboxPolicy":{"type":"dangerFullAccess"}}),
        )
        .await;
    connection.notification("thread/settings/updated").await;
    turn(
        &home,
        &server,
        &mut connection,
        &thread,
        vec![stdin("approved-poll", &session, "q\n", 1000)],
    )
    .await;
    assert_eq!(logged(&permissions).len(), 1, "stdin went through approval");
    let payloads = logged(&log);
    assert_eq!(payloads.len(), 1);
    assert_completion(&payloads[0], "original", command, "approved-input\n");
}

#[tokio::test]
async fn truncation_uses_raw_capture_and_preserves_source_byte_token_count() {
    for (count, omitted) in [(50000, false), (1100000, true)] {
        let home = HooksHome::new();
        let log = home.path("post.jsonl");
        home.write_hooks(&hook_section("PostToolUse", &logging(&log)));
        let server = home.server();
        let mut connection = server.connect(1).await;
        let thread = start_thread(&server, &mut connection).await;
        turn(
            &home,
            &server,
            &mut connection,
            &thread,
            vec![exec(
                "large",
                &format!("head -c {count} /dev/zero | tr '\\000' x"),
                5000,
                false,
            )],
        )
        .await;
        let payloads = logged(&log);
        assert_eq!(payloads.len(), 1);
        let response = payloads[0]["tool_response"].as_str().unwrap();
        if omitted {
            assert!(
                response.starts_with(&format!(
                    "Warning: truncated output (original token count: {})\n",
                    count / 4
                )),
                "{}",
                &response[..response.len().min(200)]
            );
            assert!(response.contains(&ra_exec::output::omission_marker(count - 100 * 1024)));
        } else {
            assert_eq!(
                response,
                formatted_truncate_text(&"x".repeat(count), TruncationPolicy::bytes(10_000))
            );
        }
    }
}

#[tokio::test]
async fn equal_byte_budgets_select_the_models_byte_policy() {
    let home = HooksHome::new();
    let log = home.path("post.jsonl");
    let catalog = home.path("catalog.json");
    std::fs::write(&catalog, json!({"models":[{
        "slug":"bytes-model","display_name":"Byte model","description":null,"supported_reasoning_levels":[],
        "shell_type":"unified_exec","visibility":"list","supported_in_api":true,"priority":0,
        "availability_nux":null,"upgrade":null,"support_verbosity":false,"default_verbosity":null,
        "apply_patch_tool_type":null,"truncation_policy":{"mode":"bytes","limit":100},
        "experimental_supported_tools":[],"base_instructions":"Test model instructions"
    }]}).to_string()).unwrap();
    home.write_hooks(&format!(
        "model = 'bytes-model'\nmodel_catalog_json = {}\ntool_output_token_limit = 10000\n{}",
        ra_config::TomlValue::String(catalog.to_string_lossy().into_owned()),
        hook_section("PostToolUse", &logging(&log))
    ));
    let server = home.server();
    let mut connection = server.connect(1).await;
    let thread = start_thread(&server, &mut connection).await;
    turn(
        &home,
        &server,
        &mut connection,
        &thread,
        vec![exec(
            "large",
            "head -c 50000 /dev/zero | tr '\\000' x",
            5000,
            false,
        )],
    )
    .await;
    let payloads = logged(&log);
    assert_eq!(payloads.len(), 1);
    let source = "x".repeat(50000);
    let expected = formatted_truncate_text(&source, TruncationPolicy::bytes(40000));
    assert_ne!(
        expected,
        formatted_truncate_text(&source, TruncationPolicy::tokens(10000))
    );
    assert_eq!(payloads[0]["tool_response"], expected);
}
