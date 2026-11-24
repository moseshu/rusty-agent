// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
use super::*;
use ra_session::{rollout::RolloutPayload, store::LoadThreadHistoryParams};

fn setup_catalog(product: &ProductModels, endpoint: &MockServer, features: &str) {
    product.write(
        "collaboration-models.json",
        &json!({"models": [
            catalog_model("catalog-v2", Some("v2")),
            catalog_model("catalog-v1", Some("v1")),
            catalog_model("catalog-unspecified", None),
        ]})
        .to_string(),
    );
    product.write(
        "config.toml",
        &format!(
            r#"
approval_policy = "never"
sandbox_mode = "read-only"
model = "catalog-v1"
model_provider = "gateway"
model_catalog_json = {:?}
[model_providers.gateway]
name = "Gateway"
base_url = "{}/product/v1"
stream_max_retries = 0
{}
"#,
            product.home.path().join("collaboration-models.json"),
            endpoint.uri(),
            features,
        ),
    );
}

pub(crate) fn catalog_model(slug: &str, version: Option<&str>) -> Value {
    json!({
        "slug": slug, "display_name": slug, "multi_agent_version": version,
        "default_reasoning_level": "low", "supported_reasoning_levels": [],
        "shell_type": "unified_exec", "visibility": "list", "supported_in_api": true,
        "priority": 1, "support_verbosity": false,
        "truncation_policy": {"mode": "tokens", "limit": 1000},
        "experimental_supported_tools": [], "context_window": 8000,
        "base_instructions": "Test model instructions",
    })
}

fn server_with_disk(product: &ProductModels) -> Server {
    let (host, settings, builder) = product.parts();
    let builder = builder.harness_overrides(ra_coding::config::ConfigOverrides {
        bypass_hook_trust: Some(true),
        ..Default::default()
    });
    let directory =
        ra_session::rollout::RolloutThreadDirectory::new(product.home.path().join("rollouts"));
    build(
        Options {
            store: Some(Arc::new(directory)),
            factory: Some(Arc::new(CodingThreadFactory::from_product_config(
                host, settings, builder,
            ))),
            unload_delay: Some(Duration::ZERO),
        },
        None,
    )
    .0
}

async fn run_on_model(
    server: &Server,
    connection: &mut Connection,
    thread_id: &str,
    model: &str,
) -> String {
    let result = server
        .call(
            connection,
            "turn/start",
            serde_json::to_value(
                TurnStartParams::new(thread_id, vec![UserInput::text("Hello")]).with_model(model),
            )
            .unwrap(),
        )
        .await;
    let completed = connection.notification("turn/completed").await;
    assert_eq!(completed["turn"]["status"], "completed", "{completed}");
    result["turn"]["id"].as_str().unwrap().to_owned()
}

async fn version_records(server: &Server, thread_id: &str) -> Vec<(String, String)> {
    server
        .store
        .load_history(&LoadThreadHistoryParams::new(SessionId::new(thread_id)))
        .await
        .unwrap()
        .into_records()
        .into_iter()
        .filter_map(|record| {
            let version = match record.payload().unwrap() {
                RolloutPayload::SessionMeta(meta) => meta.multi_agent_version().map(str::to_owned),
                RolloutPayload::TurnContext(ctx) => ctx.multi_agent_version().map(str::to_owned),
                _ => None,
            }?;
            Some((record.type_name().to_owned(), version))
        })
        .collect()
}

