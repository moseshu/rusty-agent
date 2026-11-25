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
        assert_v1_tools(&endpoint, version == "v1").await;
        let body = latest_body(&endpoint).await;
        assert_eq!(
            developer_texts(&body)
                .iter()
                .any(|text| text.starts_with("You are `/root`, the primary agent")),
            enabled,
        );
        run_on_model(&server, &mut connection, &thread, next_model).await;
        assert_latest_tools(&endpoint, next_model, enabled).await;
        assert_v1_tools(&endpoint, version == "v1").await;
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
    assert_v1_tools(&endpoint, false).await;
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
    assert_v1_tools(&endpoint, true).await;
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

async fn assert_v1_tools(endpoint: &MockServer, enabled: bool) {
    let tools = latest_collaboration_tools(endpoint, "multi_agent_v1").await;
    assert_eq!(tools.is_some(), enabled);
    if let Some(tools) = tools {
        assert_eq!(
            tools.keys().map(String::as_str).collect::<Vec<_>>(),
            [
                "close_agent",
                "resume_agent",
                "send_input",
                "spawn_agent",
                "wait_agent"
            ]
        );
        assert_eq!(
            tools["wait_agent"]["parameters"]["required"],
            json!(["targets"])
        );
        assert!(
            tools["spawn_agent"]["parameters"]["properties"]
                .get("fork_context")
                .is_some()
        );
        let description = tools["spawn_agent"]["description"].as_str().unwrap();
        assert!(description.contains(
            "Available model overrides (optional; inherited parent model is preferred):"
        ));
        assert!(description.contains("Do not spawn sub-agents unless the user or applicable AGENTS.md/skill instructions explicitly ask"));
        assert!(
            tools["spawn_agent"]["parameters"]["properties"]
                .get("agent_type")
                .is_none()
        );
        assert_eq!(
            tools["send_input"]["description"],
            "Send a message to an existing agent. Use interrupt=true to redirect work immediately. You should reuse the agent by send_input if you believe your assigned task is highly dependent on the context of a previous task."
        );
        let fields = &tools["send_input"]["parameters"]["properties"];
        assert_eq!(
            fields["target"]["description"],
            "Agent id to message (from spawn_agent)."
        );
        assert_eq!(fields["items"]["items"]["type"], "object");
        assert_eq!(
            fields["items"]["items"]["properties"]["type"]["type"],
            "string"
        );
        assert!(
            fields["items"]["items"]["properties"]
                .get("audio_url")
                .is_some()
        );
    }
}

async fn v1_call(
    endpoint: &MockServer,
    server: &Server,
    connection: &mut Connection,
    thread: &str,
    name: &str,
    arguments: Value,
) -> String {
    let call_id = format!(
        "call-{name}-{}",
        endpoint.received_requests().await.unwrap().len()
    );
    let mut response = websocket_completion(0);
    response["response"]["output"] = json!([{
        "id": call_id, "type": "function_call", "call_id": call_id,
        "namespace": "multi_agent_v1", "name": name, "arguments": arguments.to_string()
    }]);
    let prompt = call_id.clone();
    Mock::given(method("POST"))
        .and(path("/product/v1/responses"))
        .and(move |request: &wiremock::Request| {
            let body: Value = request.body_json().unwrap();
            body["input"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["role"] == "user" && item["content"][0]["text"] == prompt)
        })
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(format!("data: {response}\n\n"), "text/event-stream"),
        )
        .with_priority(1)
        .up_to_n_times(1)
        .expect(1)
        .mount(endpoint)
        .await;
    server
        .call(
            connection,
            "turn/start",
            serde_json::to_value(
                TurnStartParams::new(thread, vec![UserInput::text(&call_id)])
                    .with_model("catalog-v1"),
            )
            .unwrap(),
        )
        .await;
    let completed = connection.notification("turn/completed").await;
    assert_eq!(completed["turn"]["status"], "completed", "{completed}");
    let requests = endpoint.received_requests().await.unwrap();
    requests
        .iter()
        .rev()
        .find_map(|request| {
            let body: Value = request.body_json().ok()?;
            let output = body["input"].as_array()?.iter().find(|item| {
                item["type"] == "function_call_output" && item["call_id"] == call_id
            })?;
            output["output"][0]["text"].as_str().map(str::to_owned)
        })
        .unwrap_or_else(|| {
            let last_inputs: Vec<_> = requests
                .iter()
                .rev()
                .take(3)
                .map(|request| {
                    let body: Value = request.body_json().unwrap();
                    body["input"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .rev()
                        .take(4)
                        .cloned()
                        .collect::<Vec<_>>()
                })
                .collect();
            panic!("missing V1 result for {call_id}. Recent inputs: {last_inputs:?}")
        })
}

