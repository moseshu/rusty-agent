//! Resource tool contracts exercised through real initialized MCP sessions and Runner dispatch.

use std::{
    num::NonZeroUsize,
    sync::{Arc, Mutex},
    time::Duration,
};

use ra_core::{
    agent::{AgentId, AgentSpec},
    context::RunContext,
    error::Result,
    item::CallId,
    permission::ActionClass,
    state::RunId,
    tool::{Tool as CoreTool, ToolConcurrency, ToolContext, ToolOutput},
};
use ra_mcp::{client::McpClientOptions, in_process::InProcessToolServer};
use ra_tools::mcp_ns::resource_tools;
use rmcp::{ErrorData, RoleServer, ServerHandler, model::*, service::RequestContext};
use serde_json::{Value, json};

type Requests = Arc<Mutex<Vec<(String, Option<String>)>>>;

#[derive(Clone)]
struct Resources {
    mode: &'static str,
    requests: Requests,
    entered: Arc<tokio::sync::Notify>,
    cancelled: Arc<tokio::sync::Notify>,
}

impl Resources {
    fn new(mode: &'static str) -> Self {
        Self {
            mode,
            requests: Arc::default(),
            entered: Arc::default(),
            cancelled: Arc::default(),
        }
    }
    async fn page(
        &self,
        method: &str,
        request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> std::result::Result<Value, ErrorData> {
        let cursor = request.and_then(|request| request.cursor);
        self.requests
            .lock()
            .unwrap()
            .push((method.to_owned(), cursor.clone()));
        if self.mode == "slow" {
            self.entered.notify_one();
            context.ct.cancelled().await;
            self.cancelled.notify_one();
        }
        if self.mode == "slow_page" {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        if self.mode == "many_slow" {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        if self.mode == "failure" || (self.mode == "late_failure" && cursor.is_some()) {
            return Err(ErrorData::invalid_params(
                "resource catalog unavailable",
                None,
            ));
        }
        let second = cursor.is_some();
        let mut next = if second { None } else { Some(String::new()) };
        if self.mode == "cycle" {
            next = Some("repeat".to_owned());
        }
        if self.mode == "many" || self.mode == "many_slow" {
            next = Some(self.requests.lock().unwrap().len().to_string());
        }
        if self.mode == "cursor_bound" {
            next = Some("c".repeat(65_537));
        }
        let item = if method == "resources/list" {
            json!({"uri":format!("memo://{}", if second { "second" } else { "first" }), "name":"note", "mimeType":"text/plain", "description":"Resource data"})
        } else {
            json!({"uriTemplate":format!("memo://{}/{{id}}", if second { "second" } else { "first" }), "name":"notes", "mimeType":"text/plain"})
        };
        let key = if method == "resources/list" {
            "resources"
        } else {
            "resourceTemplates"
        };
        let count = if self.mode == "overflow" { 2049 } else { 1 };
        Ok(json!({key:vec![item; count], "nextCursor":next}))
    }
}

impl ServerHandler for Resources {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_resources().build())
    }
    async fn list_resources(
        &self,
        request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> std::result::Result<ListResourcesResult, ErrorData> {
        Ok(serde_json::from_value(self.page("resources/list", request, context).await?).unwrap())
    }
    async fn list_resource_templates(
        &self,
        request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> std::result::Result<ListResourceTemplatesResult, ErrorData> {
        Ok(serde_json::from_value(
            self.page("resources/templates/list", request, context)
                .await?,
        )
        .unwrap())
    }
    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _: RequestContext<RoleServer>,
    ) -> std::result::Result<ReadResourceResponse, ErrorData> {
        self.requests
            .lock()
            .unwrap()
            .push(("resources/read".to_owned(), Some(request.uri.clone())));
        if request.uri == "memo://missing" {
            return Err(ErrorData::invalid_params("resource not found", None));
        }
        if self.mode == "slow_page" {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        Ok(
            serde_json::from_value::<ReadResourceResult>(
                json!({"_meta":{"revision":2},"contents":[
                    {"uri":request.uri,"mimeType":"text/plain","text":"Resource fixture: café"},
                    {"uri":"memo://binary","mimeType":"application/octet-stream","blob":"AAH/"}
                ]}),
            )
            .unwrap()
            .into(),
        )
    }
}

async fn session(name: &str, handler: Resources, options: McpClientOptions) -> InProcessToolServer {
    InProcessToolServer::connect(name, handler, options)
        .await
        .unwrap()
}

fn run_context() -> RunContext {
    let agent = AgentSpec::builder()
        .id(AgentId::new("resource-agent"))
        .name("Resources")
        .build()
        .unwrap();
    RunContext::new(RunId::new("resource-run"), &agent)
}

async fn call(tool: &dyn CoreTool, arguments: Value) -> Result<ToolOutput> {
    let run = run_context();
    let id = CallId::new("resource-call");
    tool.call(ToolContext::new(&run, tool, &id, &arguments))
        .await
}

async fn shaped(tool: &dyn CoreTool, arguments: Value) -> ToolOutput {
    match call(tool, arguments.clone()).await {
        Ok(output) => output,
        Err(error) => {
            let run = run_context();
            let id = CallId::new("resource-call");
            tool.handle_failure(&ToolContext::new(&run, tool, &id, &arguments), &error)
                .await
                .unwrap()
                .unwrap()
        }
    }
}

fn payload(output: &ToolOutput) -> Value {
    serde_json::from_str(output.as_text().unwrap()).unwrap()
}

#[test]
fn resource_schemas_preserve_the_three_upstream_public_contracts() {
    let tools = resource_tools(vec![]).unwrap();
    let definitions: Vec<_> = tools
        .iter()
        .map(|tool| {
            tool.validate().unwrap();
            assert!(!tool.schema().strict_json_schema());
            assert_eq!(tool.options().concurrency(), ToolConcurrency::Parallel);
            assert_eq!(tool.options().action_classes(), &[ActionClass::from("read")]);
            json!({"name":tool.schema().name(), "description":tool.schema().description(),
                "parameters":tool.schema().input_schema(), "strict":tool.schema().strict_json_schema()})
        })
        .collect();
    insta::assert_json_snapshot!("mcp_resource_schemas", definitions);
    assert_eq!(
        tools[2].schema().input_schema()["required"],
        json!(["server", "uri"])
    );
    assert!(tools[0].schema().input_schema().get("required").is_none());
}

#[tokio::test]
async fn single_server_lists_one_page_and_keeps_opaque_response_cursors() {
    let handler = Resources::new("normal");
    let requests = handler.requests.clone();
    let mut server = session("docs", handler, McpClientOptions::default()).await;
    let tools = resource_tools(vec![server.client()]).unwrap();
    for (index, key, method) in [
        (0, "resources", "resources/list"),
        (1, "resourceTemplates", "resources/templates/list"),
    ] {
        let first = payload(
            &call(
                tools[index].as_ref(),
                json!({"server":"  docs  ","cursor":"  "}),
            )
            .await
            .unwrap(),
        );
        assert_eq!(first["server"], "docs");
        assert_eq!(first["nextCursor"], "");
        let text = call(tools[index].as_ref(), json!({"server":"docs"}))
            .await
            .unwrap();
        // Codex's typed payloads put `server` before the listing and each flattened entry.
        assert!(
            text.as_text()
                .unwrap()
                .starts_with(&format!(r#"{{"server":"docs","{key}":[{{"server":"docs","#)),
            "{text:?}"
        );
        assert_eq!(first[key].as_array().unwrap().len(), 1);
        assert_eq!(first[key][0]["server"], "docs");
        let next = payload(
            &call(
                tools[index].as_ref(),
                json!({"server":"docs","cursor":"  opaque cursor  "}),
            )
            .await
            .unwrap(),
        );
        assert!(next.get("nextCursor").is_none());
        assert_eq!(
            requests.lock().unwrap().last().unwrap(),
            &(method.to_owned(), Some("opaque cursor".to_owned()))
        );
    }
    assert_eq!(requests.lock().unwrap().len(), 6);
    server.cleanup().await.unwrap();
}

#[tokio::test]
async fn all_server_lists_are_complete_sorted_and_isolate_failed_servers() {
    let alpha = Resources::new("normal");
    let requests = alpha.requests.clone();
    let mut servers = vec![
        session(
            "beta",
            Resources::new("normal"),
            McpClientOptions::default(),
        )
        .await,
        session(
            "broken",
            Resources::new("failure"),
            McpClientOptions::default(),
        )
        .await,
        session("alpha", alpha, McpClientOptions::default()).await,
        session(
            "partial",
            Resources::new("late_failure"),
            McpClientOptions::default(),
        )
        .await,
    ];
    let tools = resource_tools(servers.iter().map(InProcessToolServer::client).collect()).unwrap();
    for (index, key) in [(0, "resources"), (1, "resourceTemplates")] {
        let output = call(tools[index].as_ref(), json!({})).await.unwrap();
        let result = payload(&output);
        assert!(result.get("server").is_none());
        assert!(result.get("nextCursor").is_none());
        let entries = result[key].as_array().unwrap();
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry["server"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["alpha", "alpha", "beta", "beta"]
        );
        // Codex returns the payload alone; no extra model-visible note is attached.
        assert!(output.metadata().render().is_none());
    }
    assert_eq!(requests.lock().unwrap()[1].1, Some(String::new()));
    assert_eq!(requests.lock().unwrap()[3].1, Some(String::new()));
    for server in &mut servers {
        server.cleanup().await.unwrap();
    }
}

#[tokio::test]
async fn list_defaults_and_ignored_extra_fields_follow_upstream_parsing() {
    let tools = resource_tools(vec![]).unwrap();
    for arguments in [
        Value::Null,
        json!({}),
        json!({"server":" ","cursor":" ","extra":true}),
    ] {
        assert_eq!(
            payload(&call(tools[0].as_ref(), arguments.clone()).await.unwrap()),
            json!({"resources":[]})
        );
        assert_eq!(
            payload(&call(tools[1].as_ref(), arguments).await.unwrap()),
            json!({"resourceTemplates":[]})
        );
    }
}

#[tokio::test]
async fn resource_reads_preserve_text_binary_and_metadata_without_opening_uris_locally() {
    let handler = Resources::new("normal");
    let requests = handler.requests.clone();
    let mut server = session("docs", handler, McpClientOptions::default()).await;
    let tools = resource_tools(vec![server.client()]).unwrap();
    // This deliberately unlisted URI must still be sent to the server, as in Codex.
    let uri = "file:///not/a/local/resource";
    let output = call(
        tools[2].as_ref(),
        json!({"server":" docs ","uri":format!(" {uri} "),"extra":true}),
    )
    .await
    .unwrap();
    // Codex serializes `server` and `uri` before the flattened read result.
    assert!(
        output
            .as_text()
            .unwrap()
            .starts_with(&format!(r#"{{"server":"docs","uri":"{uri}","contents":["#)),
        "{output:?}"
    );
    let result = payload(&output);
    assert_eq!(result["server"], "docs");
    assert_eq!(result["uri"], uri);
    assert_eq!(result["_meta"]["revision"], 2);
    assert_eq!(result["contents"][0]["text"], "Resource fixture: café");
    assert_eq!(result["contents"][1]["blob"], "AAH/");
    assert_eq!(requests.lock().unwrap()[0].1.as_deref(), Some(uri));
    server.cleanup().await.unwrap();
}

#[tokio::test]
async fn invalid_arguments_and_server_failures_are_actionable_model_observations() {
    let mut server = session(
        "docs",
        Resources::new("normal"),
        McpClientOptions::default(),
    )
    .await;
    let tools = resource_tools(vec![server.client()]).unwrap();
    // Argument errors are returned verbatim; lookup and remote failures carry Codex's
    // `{method} failed:` prefix and the connection manager's per-server context.
    for (index, args, expected) in [
        (
            0,
            json!({"cursor":"next"}),
            "cursor can only be used when a server is specified",
        ),
        (
            0,
            json!({"server":"unknown"}),
            "resources/list failed: unknown MCP server 'unknown'",
        ),
        (
            1,
            json!({"server":4}),
            "failed to parse function arguments: invalid type: number, expected a string",
        ),
        (
            2,
            Value::Null,
            "failed to parse function arguments: expected value",
        ),
        (
            2,
            json!({"server":" ","uri":"memo://first"}),
            "server must be provided",
        ),
        (
            2,
            json!({"server":"docs","uri":" "}),
            "uri must be provided",
        ),
        (
            2,
            json!({"server":"docs","uri":"memo://missing"}),
            "resources/read failed: resources/read failed for `docs` (memo://missing): \
             MCP server error -32602: resource not found",
        ),
    ] {
        let output = shaped(tools[index].as_ref(), args).await;
        assert_eq!(output.as_text().unwrap(), expected, "{output:?}");
    }
    server.cleanup().await.unwrap();
}

#[tokio::test]
async fn automatic_catalog_limits_do_not_change_explicit_resource_pagination() {
    for index in [0, 1] {
        let mut server = session(
            "docs",
            Resources::new("normal"),
            McpClientOptions::default().with_max_list_pages(NonZeroUsize::new(1)),
        )
        .await;
        let client = server.client();
        let error = if index == 0 {
            client.list_all_resources().await.unwrap_err()
        } else {
            client.list_all_resource_templates().await.unwrap_err()
        };
        assert!(error.to_string().contains("exceeded 1 pages"));
        let tools = resource_tools(vec![client]).unwrap();
        let output = call(tools[index].as_ref(), json!({"server":"docs"}))
            .await
            .unwrap();
        assert_eq!(payload(&output)["nextCursor"], "");
        server.cleanup().await.unwrap();
    }
}

#[tokio::test]
async fn automatic_listing_rejects_cursor_cycles_and_selected_codex_bounds() {
    for mode in ["cycle", "overflow", "cursor_bound"] {
        let mut server = session(
            "docs",
            Resources::new(mode),
            McpClientOptions::default().with_codex_catalog_limits(),
        )
        .await;
        for templates in [false, true] {
            let error = if templates {
                server
                    .client()
                    .list_all_resource_templates()
                    .await
                    .unwrap_err()
            } else {
                server.client().list_all_resources().await.unwrap_err()
            };
            assert!(
                error.to_string().contains(match mode {
                    "cycle" => "repeated",
                    "overflow" => "2048 items",
                    _ => "65536 bytes",
                }),
                "{error}"
            );
        }
        server.cleanup().await.unwrap();
    }
}

#[tokio::test]
async fn aggregate_deadline_applies_to_resource_and_template_pagination() {
    let mut server = session(
        "docs",
        Resources::new("many"),
        McpClientOptions::default().with_catalog_timeout(Duration::ZERO),
    )
    .await;
    for templates in [false, true] {
        let error = if templates {
            server
                .client()
                .list_all_resource_templates()
                .await
                .unwrap_err()
        } else {
            server.client().list_all_resources().await.unwrap_err()
        };
        assert!(error.to_string().contains("catalog listing timed out"));
    }
    server.cleanup().await.unwrap();
}

#[tokio::test]
async fn failed_or_timed_out_servers_do_not_discard_healthy_resource_results() {
    let mut slow = session(
        "slow",
        Resources::new("slow"),
        McpClientOptions::default().with_session_timeout(Some(Duration::from_millis(20))),
    )
    .await;
    let mut good = session(
        "good",
        Resources::new("normal"),
        McpClientOptions::default(),
    )
    .await;
    let tools = resource_tools(vec![slow.client(), good.client()]).unwrap();
    for (index, key) in [(0, "resources"), (1, "resourceTemplates")] {
        let result = payload(&call(tools[index].as_ref(), json!({})).await.unwrap());
        assert_eq!(result[key].as_array().unwrap().len(), 2);
        assert_eq!(result[key][0]["server"], "good");
        let error = shaped(tools[index].as_ref(), json!({"server":"slow"})).await;
        assert!(error.as_text().unwrap().contains("timed out"));
    }
    slow.cleanup().await.unwrap();
    good.cleanup().await.unwrap();
}

#[tokio::test]
async fn duplicate_server_names_fail_before_ambiguous_resource_routing() {
    let mut server = session(
        "docs",
        Resources::new("normal"),
        McpClientOptions::default(),
    )
    .await;
    assert!(resource_tools(vec![server.client(), server.client()]).is_err());
    server.cleanup().await.unwrap();
}

#[tokio::test]
async fn resource_aggregation_defaults_port_codex_page_item_and_cursor_limits() {
    for (mode, expected) in [
        ("many", "100 pages"),
        ("overflow", "2048 items"),
        ("cursor_bound", "65536 bytes"),
    ] {
        let handler = Resources::new(mode);
        let requests = handler.requests.clone();
        let mut server = session("docs", handler, McpClientOptions::default()).await;
        for templates in [false, true] {
            let before = requests.lock().unwrap().len();
            let error = if templates {
                server
                    .client()
                    .list_all_resource_templates()
                    .await
                    .unwrap_err()
            } else {
                server.client().list_all_resources().await.unwrap_err()
            };
            assert!(error.to_string().contains(expected), "{error}");
            assert_eq!(
                requests.lock().unwrap().len() - before,
                if mode == "many" { 100 } else { 1 }
            );
        }
        server.cleanup().await.unwrap();
    }
}

#[tokio::test]
async fn cancelling_an_aggregate_call_cancels_each_remote_request_and_keeps_sessions_usable() {
    for index in [0, 1] {
        let first = Resources::new("slow");
        let second = Resources::new("slow");
        let mut alpha = session(
            "alpha",
            first.clone(),
            McpClientOptions::default().with_session_timeout(None),
        )
        .await;
        let mut beta = session(
            "beta",
            second.clone(),
            McpClientOptions::default().with_session_timeout(None),
        )
        .await;
        let tools = resource_tools(vec![alpha.client(), beta.client()]).unwrap();
        let tool = tools[index].clone();
        let task = tokio::spawn(async move { call(tool.as_ref(), json!({})).await });
        tokio::time::timeout(Duration::from_secs(1), async {
            first.entered.notified().await;
            second.entered.notified().await;
        })
        .await
        .unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        tokio::time::timeout(Duration::from_secs(1), async {
            first.cancelled.notified().await;
            second.cancelled.notified().await;
        })
        .await
        .unwrap();
        assert!(alpha.client().is_connected().await);
        assert!(beta.client().is_connected().await);
        alpha.cleanup().await.unwrap();
        beta.cleanup().await.unwrap();
    }
}

#[tokio::test]
async fn resource_requests_use_the_tool_deadline_instead_of_the_session_deadline() {
    let mut server = session(
        "docs",
        Resources::new("slow_page"),
        McpClientOptions::default()
            .with_session_timeout(Some(Duration::from_millis(20)))
            .with_tool_timeout(Duration::from_secs(5)),
    )
    .await;
    let client = server.client();
    // The Python-compatible single-page API keeps the session deadline.
    assert!(client.list_resources(None).await.is_err());
    let tools = resource_tools(vec![client]).unwrap();
    let single = payload(
        &call(tools[0].as_ref(), json!({"server":"docs"}))
            .await
            .unwrap(),
    );
    assert_eq!(single["resources"].as_array().unwrap().len(), 1);
    let all = payload(&call(tools[1].as_ref(), json!({})).await.unwrap());
    assert_eq!(all["resourceTemplates"].as_array().unwrap().len(), 2);
    let read = payload(
        &call(
            tools[2].as_ref(),
            json!({"server":"docs","uri":"memo://first"}),
        )
        .await
        .unwrap(),
    );
    assert_eq!(read["contents"][0]["text"], "Resource fixture: café");
    server.cleanup().await.unwrap();
}

#[tokio::test]
async fn an_explicit_tool_timeout_bounds_the_whole_aggregation_without_a_catalog_timeout() {
    let mut server = session(
        "docs",
        Resources::new("many_slow"),
        McpClientOptions::default()
            .with_session_timeout(None)
            .with_tool_timeout(Duration::from_millis(200)),
    )
    .await;
    // Each page takes 10 ms, so only the overall deadline can stop this before 100 pages.
    let error = server.client().list_all_resources().await.unwrap_err();
    assert!(
        error.to_string().contains("catalog listing timed out"),
        "{error}"
    );
    server.cleanup().await.unwrap();
}
