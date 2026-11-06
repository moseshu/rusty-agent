// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! MCP hook identity, protocol results and error gating through the default product factory.
use super::*;
use ra_mcp::tool::McpToolNaming;

async fn payload_fixture(
    naming: McpToolNaming,
    is_error: Option<bool>,
    tool: &str,
    rewrite: bool,
) -> (Vec<Value>, Value) {
    let home = HooksHome::new();
    let script = home.path("payload_server.py");
    let log = home.path("payload_hooks.jsonl");
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
        result = {'tools':[{'name':sys.argv[2],'inputSchema':{'type':'object','properties':{}},'annotations':{'readOnlyHint':True,'destructiveHint':False,'openWorldHint':False}}]}
    elif method == 'tools/call':
        result = {'content':[{'type':'text','text':'original result'}], 'structuredContent':{'arguments':message['params']['arguments']}, '_meta':{'provider/custom':{'items':[1,None]}}}
        if sys.argv[1] != 'none':
            result['isError'] = sys.argv[1] == 'true'
    else:
        result = {}
    print(json.dumps({'jsonrpc':'2.0','id':message['id'],'result':result}), flush=True)
"#).unwrap();
    let pre = if rewrite {
        format!(
            "{}; printf '%s' '{{\"hookSpecificOutput\":{{\"hookEventName\":\"PreToolUse\",\"permissionDecision\":\"allow\",\"updatedInput\":{{\"message\":\"rewritten\"}}}}}}'",
            logging(&log)
        )
    } else {
        logging(&log)
    };
    home.write_hooks(&format!(
        r#"
[features]
codex_hooks = true
[mcp_servers.echo]
command = "python3"
default_tools_approval_mode = "approve"
args = [{}, "{}", "{}"]
{}
{}
"#,
        ra_config::TomlValue::String(script.to_string_lossy().into_owned()),
        is_error.map_or("none", |error| if error { "true" } else { "false" }),
        tool,
        hook_section("PreToolUse", &pre),
        hook_section("PostToolUse", &logging(&log))
    ));
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
    let mcp = CodingMcpRuntime::new(plan.servers, McpDiscoveryOptions::new().with_naming(naming))
        .unwrap();
    mcp.reload_mcp_servers().await.unwrap();
    let fixture = ExecutorFixture {
        home,
        mcp,
        calls: Arc::new(Mutex::new(Vec::new())),
    };
    // Bare MCP tools that collide with installed built-ins can be rejected by assembly.
    // Exercise their prepared executable directly to verify the adapter still identifies MCP.
    if matches!(naming, McpToolNaming::Original) && tool != "echo" {
        use ra_core::tool::ToolResolver;
        let agent = AgentSpec::builder()
            .id(AgentId::new("mcp-preflight"))
            .name("MCP")
            .build()
            .unwrap();
        let run = Arc::new(ra_core::context::RunContext::new(
            RunId::new("mcp-preflight"),
            &agent,
        ));
        let tools = fixture
            .mcp
            .get_tools(&run, &std::collections::BTreeSet::new())
            .await
            .unwrap();
        let selected = tools
            .iter()
            .find(|entry| entry.origin().name() == tool)
            .unwrap();
        let sid = SessionId::new("mcp-preflight");
        let runtime = ra_coding::hooks::ProductHookRuntime::new(
            ra_coding::hooks::HooksConfig {
                feature_enabled: true,
                bypass_hook_trust: true,
                config_layer_stack: Some(config.config_layer_stack.clone()),
                ..Default::default()
            },
            ra_coding::hooks::ProductHookSession {
                session_id: sid.clone(),
                cwd: ra_paths::AbsolutePathBuf::from_absolute_path(
                    fixture.home.workspace.path().canonicalize().unwrap(),
                )
                .unwrap(),
                model: "mock-model".into(),
                approval_policy: ra_config::AskForApproval::Never,
                transcript_path: None,
            },
            Arc::new(ra_coding::hook_mcp_executor::CodingHookMcpExecutor::new(
                fixture.mcp.clone(),
                sid,
            )),
        )
        .unwrap();
        let bound = runtime.user_hooks().bind(
            run,
            ra_core::cancel::CancelScope::root(),
            ra_core::tool::ToolServices::new(),
        );
        let id = CallId::new("call");
        let arguments = json!({"message":"original"});
        let call = ra_core::hook::ToolHookData::new(selected.origin(), &id, &arguments)
            .with_tool(selected.as_ref());
        assert_eq!(
            bound
                .dispatch(ra_core::hook::HookEvent::PreToolUse(call))
                .await
                .unwrap(),
            ra_core::hook::HookDecision::UpdateInput {
                input: json!({"message":"rewritten"})
            }
        );
        let payloads = logged(&log);
        fixture.mcp.shutdown().await.unwrap();
        return (payloads, Value::Null);
    }
    let server = fixture.server(None);
    let mut connection = server.connect(1).await;
    let thread = start_thread(&server, &mut connection).await;
    let arguments = json!({"message":"original"});
    let call = match naming {
        McpToolNaming::Original => ToolCall::new(CallId::new("call"), tool, arguments),
        McpToolNaming::ServerPrefixed => {
            ToolCall::new(CallId::new("call"), format!("mcp_echo__{tool}"), arguments)
        }
        McpToolNaming::Namespaced => {
            ToolCall::new(CallId::new("call"), tool, arguments).with_namespace("mcp__echo")
        }
        _ => unreachable!(),
    };
    fixture
        .home
        .script
        .push(Step::Respond(ModelResponse::new(vec![RunItem::new(
            ItemId::new("mcp-call"),
            RunItemKind::ToolCall(call),
        )])));
    fixture.home.script.answer("reviewed");
    server.run_turn(&mut connection, &thread, "call MCP").await;
    let output = last_tool_output(&fixture.home.script);
    assert_eq!(output["blocks"][0]["text"], "original result");
    let payloads = logged(&log);
    fixture.mcp.shutdown().await.unwrap();
    (payloads, output)
}

