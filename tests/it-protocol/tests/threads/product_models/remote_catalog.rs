//! Tests for product model catalog discovery and refresh through real HTTP.

use super::*;
use wiremock::matchers::{header, query_param};

fn configure_catalog(
    product: &ProductModels,
    inference_url: &str,
    catalog_url: &str,
    token: Option<&str>,
    root_settings: &str,
) {
    let bearer = token.map_or_else(String::new, |token| {
        format!("experimental_bearer_token = {token:?}")
    });
    product.write(
        "config.toml",
        &format!(
            r#"
approval_policy = "never"
sandbox_mode = "read-only"
model_provider = "gateway"
{root_settings}
[model_providers.gateway]
name = "Gateway"
base_url = "{inference_url}/product/v1"
model_catalog_url = "{catalog_url}"
{bearer}
request_max_retries = 0
[model_providers.gateway.query_params]
api-version = "test version"
[model_providers.gateway.http_headers]
x-provider-header = "custom"
"#
        ),
    );
}

fn catalog_model(slug: &str, priority: u32, effort: &str) -> Value {
    json!({
        "slug": slug, "display_name": slug, "default_reasoning_level": effort,
        "supported_reasoning_levels": [], "shell_type": "unified_exec", "visibility": "list",
        "supported_in_api": true, "priority": priority, "support_verbosity": true,
        "default_verbosity": "medium", "truncation_policy": {"mode": "tokens", "limit": 1000},
        "experimental_supported_tools": [], "context_window": 8000,
        "model_messages": {"instructions_template": format!("Instructions for {slug}")}
    })
}

fn catalog_response(etag: &str, models: Vec<Value>) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("etag", etag)
        .set_body_json(json!({"models": models}))
}