#[tokio::test]
async fn multi_agent_v1_resident_limit_and_closed_thread_resume() {
    let endpoint = responses_endpoint().await;
    let product = ProductModels::new();
    setup_catalog(
        &product,
        &endpoint,
        "[agents]\nmax_concurrent_threads_per_session = 1",
    );
    let server = server_with_disk(&product);
    let mut connection = server.connect(1).await;
    let (thread, _) = start(&server, &mut connection, json!({})).await;

    let spawned = v1_call(
        &endpoint,
        &server,
        &mut connection,
        &thread,
        "spawn_agent",
        json!({"items": [{"type": "text", "text": "V1 original task"}]}),
    )
    .await;
    let spawned: Value =
        serde_json::from_str(&spawned).unwrap_or_else(|error| panic!("{error}: {spawned}"));
    let child = spawned["agent_id"].as_str().unwrap();
    assert_eq!(spawned["nickname"], Value::Null);
    let waited = v1_call(
        &endpoint,
        &server,
        &mut connection,
        &thread,
        "wait_agent",
        json!({"targets": [child]}),
    )
    .await;
    assert_eq!(
        serde_json::from_str::<Value>(&waited).unwrap(),
        json!({
            "status": { (child): {"completed": "Done"} }, "timed_out": false
        })
    );

    let requests = endpoint.received_requests().await.unwrap();
    let notification = format!(
        "<subagent_notification>\n{}\n</subagent_notification>",
        json!({"agent_path": child, "status": {"completed": "Done"}})
    );
    let bodies: Vec<Value> = requests
        .iter()
        .map(|request| request.body_json().unwrap())
        .collect();
    assert!(
        bodies
            .iter()
            .flat_map(|body| body["input"].as_array().unwrap())
            .any(|item| {
                item["role"] == "user"
                    && item["content"].as_array().is_some_and(|blocks| {
                        blocks.iter().any(|block| block["text"] == notification)
                    })
            })
    );
    assert!(!bodies.iter().any(|body| {
        body["input"]
            .to_string()
            .contains("Message Type: FINAL_ANSWER")
    }));
    let child_body = requests
        .iter()
        .find_map(|request| {
            let body: Value = request.body_json().ok()?;
            body["input"]
                .as_array()?
                .iter()
                .any(|item| {
                    item["role"] == "user" && item["content"][0]["text"] == "V1 original task"
                })
                .then_some(body)
        })
        .expect("the child takes the original task");
    assert!(
        child_body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .all(|tool| { tool["name"] != "multi_agent_v1" && tool["name"] != "collaboration" })
    );
    assert_eq!(
        version_records(&server, child).await.last().unwrap().1,
        "v1"
    );

    let refused = v1_call(
        &endpoint,
        &server,
        &mut connection,
        &thread,
        "spawn_agent",
        json!({"message": "Extra task"}),
    )
    .await;
    assert!(refused.contains("agent thread limit reached"), "{refused}");

    let closed = v1_call(
        &endpoint,
        &server,
        &mut connection,
        &thread,
        "close_agent",
        json!({"target": child}),
    )
    .await;
    assert_eq!(
        serde_json::from_str::<Value>(&closed).unwrap(),
        json!({"previous_status": {"completed": "Done"}})
    );
    unload(&server, &mut connection, &thread).await;
    server
        .call(
            &mut connection,
            "thread/resume",
            json!({"threadId": thread}),
        )
        .await;

    // As in Codex, a resume whose spawn edge cannot be written still succeeds. The edge stays
    // closed, so a later cold resume does not bring the agent back.
    let edges = product
        .home
        .path()
        .join("rollouts")
        .join("thread_spawn_edges.jsonl");
    let kept = edges.with_extension("jsonl.kept");
    std::fs::rename(&edges, &kept).unwrap();
    std::fs::create_dir(&edges).unwrap();
    let resumed = v1_call(
        &endpoint,
        &server,
        &mut connection,
        &thread,
        "resume_agent",
        json!({"id": child}),
    )
    .await;
    std::fs::remove_dir(&edges).unwrap();
    std::fs::rename(&kept, &edges).unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&resumed)
            .unwrap_or_else(|error| panic!("{error}: {resumed}")),
        json!({"status": "pending_init"})
    );
    let sent = v1_call(
        &endpoint,
        &server,
        &mut connection,
        &thread,
        "send_input",
        json!({"target": child, "message": "V1 next task"}),
    )
    .await;
    assert!(serde_json::from_str::<Value>(&sent).unwrap()["submission_id"].is_string());
    let waited = v1_call(
        &endpoint,
        &server,
        &mut connection,
        &thread,
        "wait_agent",
        json!({"targets": [child]}),
    )
    .await;
    assert_eq!(
        serde_json::from_str::<Value>(&waited).unwrap()["timed_out"],
        false
    );
    let requests = endpoint.received_requests().await.unwrap();
    assert!(
        requests.iter().any(|request| {
            let body: Value = request.body_json().unwrap();
            let text: Vec<_> = body["input"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|item| item["role"] == "user")
                .filter_map(|item| item["content"][0]["text"].as_str())
                .collect();
            text.contains(&"V1 original task") && text.contains(&"V1 next task")
        }),
        "resumed agent kept its history"
    );

    unload(&server, &mut connection, &thread).await;
    server
        .call(
            &mut connection,
            "thread/resume",
            json!({"threadId": thread}),
        )
        .await;
    let unrestored = v1_call(
        &endpoint,
        &server,
        &mut connection,
        &thread,
        "wait_agent",
        json!({"targets": [child]}),
    )
    .await;
    assert_eq!(
        serde_json::from_str::<Value>(&unrestored).unwrap(),
        json!({"status": {(child): "not_found"}, "timed_out": false})
    );
}

