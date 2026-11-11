// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Exact hook payloads through the default coding factory, against Codex's handler payload tests:
//! `exec_command_pre_tool_use_payload_uses_raw_command` (unified exec),
//! `function_tools_expose_default_hook_payloads_and_rewrites` and
//! `function_hook_input_defaults_empty_arguments_to_object` (registry).
use super::*;

fn call(id: &str, name: &str, arguments: Value) -> RunItem {
    RunItem::new(
        ItemId::new(id),
        RunItemKind::ToolCall(ToolCall::new(CallId::new(id), name, arguments)),
    )
}

fn matched(event: &str, matcher: &str, command: &str) -> String {
    format!(
        "[[hooks.{event}]]\nmatcher = '{matcher}'\n[[hooks.{event}.hooks]]\ntype = \"command\"\ncommand = '''{command}'''\n"
    )
}

async fn run(home: &HooksHome, items: Vec<RunItem>) {
    let server = home.server();
    let mut connection = server.connect(1).await;
    let thread = start_thread(&server, &mut connection).await;
    home.script.push(Step::Respond(ModelResponse::new(items)));
    home.script.answer("done");
    server.run_turn(&mut connection, &thread, "run").await;
}

/// The text the model read for `call`.
fn model_text(script: &Script, call: &str) -> String {
    script
        .last_request()
        .input()
        .iter()
        .find_map(|item| match item {
            ModelInputItem::ToolCallOutput(output) if output.call_id().as_str() == call => {
                output.output().as_str().map(str::to_owned).or_else(|| {
                    output.output()["blocks"][0]["text"]
                        .as_str()
                        .map(str::to_owned)
                })
            }
            _ => None,
        })
        .expect("the model read the call's output")
}

/// `exec_command`'s PreToolUse is Codex's `Bash` with the raw command as `command` and nothing
/// else of the call's arguments.
#[tokio::test]
async fn exec_pre_tool_use_reports_the_raw_command_as_bash() {
    let home = HooksHome::new();
    let pre = home.path("pre.jsonl");
    home.write_hooks(&matched("PreToolUse", "^Bash$", &logging(&pre)));
    run(
        &home,
        vec![call(
            "exec",
            "exec_command",
            json!({"cmd": "printf 'exec command'", "workdir": null, "yield_time_ms": 1000}),
        )],
    )
    .await;
    let payloads = logged(&pre);
    assert_eq!(payloads.len(), 1);
    assert_eq!(payloads[0]["tool_name"], "Bash");
    assert_eq!(
        payloads[0]["tool_input"],
        json!({"command": "printf 'exec command'"})
    );
}

/// A function tool's PreToolUse and PostToolUse carry its own name, the call's arguments and its
/// text output; a PreToolUse rewrite is what the tool runs with and what PostToolUse reports.
#[tokio::test]
async fn function_tools_expose_default_payloads_and_apply_rewrites() {
    let home = HooksHome::new();
    std::fs::write(home.workspace.path().join("a.txt"), "alpha\n").unwrap();
    std::fs::write(home.workspace.path().join("b.txt"), "beta\n").unwrap();
    let (pre, post) = (home.path("pre.jsonl"), home.path("post.jsonl"));
    let rewrite = r#"printf '%s' '{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"allow","updatedInput":{"path":"b.txt"}}}'"#;
    home.write_hooks(&format!(
        "{}\n{}",
        matched(
            "PreToolUse",
            "^read_file$",
            &format!("{}; {rewrite}", logging(&pre))
        ),
        matched("PostToolUse", "^read_file$", &logging(&post))
    ));
    run(
        &home,
        vec![call("read", "read_file", json!({"path": "a.txt"}))],
    )
    .await;

    let text = model_text(&home.script, "read");
    assert!(text.contains("beta") && !text.contains("alpha"), "{text}");
    let pre = logged(&pre);
    assert_eq!(pre.len(), 1);
    assert_eq!(pre[0]["tool_name"], "read_file");
    assert_eq!(pre[0]["tool_input"], json!({"path": "a.txt"}));
    let post = logged(&post);
    assert_eq!(post.len(), 1);
    assert_eq!(post[0]["tool_name"], "read_file");
    assert_eq!(post[0]["tool_use_id"], "read");
    assert_eq!(post[0]["tool_input"], json!({"path": "b.txt"}));
    assert_eq!(post[0]["tool_response"], json!(text));
}