async fn mount_sse(endpoint: &MockServer, etag: &str) {
    Mock::given(method("POST"))
        .and(path("/product/v1/responses"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("x-models-etag", etag)
                .set_body_raw(
                    format!("data: {}\n\n", websocket_completion(1)),
                    "text/event-stream",
                ),
        )
        .mount(endpoint)
        .await;
}

#[tokio::test]
async fn remote_catalog_uses_fresh_cache_and_refreshes_on_responses_etag() {
    let endpoint = MockServer::start().await;
    let product = ProductModels::new();
    configure_catalog(
        &product,
        &endpoint.uri(),
        &format!("{}/catalog?deployment=one", endpoint.uri()),
        Some("catalog-key"),
        "",
    );
    for (priority, etag, models) in [
        (
            1,
            "\"v1\"",
            vec![
                catalog_model("remote-old", 1, "low"),
                catalog_model("other", 2, "high"),
            ],
        ),
        (
            2,
            "\"v2\"",
            vec![
                catalog_model("remote-new", 1, "medium"),
                catalog_model("remote-old", 2, "high"),
            ],
        ),
    ] {
        Mock::given(method("GET"))
            .and(path("/catalog"))
            .and(header("authorization", "Bearer catalog-key"))
            .and(header("x-provider-header", "custom"))
            .and(query_param("deployment", "one"))
            .and(query_param("api-version", "test version"))
            .and(query_param("client_version", "0.3.0"))
            .respond_with(catalog_response(etag, models))
            .with_priority(priority)
            .up_to_n_times(1)
            .expect(1)
            .mount(&endpoint)
            .await;
    }
    mount_sse(&endpoint, "\"v2\"").await;

    let server = product.server(None);
    let mut connection = server.connect(1).await;
    let (first, started) = start(&server, &mut connection, json!({})).await;
    assert_eq!(started["model"], "remote-old");
    // A fresh catalog can be loaded by a new server from the product home.
    let restarted = product.server(None);
    let mut another_connection = restarted.connect(1).await;
    let (cached_thread, cached) = start(&restarted, &mut another_connection, json!({})).await;
    assert_eq!(cached["model"], "remote-old");
    assert_eq!(endpoint.received_requests().await.unwrap().len(), 1);

    for prompt in ["initial catalog", "refreshed metadata"] {
        let result = completed_turn(&server, &mut connection, &first, prompt).await;
        assert_eq!(result["turn"]["status"], "completed", "{result}");
    }
    let requests = endpoint.received_requests().await.unwrap();
    assert_eq!(requests.len(), 4);
    let initial: Value = requests[1].body_json().unwrap();
    let refreshed: Value = requests[3].body_json().unwrap();
    assert_eq!(initial["model"], "remote-old");
    assert_eq!(initial["reasoning"]["effort"], "low");
    assert_developer_prefix(&initial, "Instructions for remote-old");
    assert_eq!(refreshed["model"], "remote-old");
    assert_eq!(refreshed["reasoning"]["effort"], "high");
    let (_, started) = start(&server, &mut connection, json!({})).await;
    assert_eq!(started["model"], "remote-new");
    let result = completed_turn(
        &restarted,
        &mut another_connection,
        &cached_thread,
        "Reuse the matching disk ETag",
    )
    .await;
    assert_eq!(result["turn"]["status"], "completed", "{result}");
    assert_eq!(endpoint.received_requests().await.unwrap().len(), 5);
    endpoint.verify().await;
}

#[tokio::test]
async fn remote_catalog_failure_clears_metadata_and_keeps_an_explicit_model_usable() {
    let endpoint = MockServer::start().await;
    let product = ProductModels::new();
    configure_catalog(
        &product,
        &endpoint.uri(),
        &format!("{}/catalog", endpoint.uri()),
        Some("catalog-key"),
        "",
    );
    Mock::given(method("GET"))
        .and(path("/catalog"))
        .respond_with(catalog_response(
            "\"v1\"",
            vec![catalog_model("remote-old", 1, "high")],
        ))
        .up_to_n_times(1)
        .expect(1)
        .with_priority(1)
        .mount(&endpoint)
        .await;
    Mock::given(method("GET"))
        .and(path("/catalog"))
        .respond_with(ResponseTemplate::new(503))
        .with_priority(2)
        .mount(&endpoint)
        .await;
    mount_sse(&endpoint, "\"unavailable\"").await;

    let server = product.server(None);
    let mut connection = server.connect(1).await;
    let (thread, _) = start(&server, &mut connection, json!({})).await;
    for prompt in ["trigger failed refresh", "use fallback metadata"] {
        let result = completed_turn(&server, &mut connection, &thread, prompt).await;
        assert_eq!(result["turn"]["status"], "completed", "{result}");
    }

    let requests = endpoint.received_requests().await.unwrap();
    let last: Value = requests
        .iter()
        .rev()
        .find(|request| request.method.as_str() == "POST")
        .unwrap()
        .body_json()
        .unwrap();
    assert_eq!(last["model"], "remote-old");
    assert!(last["reasoning"].get("effort").is_none());
    let error = server
        .call_error(&mut connection, "thread/start", json!({}))
        .await;
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .contains("No models are available"),
        "{error}"
    );
    let (_, started) = start(&server, &mut connection, json!({"model": "manual-model"})).await;
    assert_eq!(started["model"], "manual-model");
}

#[tokio::test]
async fn remote_catalog_cache_is_bound_to_the_effective_credential() {
    let endpoint = MockServer::start().await;
    let product = ProductModels::new();
    for token in ["first-key", "second-key"] {
        Mock::given(method("GET"))
            .and(path("/catalog"))
            .and(header("authorization", format!("Bearer {token}")))
            .respond_with(catalog_response(
                "revision",
                vec![catalog_model(token, 1, "high")],
            ))
            .expect(1)
            .mount(&endpoint)
            .await;
        configure_catalog(
            &product,
            &endpoint.uri(),
            &format!("{}/catalog", endpoint.uri()),
            Some(token),
            "",
        );
        let server = product.server(None);
        let mut connection = server.connect(1).await;
        let (_, started) = start(&server, &mut connection, json!({})).await;
        assert_eq!(started["model"], token);
    }
}