#[tokio::test]
async fn multi_agent_v1_spawn_validates_overrides_and_applies_role_last() {
    let endpoint = responses_endpoint().await;
    let product = ProductModels::new();
    setup_catalog(
        &product,
        &endpoint,
        r#"
[features.multi_agent_v2]
enabled = false
usage_hint_text = "Delegate only after planning."
[agents.worker]
description = "Runs the configured worker model."
config_file = "worker.toml"
[agents.worker_external]
description = "Runs a model outside the catalog."
config_file = "worker_external.toml"
"#,
    );
    let config = std::fs::read_to_string(product.home.path().join("config.toml")).unwrap();
    product.write(
        "config.toml",
        &format!("model_reasoning_effort = \"high\"\n{config}"),
    );
    product.write(
        "worker.toml",
        "model = \"catalog-unspecified\"\nmodel_reasoning_effort = \"medium\"\n",
    );
    product.write(
        "worker_external.toml",
        "model = \"outside-catalog\"\nmodel_reasoning_effort = \"high\"\n",
    );
    let mut models = ["catalog-v1", "catalog-v2", "catalog-unspecified"]
        .into_iter()
        .map(|model| catalog_model(model, Some("v1")))
        .collect::<Vec<_>>();
    for model in &mut models {
        model["supported_reasoning_levels"] = json!([
            {"effort":"low", "description":"Low"},
            {"effort":"medium", "description":"Medium"},
            {"effort":"high", "description":"High"}
        ]);
    }
    product.write(
        "collaboration-models.json",
        &json!({"models": models}).to_string(),
    );

    let server = server_with_disk(&product);
    let mut connection = server.connect(1).await;
    let (thread, _) = start(&server, &mut connection, json!({})).await;
    run_on_model(&server, &mut connection, &thread, "catalog-v1").await;
    let tools = latest_collaboration_tools(&endpoint, "multi_agent_v1")
        .await
        .unwrap();
    let description = tools["spawn_agent"]["description"].as_str().unwrap();
    assert!(description.contains("- `catalog-v2`:"));
    assert!(description.contains("Reasoning efforts: low (default), medium, high."));
    assert!(description.ends_with("Delegate only after planning."));
    assert!(!description.contains("Do not spawn sub-agents"));
    let roles = tools["spawn_agent"]["parameters"]["properties"]["agent_type"]["description"]
        .as_str()
        .unwrap();
    assert!(roles.starts_with("Agent type override for the new agent. Omit to inherit the parent agent type with a full-history fork; otherwise, `default` is used.\nAvailable roles:\nworker: {\nRuns the configured worker model."));
    assert!(roles.contains("This role's model is set to `catalog-unspecified` and its reasoning effort is set to `medium`. These settings cannot be changed."));
    assert!(roles.contains("default: {\nDefault agent.\n}"));

    for (args, expected) in [
        (
            json!({"message":"Unknown model task", "model":"missing"}),
            "Unknown model `missing` for spawn_agent. Available models: catalog-v1, catalog-v2, catalog-unspecified",
        ),
        (
            json!({"message":"Unsupported effort task", "model":"catalog-v2", "reasoning_effort":"ultra"}),
            "Reasoning effort `ultra` is not supported for model `catalog-v2`. Supported reasoning efforts: low, medium, high",
        ),
    ] {
        let refusal = v1_call(
            &endpoint,
            &server,
            &mut connection,
            &thread,
            "spawn_agent",
            args,
        )
        .await;
        assert_eq!(refusal, expected);
    }
    for (prompt, args, model, effort) in [
        (
            "Fresh model task",
            json!({"model":"catalog-v2"}),
            "catalog-v2",
            "low",
        ),
        (
            "Role priority task",
            json!({"model":"catalog-v2","reasoning_effort":"high","agent_type":"worker"}),
            "catalog-unspecified",
            "medium",
        ),
        // Codex validates a role's effort only against catalog metadata, so a role may name a
        // model the catalog does not list.
        (
            "Uncataloged role task",
            json!({"agent_type":"worker_external"}),
            "outside-catalog",
            "high",
        ),
    ] {
        let mut args = args;
        args["message"] = json!(prompt);
        let output = v1_call(
            &endpoint,
            &server,
            &mut connection,
            &thread,
            "spawn_agent",
            args,
        )
        .await;
        let child: Value =
            serde_json::from_str(&output).unwrap_or_else(|error| panic!("{error}: {output}"));
        v1_call(
            &endpoint,
            &server,
            &mut connection,
            &thread,
            "wait_agent",
            json!({"targets":[child["agent_id"]]}),
        )
        .await;
        let body: Value = endpoint
            .received_requests()
            .await
            .unwrap()
            .iter()
            .find_map(|request| {
                let body: Value = request.body_json().ok()?;
                body["input"]
                    .as_array()?
                    .iter()
                    .any(|item| item["role"] == "user" && item["content"][0]["text"] == prompt)
                    .then_some(body)
            })
            .expect("valid spawn made a child model request");
        assert_eq!(body["model"], model);
        assert_eq!(body["reasoning"]["effort"], effort);
    }

    let config = std::fs::read_to_string(product.home.path().join("config.toml")).unwrap();
    product.write(
        "config.toml",
        &format!(
            "{config}\n[agents]\ndefault_subagent_reasoning_effort = \"unsupported-default\"\n"
        ),
    );
    let (default_thread, _) = start(&server, &mut connection, json!({})).await;
    let refused = v1_call(
        &endpoint,
        &server,
        &mut connection,
        &default_thread,
        "spawn_agent",
        json!({"message": "Invalid configured default task"}),
    )
    .await;
    assert_eq!(
        refused,
        "Reasoning effort `unsupported-default` is not supported for model `catalog-v1`. Supported reasoning efforts: low, medium, high"
    );
}