async fn assert_latest_tools(endpoint: &MockServer, model: &str, enabled: bool) {
    let requests = endpoint.received_requests().await.unwrap();
    let body: Value = requests.last().unwrap().body_json().unwrap();
    assert_eq!(body["model"], model);
    assert!(
        !body["instructions"]
            .as_str()
            .unwrap()
            .contains("collaboration.spawn_agent")
    );
    let collaboration = body["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "collaboration" && tool["type"] == "namespace");
    assert_eq!(collaboration.is_some(), enabled, "tools: {}", body["tools"]);
    if let Some(collaboration) = collaboration {
        let mut names = collaboration["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect::<Vec<_>>();
        names.sort_unstable();
        assert_eq!(
            names,
            [
                "followup_task",
                "interrupt_agent",
                "list_agents",
                "send_message",
                "spawn_agent",
                "wait_agent"
            ]
        );
    }
}

#[tokio::test]
async fn multi_agent_first_turn_model_selects_and_locks_version() {
    let endpoint = responses_endpoint().await;
    let product = ProductModels::new();
    setup_catalog(&product, &endpoint, "");
    let server = product.server(None);
    let mut connection = server.connect(1).await;

    for (first_model, next_model, version, enabled) in [
        ("catalog-v2", "catalog-v1", "v2", true),
        ("catalog-v1", "catalog-v2", "v1", false),
        ("catalog-unspecified", "catalog-v2", "v1", false),
        ("outside-catalog", "catalog-v2", "v1", false),
    ] {
        let (thread, _) = start(&server, &mut connection, json!({"model": "catalog-v1"})).await;
        run_on_model(&server, &mut connection, &thread, first_model).await;
        assert_latest_tools(&endpoint, first_model, enabled).await;
        let body = latest_body(&endpoint).await;
        assert_eq!(
            developer_texts(&body)
                .iter()
                .any(|text| text.starts_with("You are `/root`, the primary agent")),
            enabled,
        );
        run_on_model(&server, &mut connection, &thread, next_model).await;
        assert_latest_tools(&endpoint, next_model, enabled).await;
        assert_eq!(
            version_records(&server, &thread).await,
            vec![("turn_context".to_owned(), version.to_owned()); 2]
        );
    }
}

#[tokio::test]
async fn multi_agent_resume_honors_feature_overrides_and_legacy_history() {
    let endpoint = responses_endpoint().await;
    let product = ProductModels::new();
    setup_catalog(&product, &endpoint, "");
    let server = server_with_disk(&product);
    let mut connection = server.connect(1).await;
    let legacy = server
        .store_thread("legacy-collaboration", "old turn")
        .await;
    legacy.shutdown().await.unwrap();
    server
        .call(
            &mut connection,
            "thread/resume",
            json!({"threadId": "legacy-collaboration"}),
        )
        .await;
    run_on_model(
        &server,
        &mut connection,
        "legacy-collaboration",
        "catalog-v2",
    )
    .await;
    assert_latest_tools(&endpoint, "catalog-v2", false).await;
    assert_eq!(
        version_records(&server, "legacy-collaboration").await,
        [("turn_context".to_owned(), "disabled".to_owned())]
    );

    let (thread, _) = start(&server, &mut connection, json!({})).await;
    run_on_model(&server, &mut connection, &thread, "outside-catalog").await;
    unload(&server, &mut connection, &thread).await;

    server
        .call(
            &mut connection,
            "thread/resume",
            json!({"threadId": thread}),
        )
        .await;
    run_on_model(&server, &mut connection, &thread, "catalog-v2").await;
    assert_latest_tools(&endpoint, "catalog-v2", false).await;
    unload(&server, &mut connection, &thread).await;

    setup_catalog(
        &product,
        &endpoint,
        "[agents]\nenabled = false\n[features]\nmulti_agent_v2 = true",
    );
    server
        .call(
            &mut connection,
            "thread/resume",
            json!({"threadId": thread}),
        )
        .await;
    run_on_model(&server, &mut connection, &thread, "outside-catalog").await;
    assert_latest_tools(&endpoint, "outside-catalog", true).await;
    assert_eq!(
        version_records(&server, &thread).await,
        [
            ("turn_context".to_owned(), "v1".to_owned()),
            ("turn_context".to_owned(), "v1".to_owned()),
            ("turn_context".to_owned(), "v2".to_owned())
        ]
    );

    unload(&server, &mut connection, &thread).await;
    setup_catalog(&product, &endpoint, "[agents]\nenabled = false");
    server
        .call(
            &mut connection,
            "thread/resume",
            json!({"threadId": thread}),
        )
        .await;
    run_on_model(&server, &mut connection, &thread, "catalog-v2").await;
    assert_latest_tools(&endpoint, "catalog-v2", false).await;
    assert_eq!(
        version_records(&server, &thread).await.last().unwrap().1,
        "disabled"
    );
}

#[tokio::test]
async fn multi_agent_empty_rollout_selects_version_on_first_turn() {
    let endpoint = responses_endpoint().await;
    let product = ProductModels::new();
    setup_catalog(&product, &endpoint, "[features]\nmulti_agent = false");
    let server = server_with_disk(&product);
    let mut connection = server.connect(1).await;
    let id = SessionId::new("empty-collaboration");
    let recorder = server
        .store
        .create_thread_with(&CreateThreadParams::new(RolloutSessionMeta::new(
            id.clone(),
        )))
        .await
        .unwrap();
    recorder
        .persist(ra_core::session::rollout::PersistContext::Standard)
        .await
        .unwrap();
    recorder.shutdown().await.unwrap();
    server
        .call(&mut connection, "thread/resume", json!({"threadId": id}))
        .await;
    run_on_model(&server, &mut connection, id.as_str(), "catalog-v2").await;
    assert_latest_tools(&endpoint, "catalog-v2", true).await;

    let (thread, _) = start(&server, &mut connection, json!({})).await;
    run_on_model(&server, &mut connection, &thread, "outside-catalog").await;
    assert_latest_tools(&endpoint, "outside-catalog", false).await;
    assert_eq!(
        version_records(&server, &thread).await,
        [("turn_context".to_owned(), "disabled".to_owned())]
    );
}

#[cfg(unix)]
#[tokio::test]
async fn multi_agent_blocked_first_turn_can_select_v2_after_cold_resume() {
    let endpoint = responses_endpoint().await;
    let product = ProductModels::new();
    setup_catalog(
        &product,
        &endpoint,
        r#"
[features]
codex_hooks = true

[[hooks.UserPromptSubmit]]
[[hooks.UserPromptSubmit.hooks]]
type = "command"
command = '''cat >/dev/null; printf '%s' '{"decision":"block","reason":"blocked","hookSpecificOutput":{"hookEventName":"UserPromptSubmit","additionalContext":"context from blocked turn"}}' '''
"#,
    );
    let server = server_with_disk(&product);
    let mut connection = server.connect(1).await;
    let (thread, _) = start(&server, &mut connection, json!({"model": "catalog-v1"})).await;
    run_on_model(&server, &mut connection, &thread, "catalog-v1").await;
    assert!(endpoint.received_requests().await.unwrap().is_empty());
    unload(&server, &mut connection, &thread).await;

    let history = server
        .store
        .load_history(&LoadThreadHistoryParams::new(SessionId::new(&thread)))
        .await
        .unwrap();
    assert!(
        history
            .records()
            .iter()
            .any(|record| record.type_name() == "run_started")
    );
    assert!(history.records().iter().any(|record| {
        matches!(
            record.payload(),
            Ok(RolloutPayload::Item(item))
                if matches!(item.kind(), RunItemKind::Message(message)
                    if message.role() == ra_core::item::MessageRole::Developer
                        && message.text_content() == "context from blocked turn")
        )
    }));
    assert!(
        history
            .records()
            .iter()
            .any(|record| record.type_name() == "run_ended")
    );
    assert!(
        history
            .records()
            .iter()
            .all(|record| record.type_name() != "turn_context")
    );

    setup_catalog(&product, &endpoint, "");
    server
        .call(
            &mut connection,
            "thread/resume",
            json!({"threadId": thread}),
        )
        .await;
    run_on_model(&server, &mut connection, &thread, "catalog-v2").await;
    assert_latest_tools(&endpoint, "catalog-v2", true).await;
    assert_eq!(
        version_records(&server, &thread).await,
        [("turn_context".to_owned(), "v2".to_owned())]
    );
}

/// A recorded value this build does not recognize is kept rather than replaced by a migrated
/// one, and the thread is offered no collaboration tools; a fork keeps it as well.
#[tokio::test]
async fn multi_agent_unrecognized_recorded_version_is_preserved() {
    let endpoint = responses_endpoint().await;
    let product = ProductModels::new();
    setup_catalog(&product, &endpoint, "");
    let server = server_with_disk(&product);
    let mut connection = server.connect(1).await;
    let id = "future-collaboration";
    let recorder = server.store_thread(id, "old turn").await;
    recorder.record(ra_core::session::rollout::RolloutItem::TurnContext(
        ra_core::session::rollout::RolloutTurnContext::new(RunId::new("turn-1"), 0)
            .with_multi_agent_version("v3"),
    ));
    recorder.flush().await.unwrap();
    recorder.shutdown().await.unwrap();

    server
        .call(&mut connection, "thread/resume", json!({"threadId": id}))
        .await;
    run_on_model(&server, &mut connection, id, "catalog-v2").await;
    assert_latest_tools(&endpoint, "catalog-v2", false).await;
    assert_eq!(
        version_records(&server, id).await,
        [
            ("turn_context".to_owned(), "v3".to_owned()),
            ("turn_context".to_owned(), "v3".to_owned())
        ]
    );

    let fork = server
        .call(
            &mut connection,
            "thread/fork",
            json!({
                "threadId": id, "approvalPolicy": "never", "approvalsReviewer": "user",
                "sandbox": "read-only",
            }),
        )
        .await;
    let child = fork["thread"]["id"].as_str().unwrap();
    assert_eq!(
        version_records(&server, child).await.first().unwrap(),
        &("session_meta".to_owned(), "v3".to_owned())
    );
}

/// A read-only role on a product thread is told collaboration depends on the turn's tool list
/// while the version can still select V2, and is told it has none once the version rules it out.
#[tokio::test]
async fn multi_agent_read_only_role_guidance_follows_product_version() {
    let endpoint = responses_endpoint().await;
    let product = ProductModels::new();
    setup_catalog(&product, &endpoint, "");
    let (host, settings, builder) = product.parts();
    let directory =
        ra_session::rollout::RolloutThreadDirectory::new(product.home.path().join("rollouts"));
    let server = build(
        Options {
            store: Some(Arc::new(directory)),
            factory: Some(Arc::new(
                CodingThreadFactory::from_product_config(host, settings, builder)
                    .with_role(ra_core::prompt::PromptRole::Planner),
            )),
            unload_delay: Some(Duration::ZERO),
        },
        None,
    )
    .0;
    let mut connection = server.connect(1).await;
    let dynamic = "Collaboration tools are offered only in turns whose request lists them";
    let absent = "No collaboration tools are advertised";
    let instructions = |body: &Value| body["instructions"].as_str().unwrap().to_owned();

    let (thread, _) = start(&server, &mut connection, json!({})).await;
    run_on_model(&server, &mut connection, &thread, "catalog-v2").await;
    assert_latest_tools(&endpoint, "catalog-v2", true).await;
    let requests = endpoint.received_requests().await.unwrap();
    let text = instructions(&requests.last().unwrap().body_json().unwrap());
    assert!(text.contains(dynamic) && !text.contains(absent), "{text}");

    let legacy = server.store_thread("legacy-planner", "old turn").await;
    legacy.shutdown().await.unwrap();
    server
        .call(
            &mut connection,
            "thread/resume",
            json!({"threadId": "legacy-planner"}),
        )
        .await;
    run_on_model(&server, &mut connection, "legacy-planner", "catalog-v2").await;
    assert_latest_tools(&endpoint, "catalog-v2", false).await;
    let requests = endpoint.received_requests().await.unwrap();
    let text = instructions(&requests.last().unwrap().body_json().unwrap());
    assert!(text.contains(absent) && !text.contains(dynamic), "{text}");
}

/// The collaboration namespace's functions in the latest request, by name, if it was sent.
async fn latest_collaboration_tools(
    endpoint: &MockServer,
    namespace: &str,
) -> Option<std::collections::BTreeMap<String, Value>> {
    let requests = endpoint.received_requests().await.unwrap();
    let body: Value = requests.last().unwrap().body_json().unwrap();
    body["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == namespace && tool["type"] == "namespace")
        .map(|namespace| {
            namespace["tools"]
                .as_array()
                .unwrap()
                .iter()
                .map(|tool| (tool["name"].as_str().unwrap().to_owned(), tool.clone()))
                .collect()
        })
}

/// Codex's `[features.multi_agent_v2]` tool table: the namespace, the direct-message and
/// `wait_agent` switches, the wait bounds, the `spawn_agent` usage hint, and the switch that drops
/// the tools while the thread stays on V2. A model outside the catalog is offered them at the root.
#[tokio::test]
async fn multi_agent_v2_tool_table_follows_configuration() {
    let endpoint = responses_endpoint().await;
    let product = ProductModels::new();
    let server = server_with_disk(&product);
    let mut connection = server.connect(1).await;

    setup_catalog(
        &product,
        &endpoint,
        r#"
[features.multi_agent_v2]
enabled = true
tool_namespace = "agents"
disable_direct_message = true
usage_hint_text = "Delegate carefully."
min_wait_timeout_ms = 15000
default_wait_timeout_ms = 20000
max_wait_timeout_ms = 60000
"#,
    );
    let (thread, _) = start(&server, &mut connection, json!({})).await;
    run_on_model(&server, &mut connection, &thread, "outside-catalog").await;
    let tools = latest_collaboration_tools(&endpoint, "agents")
        .await
        .unwrap();
    assert_eq!(
        tools.keys().map(String::as_str).collect::<Vec<_>>(),
        [
            "interrupt_agent",
            "list_agents",
            "spawn_agent",
            "wait_agent"
        ]
    );
    assert!(
        latest_collaboration_tools(&endpoint, "collaboration")
            .await
            .is_none()
    );
    let spawn = tools["spawn_agent"]["description"].as_str().unwrap();
    assert!(spawn.ends_with("\nDelegate carefully."), "{spawn}");
    assert_eq!(
        tools["wait_agent"]["parameters"]["properties"]["timeout_ms"]["description"],
        "Timeout in milliseconds. Defaults to 20000, min 15000, max 60000."
    );
    let requests = endpoint.received_requests().await.unwrap();
    let body: Value = requests.last().unwrap().body_json().unwrap();
    assert!(
        !body["instructions"]
            .as_str()
            .unwrap()
            .contains("agents.spawn_agent"),
        "per-turn tools stay out of the stable prefix"
    );

    let mut response = websocket_completion(0);
    response["response"]["output"] = json!([{
        "id": "wait", "type": "function_call", "call_id": "wait-too-long",
        "namespace": "agents", "name": "wait_agent", "arguments": "{\"timeout_ms\":60001}"
    }]);
    Mock::given(method("POST"))
        .and(path("/product/v1/responses"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(format!("data: {response}\n\n"), "text/event-stream"),
        )
        .with_priority(1)
        .up_to_n_times(1)
        .expect(1)
        .mount(&endpoint)
        .await;
    run_on_model(&server, &mut connection, &thread, "outside-catalog").await;
    let requests = endpoint.received_requests().await.unwrap();
    let body: Value = requests.last().unwrap().body_json().unwrap();
    let output = body["input"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["type"] == "function_call_output" && item["call_id"] == "wait-too-long")
        .unwrap();
    assert_eq!(
        output["output"],
        json!([{"type": "input_text", "text": "timeout_ms must be at most 60000"}])
    );

    setup_catalog(
        &product,
        &endpoint,
        r#"
[features.multi_agent_v2]
enabled = true
wait_agent_enabled = false
tool_namespace = "wait_agent"
"#,
    );
    let (thread, _) = start(&server, &mut connection, json!({})).await;
    run_on_model(&server, &mut connection, &thread, "outside-catalog").await;
    let tools = latest_collaboration_tools(&endpoint, "wait_agent")
        .await
        .unwrap();
    assert_eq!(
        tools.keys().map(String::as_str).collect::<Vec<_>>(),
        [
            "followup_task",
            "interrupt_agent",
            "list_agents",
            "send_message",
            "spawn_agent"
        ]
    );

    setup_catalog(
        &product,
        &endpoint,
        r#"
[features.multi_agent_v2]
enabled = true
disable_native_tools = true
tool_namespace = "spawn_agent"
"#,
    );
    let (thread, _) = start(&server, &mut connection, json!({})).await;
    run_on_model(&server, &mut connection, &thread, "catalog-v2").await;
    assert_latest_tools(&endpoint, "catalog-v2", false).await;
    assert!(
        latest_collaboration_tools(&endpoint, "spawn_agent")
            .await
            .is_none()
    );
    let body = latest_body(&endpoint).await;
    let hint = developer_texts(&body)
        .into_iter()
        .find(|text| text.starts_with("You are `/root`, the primary agent"))
        .expect("disabling native tools preserves the V2 role");
    assert!(!hint.contains("only as direct tool calls"), "{hint}");
    assert!(!hint.contains("When calling `wait_agent`"), "{hint}");
    assert_eq!(
        version_records(&server, &thread).await,
        [("turn_context".to_owned(), "v2".to_owned())]
    );
}

/// A host whose model adapters refuse namespaced tools keeps its explicit bare-name choice over the
/// configured namespace, through an injected resolver.
#[tokio::test]
async fn multi_agent_explicit_host_namespace_overrides_configuration() {
    let endpoint = responses_endpoint().await;
    let product = ProductModels::new();
    setup_catalog(
        &product,
        &endpoint,
        "[features.multi_agent_v2]\nenabled = true\ntool_namespace = \"agents\"\n",
    );
    let (host, settings, builder) = product.parts();
    let script = Arc::new(Script::default());
    script.answer("bare");
    let server = build(
        Options {
            store: Some(Arc::new(InMemoryThreadStore::new())),
            factory: Some(Arc::new(
                CodingThreadFactory::new(
                    host.with_collaboration().with_collaboration_namespace(None),
                    Arc::new(Resolver(Arc::clone(&script))),
                    settings,
                )
                .with_product_config(builder),
            )),
            unload_delay: None,
        },
        None,
    )
    .0;
    let mut connection = server.connect(1).await;
    let (thread, _) = start(&server, &mut connection, json!({})).await;
    server.run_turn(&mut connection, &thread, "Delegate").await;
    let request = script.last_request();
    let mut names = request
        .tools()
        .iter()
        .filter(|tool| {
            matches!(
                tool.name(),
                "spawn_agent"
                    | "send_message"
                    | "followup_task"
                    | "interrupt_agent"
                    | "wait_agent"
                    | "list_agents"
            )
        })
        .map(|tool| (tool.namespace().map(str::to_owned), tool.name().to_owned()))
        .collect::<Vec<_>>();
    names.sort();
    assert_eq!(names.len(), 6, "{names:?}");
    assert!(
        names.iter().all(|(namespace, _)| namespace.is_none()),
        "{names:?}"
    );
    assert!(request.input().iter().any(|item| {
        matches!(item, ModelInputItem::Message(message) if {
            let text = message.text_content();
            text.contains("`to=functions.spawn_agent`") && !text.contains("agents.spawn_agent")
        })
    }));
}

#[tokio::test]
async fn multi_agent_fork_before_first_turn_preserves_model_selected_version() {
    let endpoint = responses_endpoint().await;
    let product = ProductModels::new();
    setup_catalog(&product, &endpoint, "");
    let server = server_with_disk(&product);
    let mut connection = server.connect(1).await;

    for unload_source in [false, true] {
        let (source, _) = start(&server, &mut connection, json!({})).await;
        let first = run_on_model(&server, &mut connection, &source, "catalog-v2").await;
        if unload_source {
            unload(&server, &mut connection, &source).await;
        }
        let fork = server
            .call(
                &mut connection,
                "thread/fork",
                json!({
                    "threadId": source, "beforeTurnId": first, "model": "catalog-v1",
                    "approvalPolicy": "never", "approvalsReviewer": "user", "sandbox": "read-only",
                }),
            )
            .await;
        let child = fork["thread"]["id"].as_str().unwrap();
        assert_eq!(fork["thread"]["turns"], json!([]));
        assert_eq!(
            version_records(&server, child).await,
            [("session_meta".to_owned(), "v2".to_owned())]
        );
        unload(&server, &mut connection, child).await;
        server
            .call(&mut connection, "thread/resume", json!({"threadId": child}))
            .await;
        run_on_model(&server, &mut connection, child, "catalog-v1").await;
        assert_latest_tools(&endpoint, "catalog-v1", true).await;
    }
}

fn developer_texts(body: &Value) -> Vec<&str> {
    body["input"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["role"] == "developer")
        .flat_map(|item| item["content"].as_array().unwrap())
        .filter_map(|content| content["text"].as_str())
        .collect()
}

async fn latest_body(endpoint: &MockServer) -> Value {
    endpoint
        .received_requests()
        .await
        .unwrap()
        .last()
        .unwrap()
        .body_json()
        .unwrap()
}

/// A role text the bundled catalog ships, which carries Codex's stock prose.
fn bundled_role(model: &str, role: &str) -> String {
    let catalog: Value = serde_json::from_str(include_str!(
        "../../../../../crates/ra-coding/src/config/models.json"
    ))
    .unwrap();
    catalog["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["slug"] == model)
        .and_then(|entry| entry["model_messages"]["multi_agent"]["role"][role].as_str())
        .unwrap()
        .to_owned()
}

/// Codex's composed role body: the base kept verbatim, then the shared hint naming the offered
/// tools and their recipient, the `wait_agent` advice when offered, and the concurrency slots.
fn composed_role(base: &str, listed: &str, recipient: &str, wait: bool, slots: usize) -> String {
    let wait = if wait {
        "When calling `wait_agent`, prefer longer waits (minutes) to avoid busy polling.\n\n"
    } else {
        ""
    };
    format!(
        "{base}\nCall {listed} only as direct tool calls using the recipient shown in their tool definitions, such as `to={recipient}`.\n\nAll agents share the same directory. In detail:\n- All agents have access to the same container and filesystem as you.\n- All agents use the same current working directory.\n- As a result, edits made by one agent are immediately visible to all other agents.\n\n{wait}There are {slots} available concurrency slots, meaning that up to {slots} agents can be active at once, including you."
    )
}

#[tokio::test]
async fn multi_agent_role_prompt_follows_the_current_catalog_and_tools() {
    let endpoint = responses_endpoint().await;
    let product = ProductModels::new();
    setup_catalog(
        &product,
        &endpoint,
        r#"
[features.multi_agent_v2]
tool_namespace = "agents"
disable_direct_message = true
wait_agent_enabled = false
max_concurrent_threads_per_session = 2
"#,
    );
    let mut v2 = catalog_model("catalog-v2", Some("v2"));
    let root_role = bundled_role("gpt-6-astra", "root");
    v2["model_messages"] = json!({"multi_agent": {"role": {"root": root_role}}});
    let mut v1 = catalog_model("catalog-v1", Some("v1"));
    v1["model_messages"] = json!({"multi_agent": {"role": {"root": ""}}});
    product.write(
        "collaboration-models.json",
        &json!({"models": [v2, v1]}).to_string(),
    );
    let server = product.server(None);
    let mut connection = server.connect(1).await;
    let (thread, _) = start(&server, &mut connection, json!({})).await;

    run_on_model(&server, &mut connection, &thread, "catalog-v2").await;
    let body = latest_body(&endpoint).await;
    let texts = developer_texts(&body);
    // The catalog's stock prose stays verbatim, as Codex sends it; only the shared hint follows
    // the configured tool table.
    let expected = format!(
        "<multi_agent_role>{}</multi_agent_role>",
        composed_role(
            &root_role,
            "`spawn_agent`, `interrupt_agent`, and `list_agents`",
            "functions.agents.spawn_agent",
            false,
            2,
        )
    );
    assert!(texts.contains(&expected.as_str()), "{texts:?}");
    assert!(
        texts
            .iter()
            .all(|text| !text.contains("functions.exec") && !text.contains("reasoning_effort")),
        "{texts:?}"
    );

    run_on_model(&server, &mut connection, &thread, "catalog-v1").await;
    let body = latest_body(&endpoint).await;
    assert!(developer_texts(&body).iter().all(|text| {
        !text.contains("<multi_agent_role>") && !text.contains("primary agent in a team")
    }));
    assert!(
        latest_collaboration_tools(&endpoint, "agents")
            .await
            .is_some()
    );

    run_on_model(&server, &mut connection, &thread, "outside-catalog").await;
    let body = latest_body(&endpoint).await;
    assert!(developer_texts(&body).iter().any(|text| {
        text.starts_with("You are `/root`, the primary agent")
            && !text.contains("<multi_agent_role>")
    }));
}

#[tokio::test]
async fn multi_agent_role_prompt_respects_verbatim_empty_and_non_root_sources() {
    let endpoint = responses_endpoint().await;
    let product = ProductModels::new();
    for (source, configured, expected, expect_mode) in [
        (
            ra_protocol::server::SessionSource::Mcp,
            "  Configured root role.  ",
            true,
            true,
        ),
        (ra_protocol::server::SessionSource::Mcp, "", false, true),
        (
            ra_protocol::server::SessionSource::SubAgent(
                ra_protocol::server::SubAgentSource::Review,
            ),
            "  Configured root role.  ",
            false,
            false,
        ),
    ] {
        setup_catalog(
            &product,
            &endpoint,
            &format!(
                "[features.multi_agent_v2]\nenabled = true\nroot_agent_usage_hint_text = {configured:?}\n"
            ),
        );
        let mut model = catalog_model("catalog-v2", Some("v2"));
        model["model_messages"] = json!({"multi_agent": {"role": {"root": "Catalog role."}}});
        product.write(
            "collaboration-models.json",
            &json!({"models": [model]}).to_string(),
        );
        let (host, settings, builder) = product.parts();
        let (server, _) = build_with_session_source(
            Options {
                store: Some(Arc::new(InMemoryThreadStore::new())),
                factory: Some(Arc::new(CodingThreadFactory::from_product_config(
                    host, settings, builder,
                ))),
                unload_delay: None,
            },
            None,
            Some(source),
        );
        let mut connection = server.connect(1).await;
        let (thread, _) = start(&server, &mut connection, json!({})).await;
        run_on_model(&server, &mut connection, &thread, "catalog-v2").await;
        let body = latest_body(&endpoint).await;
        let texts = developer_texts(&body);
        assert_eq!(texts.contains(&configured), expected, "{texts:?}");
        assert_eq!(mode_texts(&body).len(), usize::from(expect_mode));
        assert!(texts.iter().all(|text| !text.contains("<multi_agent_role>")
            && !text.contains("primary agent in a team")));
    }
}

#[tokio::test]
async fn multi_agent_spawn_projects_child_role_and_developer_instructions() {
    for (fork_turns, override_line, expected) in [
        (
            "all",
            "subagent_developer_instructions = \"  Child instructions.  \"",
            Some("Child instructions."),
        ),
        ("none", "subagent_developer_instructions = \"   \"", None),
        ("all", "", Some("Parent instructions.")),
    ] {
        let endpoint = responses_endpoint().await;
        let product = ProductModels::new();
        setup_catalog(
            &product,
            &endpoint,
            &format!(
                "[features.multi_agent_v2]\nenabled = true\ndisable_direct_message = true\nroot_agent_usage_hint_text = \"Parent role.\"\n{override_line}"
            ),
        );
        let config = std::fs::read_to_string(product.home.path().join("config.toml")).unwrap();
        product.write(
            "config.toml",
            &format!("developer_instructions = \"Parent instructions.\"\n{config}"),
        );
        let mut model = catalog_model("catalog-v2", Some("v2"));
        model["model_messages"] =
            json!({"multi_agent": {"role": {"subagent": bundled_role("gpt-6-astra", "subagent")}}});
        product.write(
            "collaboration-models.json",
            &json!({"models": [model]}).to_string(),
        );

        let server = product.server(None);
        let mut connection = server.connect(1).await;
        let (thread, _) = start(&server, &mut connection, json!({})).await;
        run_on_model(&server, &mut connection, &thread, "catalog-v2").await;

        let mut response = websocket_completion(0);
        response["response"]["output"] = json!([{
            "id": "spawn", "type": "function_call", "call_id": "spawn-child",
            "namespace": "collaboration", "name": "spawn_agent",
            "arguments": json!({"task_name": "worker", "message": "Child task.", "fork_turns": fork_turns}).to_string()
        }]);
        Mock::given(method("POST"))
            .and(path("/product/v1/responses"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(format!("data: {response}\n\n"), "text/event-stream"),
            )
            .with_priority(1)
            .up_to_n_times(1)
            .expect(1)
            .mount(&endpoint)
            .await;
        run_on_model(&server, &mut connection, &thread, "catalog-v2").await;
        let (parent, child) = timeout(PATIENCE, async {
            loop {
                let requests = endpoint.received_requests().await.unwrap();
                let parent: Value = requests[1].body_json().unwrap();
                if let Some(child) = requests.iter().skip(2).find_map(|request| {
                    let body: Value = request.body_json().unwrap();
                    body["input"]
                        .as_array()?
                        .iter()
                        .any(|item| {
                            item["role"] == "user" && item.to_string().contains("Child task.")
                        })
                        .then_some(body)
                }) {
                    break (parent, child);
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the child samples its task");

        let parent_texts = developer_texts(&parent);
        assert!(parent_texts.contains(&"Parent role."), "{parent_texts:?}");
        assert!(
            parent_texts.contains(&"Parent instructions."),
            "{parent_texts:?}"
        );

        let child_texts = developer_texts(&child);
        // The bundled child prose stays verbatim; the shared hint names only the offered tools.
        let child_role = format!(
            "<multi_agent_role>{}</multi_agent_role>",
            composed_role(
                &bundled_role("gpt-6-astra", "subagent"),
                "`spawn_agent`, `wait_agent`, `interrupt_agent`, and `list_agents`",
                "functions.collaboration.spawn_agent",
                true,
                4,
            )
        );
        assert!(
            child_texts.contains(&child_role.as_str()),
            "{child_texts:?}"
        );
        assert!(!child_texts.contains(&"Parent role."), "{child_texts:?}");
        assert_eq!(mode_texts(&child).len(), 1);
        let actual = child_texts
            .iter()
            .copied()
            .filter(|text| matches!(*text, "Parent instructions." | "Child instructions."))
            .collect::<Vec<_>>();
        assert_eq!(
            actual,
            expected.into_iter().collect::<Vec<_>>(),
            "{child_texts:?}"
        );
        assert!(!child_texts.contains(&""), "{child_texts:?}");
        assert_eq!(
            child["input"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["role"] == "user" && item["content"][0]["text"] == "Hello"),
            fork_turns == "all",
        );
    }
}

fn mode_texts(body: &Value) -> Vec<&str> {
    developer_texts(body)
        .into_iter()
        .filter_map(|text| {
            text.strip_prefix("<multi_agent_mode>")?
                .strip_suffix("</multi_agent_mode>")
        })
        .collect()
}

fn configure_mode_effort(product: &ProductModels, effort: &str) {
    let config = std::fs::read_to_string(product.home.path().join("config.toml")).unwrap();
    product.write(
        "config.toml",
        &format!("model_reasoning_effort = {effort:?}\n{config}"),
    );
}

#[tokio::test]
async fn multi_agent_mode_follows_effective_effort_and_ignores_deprecated_rpc_fields() {
    let endpoint = responses_endpoint().await;
    let product = ProductModels::new();
    setup_catalog(&product, &endpoint, "");
    configure_mode_effort(&product, "ultra");
    let server = product.server(None);
    let mut connection = server.connect(1).await;
    let (thread, _) = start(
        &server,
        &mut connection,
        json!({
            "multiAgentMode": "explicitRequestOnly",
        }),
    )
    .await;
    run_on_model(&server, &mut connection, &thread, "catalog-v2").await;
    let body = latest_body(&endpoint).await;
    assert_eq!(body["reasoning"]["effort"], "medium");
    let texts = mode_texts(&body);
    assert!(
        matches!(texts.as_slice(), [text] if text.starts_with("Proactive multi-agent delegation is active.")),
        "{texts:?}"
    );

    server
        .call(
            &mut connection,
            "thread/settings/update",
            json!({
                "threadId": thread, "multiAgentMode": "proactive",
            }),
        )
        .await;
    server
        .call(
            &mut connection,
            "turn/start",
            json!({
                "threadId": thread, "input": [{"type": "text", "text": "Continue"}],
                "effort": "low", "multiAgentMode": "proactive",
            }),
        )
        .await;
    assert_eq!(
        connection.notification("turn/completed").await["turn"]["status"],
        "completed",
    );
    let body = latest_body(&endpoint).await;
    assert_eq!(body["reasoning"]["effort"], "low");
    let texts = mode_texts(&body);
    assert!(
        matches!(texts.as_slice(), [text] if text.starts_with("Any earlier instruction enabling proactive multi-agent delegation")),
        "{texts:?}"
    );

    let (v1, _) = start(&server, &mut connection, json!({})).await;
    run_on_model(&server, &mut connection, &v1, "catalog-v1").await;
    let body = latest_body(&endpoint).await;
    assert_eq!(body["reasoning"]["effort"], "medium");
    assert!(mode_texts(&body).is_empty());
}

#[tokio::test]
async fn multi_agent_mode_honors_catalog_and_configuration_precedence() {
    let endpoint = responses_endpoint().await;
    let product = ProductModels::new();
    let server = product.server(None);
    let mut connection = server.connect(1).await;

    let mut with_branches = catalog_model("catalog-v2", Some("v2"));
    with_branches["default_reasoning_level"] = json!("ultra");
    with_branches["model_messages"] = json!({"multi_agent": {
        "role": {"root": "First model role."},
        "mode": {"explicit": "Catalog explicit.", "proactive": "Catalog proactive."}
    }});
    let mut with_hint = catalog_model("catalog-hint", Some("v2"));
    with_hint["model_messages"] = json!({"multi_agent": {
        "role": {"root": "Second model role."},
        "mode": {"explicit": "Ignored branch.", "hint_text": "Catalog override."}
    }});
    let mut with_empty = catalog_model("catalog-empty", Some("v2"));
    with_empty["model_messages"] = json!({"multi_agent": {
        "mode": {"explicit": "Ignored branch.", "hint_text": ""}
    }});

    for configured in [None, Some(""), Some("Configured override.")] {
        let feature = configured.map_or_else(String::new, |text| {
            format!("[features.multi_agent_v2]\nmulti_agent_mode_hint_text = {text:?}")
        });
        setup_catalog(&product, &endpoint, &feature);
        product.write(
            "collaboration-models.json",
            &json!({"models": [with_branches, with_hint, with_empty]}).to_string(),
        );
        let (thread, _) = start(&server, &mut connection, json!({})).await;
        for (model, catalog) in [
            ("catalog-v2", "Catalog proactive."),
            ("catalog-hint", "Catalog override."),
            ("catalog-empty", ""),
        ] {
            run_on_model(&server, &mut connection, &thread, model).await;
            let body = latest_body(&endpoint).await;
            let expected = configured.unwrap_or(catalog);
            let expected = (!expected.is_empty())
                .then_some(expected)
                .into_iter()
                .collect::<Vec<_>>();
            assert_eq!(mode_texts(&body), expected, "{model}");
            if model == "catalog-hint" {
                let texts = developer_texts(&body);
                assert!(
                    texts
                        .iter()
                        .any(|text| text.starts_with("<multi_agent_role>Second model role."))
                );
                assert!(!texts.iter().any(|text| text.contains("First model role.")));
            }
        }
    }
}

#[tokio::test]
async fn multi_agent_mode_bounds_custom_text() {
    use ra_core::sandbox::token_truncation::{TruncationPolicy, truncate_text};

    let endpoint = responses_endpoint().await;
    let product = ProductModels::new();
    let text = format!("Start {} end.", "é ".repeat(1500));
    setup_catalog(
        &product,
        &endpoint,
        &format!("[features.multi_agent_v2]\nmulti_agent_mode_hint_text = {text:?}"),
    );
    let server = product.server(None);
    let mut connection = server.connect(1).await;
    let (thread, _) = start(&server, &mut connection, json!({})).await;
    run_on_model(&server, &mut connection, &thread, "catalog-v2").await;
    let body = latest_body(&endpoint).await;
    let truncated = truncate_text(&text, TruncationPolicy::tokens(400));
    assert_ne!(text, truncated);
    assert_eq!(mode_texts(&body), [truncated.as_str()]);
}

/// A spawned child takes its mode from the effort it inherits, as Codex's child configuration
/// carries the parent turn's effort: an Ultra default stays proactive, and a turn that selects a
/// lower effort spawns an explicit-request child.
#[tokio::test]
async fn multi_agent_spawned_child_mode_follows_inherited_effort() {
    for (turn_effort, proactive) in [(None, true), (Some("low"), false)] {
        let endpoint = responses_endpoint().await;
        let product = ProductModels::new();
        setup_catalog(
            &product,
            &endpoint,
            "[features.multi_agent_v2]\nenabled = true\n",
        );
        configure_mode_effort(&product, "ultra");
        let server = product.server(None);
        let mut connection = server.connect(1).await;
        let (thread, _) = start(&server, &mut connection, json!({})).await;

        let mut response = websocket_completion(0);
        response["response"]["output"] = json!([{
            "id": "spawn", "type": "function_call", "call_id": "spawn-child",
            "namespace": "collaboration", "name": "spawn_agent",
            "arguments": json!({"task_name": "worker", "message": "Child task.", "fork_turns": "none"}).to_string()
        }]);
        Mock::given(method("POST"))
            .and(path("/product/v1/responses"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(format!("data: {response}\n\n"), "text/event-stream"),
            )
            .with_priority(1)
            .up_to_n_times(1)
            .expect(1)
            .mount(&endpoint)
            .await;
        let mut params = json!({
            "threadId": thread, "input": [{"type": "text", "text": "Delegate"}],
            "model": "catalog-v2",
        });
        if let Some(effort) = turn_effort {
            params["effort"] = json!(effort);
        }
        server.call(&mut connection, "turn/start", params).await;
        assert_eq!(
            connection.notification("turn/completed").await["turn"]["status"],
            "completed",
        );
        let (parent, child) = timeout(PATIENCE, async {
            loop {
                let requests = endpoint.received_requests().await.unwrap();
                let bodies = requests
                    .iter()
                    .map(|request| request.body_json::<Value>().unwrap())
                    .collect::<Vec<_>>();
                let is_child = |body: &Value| {
                    body["input"].as_array().is_some_and(|items| {
                        items.iter().any(|item| {
                            item["role"] == "user" && item.to_string().contains("Child task.")
                        })
                    })
                };
                if let Some(child) = bodies.iter().find(|body| is_child(body)) {
                    let parent = bodies.iter().find(|body| !is_child(body)).unwrap().clone();
                    break (parent, child.clone());
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the child samples its task");

        let expected = if proactive {
            "Proactive multi-agent delegation is active."
        } else {
            "Any earlier instruction enabling proactive multi-agent delegation"
        };
        for (who, body) in [("parent", &parent), ("child", &child)] {
            let texts = mode_texts(body);
            assert!(
                matches!(texts.as_slice(), [text] if text.starts_with(expected)),
                "{who} {turn_effort:?}: {texts:?}"
            );
        }
    }
}