#[tokio::test]
async fn configured_local_catalog_takes_precedence_and_feature_opt_out_skips_discovery() {
    let endpoint = MockServer::start().await;
    let product = ProductModels::new();
    product.write(
        "product-models.json",
        &json!({"models": [catalog_model("local-choice", 1, "low")]}).to_string(),
    );
    let url = format!("{}/catalog", endpoint.uri());
    configure_catalog(
        &product,
        &endpoint.uri(),
        &url,
        Some("catalog-key"),
        &format!(
            "model_catalog_json = {:?}",
            product.home.path().join("product-models.json")
        ),
    );
    let server = product.server(None);
    let mut connection = server.connect(1).await;
    let (_, started) = start(&server, &mut connection, json!({})).await;
    assert_eq!(started["model"], "local-choice");

    configure_catalog(
        &product,
        &endpoint.uri(),
        &url,
        Some("catalog-key"),
        "model = 'explicit-model'\n[features]\napi_key_model_discovery = false",
    );
    let server = product.server(None);
    let mut connection = server.connect(1).await;
    let (_, started) = start(&server, &mut connection, json!({})).await;
    assert_eq!(started["model"], "explicit-model");
    assert!(endpoint.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn configured_remote_catalog_rejects_duplicate_slugs_oversize_and_redirects() {
    let other = MockServer::start().await;
    for response in [
        catalog_response(
            "duplicate",
            vec![
                catalog_model("duplicate", 1, "low"),
                catalog_model("duplicate", 2, "high"),
            ],
        ),
        ResponseTemplate::new(200).set_body_raw(" ".repeat(1024 * 1024 + 1), "application/json"),
        ResponseTemplate::new(302).insert_header("location", format!("{}/elsewhere", other.uri())),
    ] {
        let endpoint = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/catalog"))
            .respond_with(response)
            .expect(1)
            .mount(&endpoint)
            .await;
        let product = ProductModels::new();
        configure_catalog(
            &product,
            &endpoint.uri(),
            &format!("{}/catalog", endpoint.uri()),
            Some("catalog-key"),
            "",
        );
        let server = product.server(None);
        let mut connection = server.connect(1).await;
        let error = server
            .call_error(&mut connection, "thread/start", json!({}))
            .await;
        assert!(
            error["message"]
                .as_str()
                .unwrap()
                .contains("No models are available"),
            "{error}"
        );
        assert!(!product.home.path().join("models_cache.json").exists());
    }
    assert!(other.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn remote_catalog_refreshes_on_websocket_metadata_etag() {
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::{accept_async, tungstenite::Message};

    let catalog = MockServer::start().await;
    for (priority, etag, slug) in [(1, "v1", "ws-old"), (2, "v2", "ws-new")] {
        Mock::given(method("GET"))
            .and(path("/catalog"))
            .respond_with(catalog_response(etag, vec![catalog_model(slug, 1, "low")]))
            .with_priority(priority)
            .up_to_n_times(1)
            .expect(1)
            .mount(&catalog)
            .await;
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let product = ProductModels::new();
    configure_catalog(
        &product,
        &format!("http://{}", listener.local_addr().unwrap()),
        &format!("{}/catalog", catalog.uri()),
        Some("catalog-key"),
        "",
    );
    let config = std::fs::read_to_string(product.home.path().join("config.toml")).unwrap();
    product.write(
        "config.toml",
        &config.replace(
            "request_max_retries = 0",
            "request_max_retries = 0\nsupports_websockets = true\nstream_max_retries = 0",
        ),
    );

    let peer = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = accept_async(stream).await.unwrap();
        let request: Value =
            serde_json::from_str(ws.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
        assert_eq!(request["model"], "ws-old");
        for frame in [
            json!({"type":"codex.response.metadata","headers":{"X-Models-Etag":"v2"}}),
            websocket_completion(1),
        ] {
            ws.send(Message::Text(frame.to_string().into()))
                .await
                .unwrap();
        }
    });
    let server = product.server(None);
    let mut connection = server.connect(1).await;
    let (thread, started) = start(&server, &mut connection, json!({})).await;
    assert_eq!(started["model"], "ws-old");
    let result = completed_turn(&server, &mut connection, &thread, "refresh catalog").await;
    assert_eq!(result["turn"]["status"], "completed", "{result}");
    timeout(PATIENCE, peer).await.unwrap().unwrap();
    let (_, started) = start(&server, &mut connection, json!({})).await;
    assert_eq!(started["model"], "ws-new");
    catalog.verify().await;
}

#[cfg(unix)]
#[tokio::test]
async fn remote_catalog_preserves_metadata_across_token_expiry_and_refreshes_on_rotation() {
    let endpoint = responses_endpoint().await;
    let product = ProductModels::new();
    product.write("token.sh", "cat token\n");
    product.write("token", "first-key\n");
    configure_catalog(
        &product,
        &endpoint.uri(),
        &format!("{}/catalog", endpoint.uri()),
        None,
        &format!(
            r#"[model_providers.gateway.auth]
command = "sh"
args = ["token.sh"]
cwd = {:?}
refresh_interval_ms = 1
"#,
            product.home.path().canonicalize().unwrap(),
        ),
    );
    for (token, effort) in [("first-key", "low"), ("second-key", "high")] {
        let mut model = catalog_model("remote", 1, effort);
        model["model_messages"]["instructions_template"] = json!(format!("Prompt for {token}"));
        Mock::given(method("GET"))
            .and(path("/catalog"))
            .and(header("authorization", format!("Bearer {token}")))
            .respond_with(catalog_response(token, vec![model]).set_delay(Duration::from_millis(20)))
            .expect(1)
            .mount(&endpoint)
            .await;
    }
    let server = product.server(None);
    let mut connection = server.connect(1).await;
    let (thread, started) = start(&server, &mut connection, json!({})).await;
    assert_eq!(started["model"], "remote");
    for token in ["first-key", "second-key"] {
        product.write("token", &format!("{token}\n"));
        tokio::time::sleep(Duration::from_millis(10)).await;
        let result = completed_turn(&server, &mut connection, &thread, "Use the catalog").await;
        assert_eq!(result["turn"]["status"], "completed", "{result}");
    }
    let requests = endpoint.received_requests().await.unwrap();
    assert_eq!(requests.len(), 4);
    for (pair, (token, effort)) in requests
        .chunks_exact(2)
        .zip([("first-key", "low"), ("second-key", "high")])
    {
        assert_eq!(pair[0].url.path(), "/catalog");
        assert_eq!(pair[1].url.path(), "/product/v1/responses");
        assert_eq!(pair[1].headers["authorization"], format!("Bearer {token}"));
        let body: Value = pair[1].body_json().unwrap();
        assert_eq!(body["reasoning"]["effort"], effort);
        assert_developer_prefix(&body, &format!("Prompt for {token}"));
    }
    endpoint.verify().await;
}

#[tokio::test]
async fn remote_catalog_is_shared_across_threads_and_stays_fixed_during_a_turn() {
    let endpoint = MockServer::start().await;
    let product = ProductModels::new();
    configure_catalog(
        &product,
        &endpoint.uri(),
        &format!("{}/catalog", endpoint.uri()),
        Some("catalog-key"),
        "",
    );
    for (priority, etag, effort) in [(1, "v1", "low"), (2, "v2", "high")] {
        let mut model = catalog_model("remote", 1, effort);
        model["model_messages"]["instructions_template"] = json!(format!("Prompt at {etag}"));
        Mock::given(method("GET"))
            .and(path("/catalog"))
            .respond_with(catalog_response(etag, vec![model]))
            .with_priority(priority)
            .up_to_n_times(1)
            .expect(1)
            .mount(&endpoint)
            .await;
    }
    let mut response = websocket_completion(0);
    response["response"]["output"] = json!([{
        "id": "read", "type": "function_call", "call_id": "read-one",
        "name": "read_file", "arguments": "{\"path\":\"example.txt\"}"
    }]);
    std::fs::write(product.workspace.path().join("example.txt"), "sample").unwrap();
    Mock::given(method("POST"))
        .and(path("/product/v1/responses"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("x-models-etag", "v2")
                .set_body_raw(format!("data: {response}\n\n"), "text/event-stream"),
        )
        .with_priority(1)
        .up_to_n_times(1)
        .mount(&endpoint)
        .await;
    Mock::given(method("POST"))
        .and(path("/product/v1/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            format!("data: {}\n\n", websocket_completion(1)),
            "text/event-stream",
        ))
        .with_priority(2)
        .mount(&endpoint)
        .await;

    let server = product.server(None);
    let mut first_conn = server.connect(1).await;
    let mut second_conn = server.connect(2).await;
    let ((first, _), (second, _)) = tokio::join!(
        start(&server, &mut first_conn, json!({})),
        start(&server, &mut second_conn, json!({})),
    );
    assert_eq!(endpoint.received_requests().await.unwrap().len(), 1);
    for (thread, conn) in [(&first, &mut first_conn), (&second, &mut second_conn)] {
        let result = completed_turn(&server, conn, thread, "Use the catalog").await;
        assert_eq!(result["turn"]["status"], "completed", "{result}");
    }
    let requests = endpoint.received_requests().await.unwrap();
    assert_eq!(requests.len(), 5);
    let bodies = requests
        .iter()
        .filter(|request| request.url.path() == "/product/v1/responses")
        .map(|request| request.body_json::<Value>().unwrap());
    for (body, (etag, effort)) in bodies.zip([("v1", "low"), ("v1", "low"), ("v2", "high")]) {
        assert_eq!(body["reasoning"]["effort"], effort);
        assert_developer_prefix(&body, &format!("Prompt at {etag}"));
    }
    endpoint.verify().await;
}

#[tokio::test]
async fn experimental_feature_toggle_updates_catalog_discovery_for_loaded_threads() {
    let endpoint = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/catalog"))
        .and(header("authorization", "Bearer catalog-key"))
        .respond_with(catalog_response(
            "\"v1\"",
            vec![catalog_model("catalog-model", 1, "high")],
        ))
        .expect(1)
        .mount(&endpoint)
        .await;
    mount_sse(&endpoint, "\"v1\"").await;

    let product = ProductModels::new();
    configure_catalog(
        &product,
        &endpoint.uri(),
        &format!("{}/catalog", endpoint.uri()),
        Some("catalog-key"),
        "model = 'catalog-model'",
    );
    let server = product.server_with_feature_config(product.parts().2).await;
    let mut client = server.connect(1).await;
    let update = |enabled| json!({"enablement": {"api_key_model_discovery": enabled}});
    let method_name = "experimentalFeature/enablement/set";
    assert_eq!(
        server.call(&mut client, method_name, update(false)).await,
        update(false)
    );
    let (thread, _) = start(&server, &mut client, json!({})).await;
    assert!(endpoint.received_requests().await.unwrap().is_empty());

    assert_eq!(
        server.call(&mut client, method_name, update(true)).await,
        update(true)
    );
    assert_eq!(
        completed_turn(&server, &mut client, &thread, "Discovery enabled").await["turn"]["status"],
        "completed"
    );
    let requests = endpoint.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2);
    let discovered: Value = requests[1].body_json().unwrap();
    assert_developer_prefix(&discovered, "Instructions for catalog-model");

    assert_eq!(
        server.call(&mut client, method_name, update(false)).await,
        update(false)
    );
    assert_eq!(
        completed_turn(&server, &mut client, &thread, "Discovery disabled").await["turn"]["status"],
        "completed"
    );
    let requests = endpoint.received_requests().await.unwrap();
    assert_eq!(requests.len(), 3);
    let undiscovered: Value = requests[2].body_json().unwrap();
    assert!(
        !undiscovered
            .to_string()
            .contains("Instructions for catalog-model")
    );
    endpoint.verify().await;
}