#[tokio::test]
async fn multi_agent_v1_close_and_resume_restores_only_open_descendants() {
    let endpoint = responses_endpoint().await;
    let product = ProductModels::new();
    setup_catalog(&product, &endpoint, "[agents]\nmax_depth = 2");
    let server = server_with_disk(&product);
    let mut connection = server.connect(1).await;
    let (thread, _) = start(&server, &mut connection, json!({})).await;

    let mut response = websocket_completion(0);
    response["response"]["output"] = json!([{
        "id": "descendant", "type": "function_call", "call_id": "descendant",
        "namespace": "multi_agent_v1", "name": "spawn_agent",
        "arguments": json!({"message": "V1 leaf"}).to_string(),
    }]);
    Mock::given(method("POST"))
        .and(|request: &wiremock::Request| {
            let body: Value = request.body_json().unwrap();
            body["input"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["role"] == "user" && item["content"][0]["text"] == "V1 branch")
        })
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(format!("data: {response}\n\n"), "text/event-stream"),
        )
        .with_priority(2)
        .up_to_n_times(1)
        .expect(1)
        .mount(&endpoint)
        .await;

    let (endpoint, server, thread) = (&endpoint, &server, thread.as_str());
    let branch = v1_call(
        endpoint,
        server,
        &mut connection,
        thread,
        "spawn_agent",
        json!({"message": "V1 branch"}),
    )
    .await;
    let branch: Value = serde_json::from_str(&branch).unwrap();
    let branch = branch["agent_id"].as_str().unwrap();
    v1_call(
        endpoint,
        server,
        &mut connection,
        thread,
        "wait_agent",
        json!({"targets": [branch]}),
    )
    .await;
    let requests = endpoint.received_requests().await.unwrap();
    let leaf: Value = requests
        .iter()
        .find_map(|request| {
            let body: Value = request.body_json().ok()?;
            let output = body["input"].as_array()?.iter().find(|item| {
                item["type"] == "function_call_output" && item["call_id"] == "descendant"
            })?;
            serde_json::from_str(output["output"][0]["text"].as_str()?).ok()
        })
        .expect("the branch spawns a leaf within the configured depth");
    let leaf = leaf["agent_id"].as_str().unwrap();

    for permanently_closed in [false, true] {
        if permanently_closed {
            let result = v1_call(
                endpoint,
                server,
                &mut connection,
                thread,
                "close_agent",
                json!({"target": leaf}),
            )
            .await;
            assert!(
                serde_json::from_str::<Value>(&result)
                    .unwrap()
                    .get("previous_status")
                    .is_some()
            );
        } else {
            // An ordinary thread unload shuts down its agents. The open edges are recovered on
            // the first collaboration operation after cold resume.
            unload(server, &mut connection, thread).await;
            setup_catalog(&product, endpoint, "[agents]\nmax_depth = 1");
            server
                .call(
                    &mut connection,
                    "thread/resume",
                    json!({"threadId": thread}),
                )
                .await;
        }
        let closed = v1_call(
            endpoint,
            server,
            &mut connection,
            thread,
            "close_agent",
            json!({"target": branch}),
        )
        .await;
        assert!(
            serde_json::from_str::<Value>(&closed)
                .unwrap()
                .get("previous_status")
                .is_some()
        );
        let resumed = v1_call(
            endpoint,
            server,
            &mut connection,
            thread,
            "resume_agent",
            json!({"id": branch}),
        )
        .await;
        assert_eq!(
            serde_json::from_str::<Value>(&resumed).unwrap()["status"],
            "pending_init"
        );
        let sent = v1_call(
            endpoint,
            server,
            &mut connection,
            thread,
            "send_input",
            json!({"target": leaf, "message": "Follow the open edge"}),
        )
        .await;
        if permanently_closed {
            assert!(sent.contains("not found"), "{sent}");
        } else {
            assert!(
                serde_json::from_str::<Value>(&sent).unwrap()["submission_id"].is_string(),
                "{sent}"
            );
            v1_call(
                endpoint,
                server,
                &mut connection,
                thread,
                "wait_agent",
                json!({"targets": [leaf]}),
            )
            .await;
        }
    }
}

