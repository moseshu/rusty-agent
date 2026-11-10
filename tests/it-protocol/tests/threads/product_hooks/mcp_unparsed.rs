//! An MCP tool call whose argument text is not JSON, through the default coding factory with a
//! product MCP server: PreToolUse receives the text as a string, as Codex's MCP handler gives it,
//! and the server is reached only by input a hook rewrote into an object.
use super::super::unparsed_arguments::{asked_nothing, model_answer, unparsed_call};
use super::*;

/// One MCP call whose argument text is not JSON, with PreToolUse logging and optionally
/// rewriting the input: the hook inputs, the arguments the server received, and the model's answer.
async fn mcp_unparsed(rewrite: bool) -> (Vec<Value>, Vec<Value>, (String, bool)) {
    let home = HooksHome::new();
    let script = home.path("unparsed_server.py");
    let hooks = home.path("unparsed_hooks.jsonl");
    let calls = home.path("unparsed_calls.jsonl");
    std::fs::write(&script, r#"
import json, sys
for line in sys.stdin:
    message = json.loads(line)
    if 'id' not in message:
        continue
    method = message['method']
    if method == 'initialize':
        result = {'protocolVersion':'2024-11-05','capabilities':{'tools':{}},'serverInfo':{'name':'echo','version':'1'}}
    elif method == 'tools/list':
        result = {'tools':[{'name':'echo','inputSchema':{'type':'object','properties':{}},'annotations':{'readOnlyHint':True,'destructiveHint':False,'openWorldHint':False}}]}
    elif method == 'tools/call':
        with open(sys.argv[1], 'a') as calls:
            calls.write(json.dumps(message['params'].get('arguments')) + '\n')
        result = {'content':[{'type':'text','text':'echoed'}]}
    else:
        result = {}
    print(json.dumps({'jsonrpc':'2.0','id':message['id'],'result':result}), flush=True)
"#).unwrap();
    let pre = if rewrite {
        format!(
            "{}; printf '%s' '{{\"hookSpecificOutput\":{{\"hookEventName\":\"PreToolUse\",\"permissionDecision\":\"allow\",\"updatedInput\":{{\"message\":\"rewritten\"}}}}}}'",
            logging(&hooks)
        )
    } else {
        logging(&hooks)
    };
    home.write_hooks_with_approval(
        "on-request",
        &format!(
            r#"
[features]
codex_hooks = true
[mcp_servers.echo]
command = "python3"
default_tools_approval_mode = "approve"
args = [{}, {}]
{}
{}
"#,
            ra_config::TomlValue::String(script.to_string_lossy().into_owned()),
            ra_config::TomlValue::String(calls.to_string_lossy().into_owned()),
            hook_section("PreToolUse", &pre),
            hook_section("PostToolUse", &logging(&hooks))
        ),
    );
    let config = home.config_builder().build().await.unwrap();
    let mut catalog = ra_mcp::ResolvedMcpCatalog::builder();
    catalog.register(ra_mcp::McpServerRegistration::from_config(
        "echo".into(),
        config.mcp_servers.get()["echo"].clone(),
    ));
    let plan = ra_coding::mcp::product_mcp_servers(
        &catalog.build(),
        home.workspace.path(),
        &config.http_client_factory(),
    );
    assert!(plan.unavailable.is_empty());
    let mcp = CodingMcpRuntime::new(plan.servers, McpDiscoveryOptions::new()).unwrap();
    mcp.reload_mcp_servers().await.unwrap();
    let fixture = ExecutorFixture {
        home,
        mcp,
        calls: Arc::new(Mutex::new(Vec::new())),
    };
    let server = fixture.server(None);
    let mut connection = server.connect(1).await;
    let thread = start_thread(&server, &mut connection).await;
    fixture
        .home
        .script
        .push(Step::Respond(ModelResponse::new(vec![unparsed_call(
            "mcp-call", "echo", "{bad",
        )])));
    fixture.home.script.answer("done");
    server.run_turn(&mut connection, &thread, "call MCP").await;
    let messages = connection.drain().await;
    assert!(asked_nothing(&messages), "{messages:#?}");
    let answer = model_answer(&fixture.home.script, "mcp-call");
    let received = logged(&calls);
    let inputs = logged(&hooks);
    fixture.mcp.shutdown().await.unwrap();
    (inputs, received, answer)
}

/// An MCP tool's PreToolUse receives the argument text as a string, as Codex's MCP handler gives
/// it; the server is never called, PostToolUse never runs and the model is told the call failed.
/// A hook that rewrites the input into an object sends the server only the rewritten arguments.
#[tokio::test]
async fn an_unparsed_mcp_call_reaches_the_server_only_as_rewritten_by_pre_tool_use() {
    let (inputs, received, (answer, is_error)) = mcp_unparsed(false).await;
    assert_eq!(events_of(&inputs), ["PreToolUse"]);
    assert_eq!(inputs[0]["tool_name"], "mcp__echo__echo");
    assert_eq!(inputs[0]["tool_input"], json!("{bad"));
    assert!(received.is_empty(), "{received:?}");
    assert!(is_error, "{answer}");

    let (inputs, received, (answer, is_error)) = mcp_unparsed(true).await;
    assert_eq!(events_of(&inputs), ["PreToolUse", "PostToolUse"]);
    assert_eq!(inputs[0]["tool_input"], json!("{bad"));
    assert_eq!(inputs[1]["tool_input"], json!({"message": "rewritten"}));
    assert_eq!(received, [json!({"message": "rewritten"})]);
    assert!(!is_error, "{answer}");
    assert!(answer.contains("echoed"), "{answer}");
}