#[tokio::test]
async fn mcp_hook_payload_keeps_canonical_identity_and_protocol_result_in_all_naming_modes() {
    for mode in [
        McpToolNaming::Original,
        McpToolNaming::ServerPrefixed,
        McpToolNaming::Namespaced,
    ] {
        for is_error in [None, Some(false)] {
            let (payloads, _) = payload_fixture(mode, is_error, "echo", true).await;
            assert_eq!(events_of(&payloads), ["PreToolUse", "PostToolUse"]);
            for payload in &payloads {
                assert_eq!(payload["tool_name"], "mcp__echo__echo");
                assert_eq!(payload["tool_use_id"], "call");
            }
            assert_eq!(payloads[0]["tool_input"], json!({"message":"original"}));
            assert_eq!(payloads[1]["tool_input"], json!({"message":"rewritten"}));
            let mut expected = json!({"content":[{"type":"text","text":"original result"}], "structuredContent":{"arguments":{"message":"rewritten"}}, "_meta":{"provider/custom":{"items":[1,null]}}});
            if let Some(error) = is_error {
                expected["isError"] = json!(error);
            }
            assert_eq!(payloads[1]["tool_response"], expected);
        }
    }
}

#[tokio::test]
async fn mcp_error_result_keeps_model_content_without_running_post_tool_use() {
    for mode in [
        McpToolNaming::Original,
        McpToolNaming::ServerPrefixed,
        McpToolNaming::Namespaced,
    ] {
        let (payloads, _) = payload_fixture(mode, Some(true), "echo", false).await;
        assert_eq!(events_of(&payloads), ["PreToolUse"]);
        assert_eq!(payloads[0]["tool_name"], "mcp__echo__echo");
    }
}

#[tokio::test]
async fn namespaced_mcp_builtin_like_tools_keep_mcp_payloads_and_rewrites() {
    for tool in ["exec_command", "apply_patch", "spawn_agent"] {
        let (payloads, _) = payload_fixture(McpToolNaming::Namespaced, None, tool, true).await;
        assert_eq!(events_of(&payloads), ["PreToolUse", "PostToolUse"]);
        for payload in &payloads {
            assert_eq!(payload["tool_name"], format!("mcp__echo__{tool}"));
        }
        assert_eq!(payloads[0]["tool_input"], json!({"message":"original"}));
        assert_eq!(payloads[1]["tool_input"], json!({"message":"rewritten"}));
        assert_eq!(
            payloads[1]["tool_response"]["structuredContent"]["arguments"],
            json!({"message":"rewritten"})
        );
    }
}

#[tokio::test]
async fn original_mcp_builtin_like_tools_keep_mcp_identity_before_assembly() {
    for tool in ["exec_command", "apply_patch", "spawn_agent"] {
        let (payloads, _) = payload_fixture(McpToolNaming::Original, None, tool, true).await;
        assert_eq!(events_of(&payloads), ["PreToolUse"]);
        assert_eq!(payloads[0]["tool_name"], format!("mcp__echo__{tool}"));
        assert_eq!(payloads[0]["tool_input"], json!({"message":"original"}));
    }
}