/// Resuming a descendant by ID moves its spawn edge under the caller, as Codex's resume upserts
/// the edge from the caller's spawn source, so a cold resume restores it below the caller even
/// though the agent that spawned it stays closed.
#[tokio::test]
async fn multi_agent_v1_resumed_descendant_is_restored_under_its_new_parent() {
    let endpoint = responses_endpoint().await;
    let product = ProductModels::new();
    setup_catalog(&product, &endpoint, "[agents]\nmax_depth = 2");
    let server = server_with_disk(&product);
    let mut connection = server.connect(1).await;
    let (thread, _) = start(&server, &mut connection, json!({})).await;

    let mut response = websocket_completion(0);
    response["response"]["output"] = json!([{
        "id": "adopted", "type": "function_call", "call_id": "adopted",
        "namespace": "multi_agent_v1", "name": "spawn_agent",
        "arguments": json!({"message": "V1 adopted leaf"}).to_string(),
    }]);
    Mock::given(method("POST"))
        .and(|request: &wiremock::Request| {
            let body: Value = request.body_json().unwrap();
            body["input"].as_array().unwrap().iter().any(|item| {
                item["role"] == "user" && item["content"][0]["text"] == "V1 adopting branch"
            })
        })
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(format!("data: {response}\n\n"), "text/event-stream"),
        )
        .with_priority(2)
        .up_to_n_times(1)
        .expect(1)
        .mount(&endpoint)
        .await;

    let (endpoint, server, thread) = (&endpoint, &server, thread.as_str());
    let call = |tool: &'static str, args: Value| (tool, args);
    let mut run = async |(tool, args): (&str, Value)| {
        v1_call(endpoint, server, &mut connection, thread, tool, args).await
    };
    let branch = run(call(
        "spawn_agent",
        json!({"message": "V1 adopting branch"}),
    ))
    .await;
    let branch: Value = serde_json::from_str(&branch).unwrap();
    let branch = branch["agent_id"].as_str().unwrap().to_owned();
    run(call("wait_agent", json!({"targets": [branch]}))).await;
    let leaf: Value = endpoint
        .received_requests()
        .await
        .unwrap()
        .iter()
        .find_map(|request| {
            let body: Value = request.body_json().ok()?;
            let output = body["input"].as_array()?.iter().find(|item| {
                item["type"] == "function_call_output" && item["call_id"] == "adopted"
            })?;
            serde_json::from_str(output["output"][0]["text"].as_str()?).ok()
        })
        .expect("the branch spawns a leaf within the configured depth");
    let leaf = leaf["agent_id"].as_str().unwrap().to_owned();

    // Closing the branch also stops the leaf, whose own edge stays open below the branch.
    let closed = run(call("close_agent", json!({"target": branch}))).await;
    assert!(
        serde_json::from_str::<Value>(&closed)
            .unwrap()
            .get("previous_status")
            .is_some(),
        "{closed}"
    );
    let resumed = run(call("resume_agent", json!({"id": leaf}))).await;
    assert_eq!(
        serde_json::from_str::<Value>(&resumed).unwrap()["status"],
        "pending_init",
        "{resumed}"
    );
    drop(run);

    unload(server, &mut connection, thread).await;
    for filter in ["parentThreadId", "ancestorThreadId"] {
        let current = server
            .call(&mut connection, "thread/list", json!({(filter): thread}))
            .await;
        assert!(
            current["data"]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| { entry["id"] == leaf && entry["parentThreadId"] == thread })
        );
        let former = server
            .call(&mut connection, "thread/list", json!({(filter): branch}))
            .await;
        assert!(former["data"].as_array().unwrap().is_empty());
    }
    server
        .call(
            &mut connection,
            "thread/archive",
            json!({"threadId": branch}),
        )
        .await;
    let leaf_read = ReadThreadParams::new(SessionId::new(&leaf));
    let branch_read = ReadThreadParams::new(SessionId::new(&branch));
    assert!(server.store.read_thread(&branch_read).await.is_err());
    let branch_read = branch_read.including_archived();
    server.store.read_thread(&branch_read).await.unwrap();
    server.store.read_thread(&leaf_read).await.unwrap();
    server
        .call(
            &mut connection,
            "thread/delete",
            json!({"threadId": branch}),
        )
        .await;
    assert!(server.store.read_thread(&branch_read).await.is_err());
    server.store.read_thread(&leaf_read).await.unwrap();
    server
        .call(
            &mut connection,
            "thread/resume",
            json!({"threadId": thread}),
        )
        .await;
    let sent = v1_call(
        endpoint,
        server,
        &mut connection,
        thread,
        "send_input",
        json!({"target": leaf, "message": "Reach the adopted leaf"}),
    )
    .await;
    assert!(
        serde_json::from_str::<Value>(&sent)
            .ok()
            .is_some_and(|sent| sent["submission_id"].is_string()),
        "the leaf is restored below the root: {sent}"
    );
    let branch_status = v1_call(
        endpoint,
        server,
        &mut connection,
        thread,
        "wait_agent",
        json!({"targets": [branch]}),
    )
    .await;
    assert_eq!(
        serde_json::from_str::<Value>(&branch_status).unwrap(),
        json!({"status": {(branch): "not_found"}, "timed_out": false})
    );
}