/// Arguments with no fields reach PreToolUse as `{}`. Blank argument text is read as `{}` by the
/// provider codecs (`it-model` `function_call_arguments`), so this is the payload Codex's
/// `function_hook_input_defaults_empty_arguments_to_object` gives blank text.
#[tokio::test]
async fn empty_function_arguments_reach_pre_tool_use_as_an_empty_object() {
    let home = HooksHome::new();
    let pre = home.path("pre.jsonl");
    home.write_hooks(&matched("PreToolUse", "^glob$", &logging(&pre)));
    run(&home, vec![call("empty", "glob", json!({}))]).await;
    let payloads = logged(&pre);
    assert_eq!(payloads.len(), 1);
    assert_eq!(payloads[0]["tool_name"], "glob");
    assert_eq!(payloads[0]["tool_input"], json!({}));
}

/// A namespaced dynamic tool is hooked as Codex hooks any function tool, under `flat_tool_name`:
/// `crm` and `lookup` give `crmlookup` for PreToolUse and PostToolUse alike, and a matcher
/// written for the dotted name selects neither.
#[tokio::test]
async fn namespaced_dynamic_tools_are_hooked_under_codexs_flat_name() {
    let home = HooksHome::new();
    let (flat, dotted) = (home.path("flat.jsonl"), home.path("dotted.jsonl"));
    home.write_hooks(&format!(
        "{}\n{}\n{}\n{}",
        matched("PreToolUse", "^crmlookup$", &logging(&flat)),
        matched("PostToolUse", "^crmlookup$", &logging(&flat)),
        matched("PreToolUse", "^crm\\.lookup$", &logging(&dotted)),
        matched("PostToolUse", "^crm\\.lookup$", &logging(&dotted))
    ));
    let server = home.server();
    let mut connection = server.connect(1).await;
    let started = server
        .call(
            &mut connection,
            "thread/start",
            json!({"dynamicTools": [{
                "namespace": "crm", "name": "lookup", "description": "Looks a customer up.",
                "inputSchema": {"type": "object", "properties": {"id": {"type": "string"}}}
            }]}),
        )
        .await;
    let thread = started["thread"]["id"].as_str().unwrap().to_owned();
    home.script
        .push(Step::Respond(ModelResponse::new(vec![RunItem::new(
            ItemId::new("lookup"),
            RunItemKind::ToolCall(
                ToolCall::new(CallId::new("lookup"), "lookup", json!({"id": "c-1"}))
                    .with_namespace("crm"),
            ),
        )])));
    home.script.answer("found");
    server.turn_start(&mut connection, &thread, "look up").await;
    let request = connection
        .until(|message| message["method"] == "item/tool/call" && message.get("id").is_some())
        .await;
    server
        .send(
            &connection,
            ra_protocol::frame::JsonRpcResponse::new(
                serde_json::from_value::<ra_protocol::frame::RequestId>(request["id"].clone())
                    .unwrap(),
                json!({"contentItems": [{"type": "inputText", "text": "Ada"}], "success": true}),
            ),
        )
        .await;
    connection.notification("turn/completed").await;

    let payloads = logged(&flat);
    let events: Vec<&Value> = payloads
        .iter()
        .map(|payload| &payload["hook_event_name"])
        .collect();
    assert_eq!(events, [&json!("PreToolUse"), &json!("PostToolUse")]);
    for payload in &payloads {
        assert_eq!(payload["tool_name"], "crmlookup");
        assert_eq!(payload["tool_input"], json!({"id": "c-1"}));
    }
    assert_eq!(payloads[1]["tool_use_id"], "lookup");
    assert!(logged(&dotted).is_empty());
}
