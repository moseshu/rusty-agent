// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Product MCP approvals ask PermissionRequest hooks before any elicitation, as Codex's
//! `hooks_mcp.rs` cases show: a grant runs the tool without asking the user, a denial reaches the
//! model as the MCP error result Codex builds from the hook's message, and no decision asks the
//! user as before. Hook grants are not remembered for the session.
use super::*;
use ra_mcp::tool::McpToolNaming;

const ALLOW: &str = r#"printf '%s' '{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow"}}}'"#;
const DENY: &str = r#"printf '%s' '{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"deny","message":"mcp blocked by hook"}}}'"#;

struct McpPermissionFixture {
    fixture: ExecutorFixture,
    permission: PathBuf,
    post: PathBuf,
    calls: PathBuf,
}

/// A server with one destructive `echo` tool under `mode`, logging each `tools/call` it receives.
async fn mcp_fixture(mode: &str, decision: Option<&str>) -> McpPermissionFixture {
    let home = HooksHome::new();
    let script = home.path("server.py");
    let (permission, post, calls) = (
        home.path("permission.jsonl"),
        home.path("post.jsonl"),
        home.path("calls.jsonl"),
    );
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
        result = {'tools':[{'name':'echo','inputSchema':{'type':'object','properties':{}},'annotations':{'readOnlyHint':False,'destructiveHint':True,'openWorldHint':False}}]}
    elif method == 'tools/call':
        with open(sys.argv[1], 'a') as log:
            log.write(json.dumps(message['params'].get('arguments')) + '\n')
        result = {'content':[{'type':'text','text':'ran'}]}
    else:
        result = {}
    print(json.dumps({'jsonrpc':'2.0','id':message['id'],'result':result}), flush=True)
"#).unwrap();
    let permission_command = decision.map_or_else(
        || logging(&permission),
        |decision| format!("{}; {decision}", logging(&permission)),
    );
    home.write_hooks_with_approval(
        "on-request",
        &format!(
            "[features]\ncodex_hooks = true\n[mcp_servers.echo]\ncommand = \"python3\"\ndefault_tools_approval_mode = \"{mode}\"\nargs = [{}, {}]\n{}\n{}",
            ra_config::TomlValue::String(script.to_string_lossy().into_owned()),
            ra_config::TomlValue::String(calls.to_string_lossy().into_owned()),
            hook_section("PermissionRequest", &permission_command),
            hook_section("PostToolUse", &logging(&post)),
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
    let mcp = CodingMcpRuntime::new(
        plan.servers,
        McpDiscoveryOptions::new().with_naming(McpToolNaming::Namespaced),
    )
    .unwrap();
    mcp.reload_mcp_servers().await.unwrap();
    McpPermissionFixture {
        fixture: ExecutorFixture {
            home,
            mcp,
            calls: Arc::new(Mutex::new(Vec::new())),
        },
        permission,
        post,
        calls,
    }
}

fn mcp_call(id: &str, arguments: Value) -> Step {
    Step::Respond(ModelResponse::new(vec![RunItem::new(
        ItemId::new(id),
        RunItemKind::ToolCall(
            ToolCall::new(CallId::new(id), "echo", arguments).with_namespace("mcp__echo"),
        ),
    )]))
}

fn server_calls(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn output_of(script: &Script, call: &str) -> ra_core::item::ToolCallOutput {
    script
        .last_request()
        .input()
        .iter()
        .find_map(|item| match item {
            ModelInputItem::ToolCallOutput(output) if output.call_id().as_str() == call => {
                Some(output.clone())
            }
            _ => None,
        })
        .expect("the model received the call's output")
}

fn asked_the_user(messages: &[Value]) -> bool {
    messages
        .iter()
        .any(|message| message["method"] == "mcpServer/elicitation/request")
}

/// Codex's `permission_request_hook_allows_mcp_tool_without_user_or_guardian_review`, with the
/// arguments shown to the hook as the call carried them: `{}` for an absent value (the adapters
/// parse a blank argument string to `{}`), `null` kept as `null`.
#[tokio::test]
async fn a_permission_hook_grants_a_product_mcp_call_without_asking_the_user() {
    for arguments in [json!({}), Value::Null, json!({"message": "x"})] {
        let McpPermissionFixture {
            fixture,
            permission,
            post: _,
            calls,
        } = mcp_fixture("prompt", Some(ALLOW)).await;
        let server = fixture.server(None);
        let mut connection = server.connect(1).await;
        let thread = start_thread(&server, &mut connection).await;
        fixture
            .home
            .script
            .push(mcp_call("call", arguments.clone()));
        fixture.home.script.answer("done");
        server.run_turn(&mut connection, &thread, "call").await;
        let messages = connection.drain().await;

        assert!(!asked_the_user(&messages), "{arguments}");
        let hooks = logged(&permission);
        assert_eq!(hooks.len(), 1, "{arguments}");
        assert_eq!(hooks[0]["tool_name"], "mcp__echo__echo");
        assert_eq!(hooks[0]["tool_input"], arguments);
        assert!(hooks[0].get("tool_use_id").is_none());
        if arguments.is_object() {
            assert_eq!(server_calls(&calls), [arguments]);
        }
        fixture.mcp.shutdown().await.unwrap();
    }
}

/// Codex's `permission_request_hook_denies_mcp_tool_without_user_or_guardian_review`: the tool
/// does not run, and the model reads exactly what this product renders for Codex's
/// `CallToolResult::from_error_text(message)`, as an error; no PostToolUse runs.
#[tokio::test]
async fn a_permission_hook_denies_a_product_mcp_call_as_codex_error_result() {
    let McpPermissionFixture {
        fixture,
        permission,
        post,
        calls,
    } = mcp_fixture("prompt", Some(DENY)).await;
    let server = fixture.server(None);
    let mut connection = server.connect(1).await;
    let thread = start_thread(&server, &mut connection).await;
    fixture
        .home
        .script
        .push(mcp_call("call", json!({"message": "x"})));
    fixture.home.script.answer("done");
    server.run_turn(&mut connection, &thread, "call").await;
    let messages = connection.drain().await;

    assert!(!asked_the_user(&messages));
    assert_eq!(logged(&permission).len(), 1);
    assert!(server_calls(&calls).is_empty());
    assert!(logged(&post).is_empty());
    let output = output_of(&fixture.home.script, "call");
    // Codex's `CallToolResult::from_error_text`, field for field.
    let codex_error: ra_mcp::rmcp::model::CallToolResult = serde_json::from_value(json!({
        "content": [{"type": "text", "text": "mcp blocked by hook"}],
        "isError": true,
    }))
    .unwrap();
    assert_eq!(
        output.output(),
        &serde_json::to_value(ra_mcp::tool::map_tool_result(codex_error).unwrap()).unwrap()
    );
    assert!(output.is_error());
    fixture.mcp.shutdown().await.unwrap();
}

/// Without a decision the user is asked through the elicitation, as before.
#[tokio::test]
async fn an_undecided_permission_hook_leaves_a_product_mcp_call_to_the_user() {
    let McpPermissionFixture {
        fixture,
        permission,
        post: _,
        calls,
    } = mcp_fixture("prompt", None).await;
    let server = fixture.server(None);
    let mut connection = server.connect(1).await;
    let thread = start_thread(&server, &mut connection).await;
    fixture
        .home
        .script
        .push(mcp_call("call", json!({"message": "x"})));
    fixture.home.script.answer("done");
    server.turn_start(&mut connection, &thread, "call").await;
    connection
        .until(|message| {
            message["method"] == "mcpServer/elicitation/request" && message.get("id").is_some()
        })
        .await;

    assert_eq!(logged(&permission).len(), 1);
    assert!(server_calls(&calls).is_empty());
    fixture.mcp.shutdown().await.unwrap();
}

/// A hook grant is Codex's `Approved` from a hook: nothing is remembered, so the same call in a
/// later turn is put to the hook again rather than run from a session approval.
#[tokio::test]
async fn a_hook_grant_is_not_remembered_for_the_session() {
    let McpPermissionFixture {
        fixture,
        permission,
        post: _,
        calls,
    } = mcp_fixture("auto", Some(ALLOW)).await;
    let server = fixture.server(None);
    let mut connection = server.connect(1).await;
    let thread = start_thread(&server, &mut connection).await;
    for turn in ["first", "second"] {
        fixture
            .home
            .script
            .push(mcp_call(turn, json!({"message": "x"})));
        fixture.home.script.answer("done");
        server.run_turn(&mut connection, &thread, turn).await;
    }
    let messages = connection.drain().await;

    assert!(!asked_the_user(&messages));
    assert_eq!(logged(&permission).len(), 2);
    assert_eq!(server_calls(&calls).len(), 2);
    fixture.mcp.shutdown().await.unwrap();
}