#[tokio::test]
async fn multi_agent_v1_send_input_interrupt_redirects_the_running_agent() {
    let endpoint = responses_endpoint().await;
    let product = ProductModels::new();
    setup_catalog(&product, &endpoint, "");
    let server = product.server(None);
    let mut connection = server.connect(1).await;
    let (thread, _) = start(&server, &mut connection, json!({})).await;

    Mock::given(method("POST"))
        .and(|request: &wiremock::Request| {
            let body: Value = request.body_json().unwrap();
            body["input"]
                .as_array()
                .unwrap()
                .iter()
                .rev()
                .find(|item| item["role"] == "user")
                .is_some_and(|item| item["content"][0]["text"] == "V1 long-running task")
        })
        .respond_with(ResponseTemplate::new(500).set_delay(Duration::from_secs(30)))
        .with_priority(2)
        .up_to_n_times(1)
        .expect(1)
        .mount(&endpoint)
        .await;

    let (endpoint, server, thread) = (&endpoint, &server, thread.as_str());
    let spawned = v1_call(
        endpoint,
        server,
        &mut connection,
        thread,
        "spawn_agent",
        json!({"message": "V1 long-running task"}),
    )
    .await;
    let spawned: Value = serde_json::from_str(&spawned).unwrap();
    let child = spawned["agent_id"].as_str().unwrap();
    timeout(PATIENCE, async {
        loop {
            if endpoint
                .received_requests()
                .await
                .unwrap()
                .iter()
                .any(|request| {
                    let body: Value = request.body_json().unwrap();
                    body["input"].as_array().unwrap().iter().any(|item| {
                        item["role"] == "user"
                            && item["content"][0]["text"] == "V1 long-running task"
                    })
                })
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the child starts its original request");
    let sent = v1_call(endpoint, server, &mut connection, thread, "send_input", json!({
        "target": child, "items": [{"type": "text", "text": "V1 replacement task"}], "interrupt": true
    })).await;
    assert!(serde_json::from_str::<Value>(&sent).unwrap()["submission_id"].is_string());
    let waited = v1_call(
        endpoint,
        server,
        &mut connection,
        thread,
        "wait_agent",
        json!({"targets": [child]}),
    )
    .await;
    assert_eq!(
        serde_json::from_str::<Value>(&waited).unwrap(),
        json!({
            "status": { (child): {"completed": "Done"} }, "timed_out": false
        })
    );
    assert!(
        endpoint
            .received_requests()
            .await
            .unwrap()
            .iter()
            .any(|request| {
                let body: Value = request.body_json().unwrap();
                body["input"].as_array().unwrap().iter().any(|item| {
                    item["role"] == "user" && item["content"][0]["text"] == "V1 replacement task"
                })
            })
    );
}

#[tokio::test]
async fn multi_agent_v1_depth_zero_omits_tools_on_first_sample() {
    let endpoint = responses_endpoint().await;
    let product = ProductModels::new();
    setup_catalog(&product, &endpoint, "[agents]\nmax_depth = 0");
    let server = product.server(None);
    let mut connection = server.connect(1).await;
    let (thread, _) = start(&server, &mut connection, json!({})).await;
    run_on_model(&server, &mut connection, &thread, "catalog-v1").await;
    assert_v1_tools(&endpoint, false).await;
    assert_eq!(
        version_records(&server, &thread).await.last().unwrap().1,
        "v1"
    );
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
