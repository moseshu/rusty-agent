// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Hosted web search on product threads, from the configured mode, the `[tools.web_search]`
//! options, the provider's capabilities and the turn's model.

use super::*;
use ra_coding::web_search::{CodingWebSearch, WebSearchMode, WebSearchToolConfig};

/// Writes a catalog of a text-only and an image-capable model, a gateway provider with
/// `provider_options`, and a configuration with `config` before any table.
fn configure(product: &ProductModels, endpoint: &MockServer, config: &str, provider_options: &str) {
    let model = |slug: &str, tool_type: &str| {
        json!({
            "slug": slug, "display_name": slug,
            "default_reasoning_level": "low", "supported_reasoning_levels": [],
            "shell_type": "unified_exec", "visibility": "list", "supported_in_api": true,
            "priority": 1, "truncation_policy": {"mode": "tokens", "limit": 1000},
            "experimental_supported_tools": [], "context_window": 8000,
            "support_verbosity": false, "base_instructions": "Catalog instructions",
            "web_search_tool_type": tool_type
        })
    };
    let catalog = json!({"models": [
        model("vendor/text", "text"),
        model("vendor/image", "text_and_image"),
    ]});
    product.write("catalog.json", &catalog.to_string());
    product.write(
        "config.toml",
        &format!(
            r#"
approval_policy = "never"
sandbox_mode = "read-only"
model = "vendor/text"
model_catalog_json = {:?}
{config}
"#,
            product.home.path().join("catalog.json")
        ),
    );
    product.write(
        "requirements.toml",
        &format!(
            r#"
model_provider = "gateway"
[model_providers.gateway]
name = "Gateway"
base_url = "{}/product/v1/"
experimental_bearer_token = "test-token"
stream_max_retries = 0
{provider_options}
"#,
            endpoint.uri()
        ),
    );
}

/// The hosted search declaration in each request the endpoint received, if any.
async fn declarations(endpoint: &MockServer) -> Vec<Option<Value>> {
    endpoint
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|request| {
            let body: Value = request.body_json().unwrap();
            body["tools"].as_array().and_then(|tools| {
                tools
                    .iter()
                    .find(|tool| tool["type"] == "web_search")
                    .cloned()
            })
        })
        .collect()
}

/// Runs one turn on a new thread and returns its search declaration.
async fn first_declaration(product: &ProductModels, endpoint: &MockServer) -> Option<Value> {
    let (server, _services) = product.standard_server();
    let mut connection = server.connect(1).await;
    let (thread, _) = start(&server, &mut connection, json!({})).await;
    server.run_turn(&mut connection, &thread, "Search").await;
    let mut declarations = declarations(endpoint).await;
    assert_eq!(declarations.len(), 1);
    declarations.remove(0)
}

/// As Codex's `web_search_mode_cached_sets_external_web_access_false`,
/// `web_search_mode_takes_precedence_over_legacy_flags` and
/// `web_search_mode_defaults_to_cached_when_features_disabled`: the configured or default cached
/// mode declares search without live access, whatever the legacy feature flags say.
#[tokio::test]
async fn product_web_search_is_cached_by_default_and_by_configuration() {
    for config in [
        "",
        "web_search = \"cached\"",
        "web_search = \"cached\"\n[features]\nweb_search_request = true",
        "[features]\nweb_search_cached = false\nweb_search_request = false",
    ] {
        let endpoint = responses_endpoint().await;
        let product = ProductModels::new();
        configure(&product, &endpoint, config, "");
        assert_eq!(
            first_declaration(&product, &endpoint).await,
            Some(json!({"type": "web_search", "external_web_access": false})),
            "{config}"
        );
    }
}

/// As Codex's `web_search_mode_updates_between_turns_with_permission_profile`: a turn under full
/// access prefers live search, and a read-only turn keeps the cached default.
#[tokio::test]
async fn product_web_search_follows_each_turns_permission_profile() {
    let endpoint = responses_endpoint().await;
    let product = ProductModels::new();
    configure(&product, &endpoint, "", "");
    let (server, _services) = product.standard_server();
    let mut connection = server.connect(1).await;
    let (thread, _) = start(&server, &mut connection, json!({})).await;
    server.run_turn(&mut connection, &thread, "Cached").await;
    server
        .call(
            &mut connection,
            "turn/start",
            json!({"threadId": thread, "input": [{"type": "text", "text": "Live"}],
                   "permissions": ":danger-full-access"}),
        )
        .await;
    connection.notification("turn/completed").await;

    let access = declarations(&endpoint)
        .await
        .into_iter()
        .map(|tool| tool.unwrap()["external_web_access"].clone())
        .collect::<Vec<_>>();
    assert_eq!(access, [json!(false), json!(true)]);
}

/// As Codex's `web_search_tool_config_from_config_toml_is_forwarded_to_request` and
/// `indexed_web_search_mode_sets_indexed_access`.
#[tokio::test]
async fn product_web_search_forwards_the_configured_mode_and_options() {
    let endpoint = responses_endpoint().await;
    let product = ProductModels::new();
    configure(
        &product,
        &endpoint,
        r#"web_search = "live"

[tools.web_search]
context_size = "high"
allowed_domains = ["example.com"]
location = { country = "US", city = "New York", timezone = "America/New_York" }"#,
        "",
    );
    assert_eq!(
        first_declaration(&product, &endpoint).await,
        Some(json!({
            "type": "web_search",
            "external_web_access": true,
            "search_context_size": "high",
            "filters": {"allowed_domains": ["example.com"]},
            "user_location": {
                "type": "approximate",
                "country": "US",
                "city": "New York",
                "timezone": "America/New_York",
            },
        }))
    );

    let endpoint = responses_endpoint().await;
    let product = ProductModels::new();
    configure(&product, &endpoint, "web_search = \"indexed\"", "");
    let tool = first_declaration(&product, &endpoint).await.unwrap();
    assert_eq!(
        (&tool["external_web_access"], &tool["indexed_web_access"]),
        (&json!(true), &json!(true))
    );
}

/// As Codex's `web_search_mode_for_turn_falls_back_when_provider_disallows_external_web_access`,
/// through a configured provider: live and indexed preferences fall back to cached search.
#[tokio::test]
async fn product_web_search_falls_back_to_cached_without_external_access() {
    for mode in ["live", "indexed"] {
        let endpoint = responses_endpoint().await;
        let product = ProductModels::new();
        configure(
            &product,
            &endpoint,
            &format!("web_search = \"{mode}\""),
            "[model_providers.gateway.capabilities]\nexternal_web_access = false",
        );
        assert_eq!(
            first_declaration(&product, &endpoint).await,
            Some(json!({"type": "web_search", "external_web_access": false})),
            "{mode}"
        );
    }
}

/// Requirements and the configured mode narrow what the host allows rather than replacing it: a
/// host that permits only disabled or cached search keeps a full-access turn cached, and a host
/// that disabled search keeps it disabled.
#[tokio::test]
async fn product_web_search_preserves_host_mode_limits_and_disable() {
    for (search, expected) in [
        (
            CodingWebSearch {
                allowed_modes: vec![WebSearchMode::Disabled, WebSearchMode::Cached],
                ..Default::default()
            },
            Some(json!({"type": "web_search", "external_web_access": false})),
        ),
        (
            CodingWebSearch {
                mode: WebSearchMode::Disabled,
                ..Default::default()
            },
            None,
        ),
    ] {
        let endpoint = responses_endpoint().await;
        let product = ProductModels::new();
        configure(&product, &endpoint, "", "");
        let (server, _services) =
            product.standard_server_with_host(None, |host| host.with_web_search(search));
        let mut connection = server.connect(1).await;
        let (thread, _) = start(
            &server,
            &mut connection,
            json!({"permissions": ":danger-full-access"}),
        )
        .await;
        server.run_turn(&mut connection, &thread, "Search").await;
        assert_eq!(declarations(&endpoint).await, [expected]);
    }
}

/// An injected resolver owns the provider, so the configured provider's capabilities do not apply:
/// live search stays live even though the configured provider disallows external access, and the
/// host's result kind stands. The configured mode still selects live search.
#[tokio::test]
async fn product_web_search_uses_host_capabilities_with_an_injected_resolver() {
    let endpoint = responses_endpoint().await;
    let product = ProductModels::new();
    configure(
        &product,
        &endpoint,
        "web_search = \"live\"",
        "[model_providers.gateway.capabilities]\nexternal_web_access = false",
    );
    let script = Arc::new(Script::default());
    script.answer("Done");
    let (server, _services) = product
        .standard_server_with_host(Some(Arc::new(Resolver(Arc::clone(&script)))), |host| host);
    let mut connection = server.connect(1).await;
    let (thread, _) = start(&server, &mut connection, json!({"model": "vendor/image"})).await;
    server.run_turn(&mut connection, &thread, "Search").await;

    let request = script.last_request();
    let search = request
        .tools()
        .iter()
        .find(|tool| tool.name() == "web_search")
        .unwrap();
    let ra_core::model::ModelToolKind::Hosted { declaration, .. } = search.kind() else {
        panic!("expected a hosted search declaration");
    };
    assert_eq!(
        declaration,
        &json!({"type": "web_search", "external_web_access": true})
    );
    assert!(endpoint.received_requests().await.unwrap().is_empty());
}

/// A catalog that can select an image-capable model is checked with the wider image declaration:
/// options that fill the schema-byte budget exactly fail thread assembly, and the same options
/// pass once every catalog model is text-only.
#[tokio::test]
async fn product_web_search_checks_native_image_declarations_against_the_schema_budget() {
    let endpoint = responses_endpoint().await;
    let product = ProductModels::new();
    let (host, _, _) = product.parts();
    let (host, _services) = ProductModels::standard_host(host);
    let host = host.with_web_search(CodingWebSearch {
        tools: WebSearchToolConfig {
            allowed_domains: Some(vec!["example.com".into()]),
            ..Default::default()
        },
        ..Default::default()
    });
    let baseline = ra_coding::host_backed_surface(
        &ra_core::prompt::PromptRole::Main,
        &host,
        ra_coding::CodingProfile::CodexLike,
    )
    .await
    .unwrap();
    let spare = baseline.budget().max_advertised_bytes().unwrap()
        - baseline.tool_surface().advertised_bytes();
    let domain = format!("{}example.com", "a".repeat(spare));
    configure(
        &product,
        &endpoint,
        &format!("[tools.web_search]\nallowed_domains = [{domain:?}]"),
        "",
    );
    let (server, _services) = product.standard_server();
    let mut connection = server.connect(1).await;
    let error = server
        .call_error(&mut connection, "thread/start", json!({}))
        .await;
    assert!(
        error
            .to_string()
            .contains("hosted search alternative exceeds"),
        "{error}"
    );

    let mut catalog: Value =
        serde_json::from_slice(&std::fs::read(product.home.path().join("catalog.json")).unwrap())
            .unwrap();
    catalog["models"][1]["web_search_tool_type"] = json!("text");
    product.write("catalog.json", &catalog.to_string());
    let (server, _services) = product.standard_server();
    let mut connection = server.connect(1).await;
    start(&server, &mut connection, json!({})).await;
}

/// Codex reads the result kind from each turn's model, so a turn on an image-capable model asks
/// for image results and a later turn back on a text-only model does not.
#[tokio::test]
async fn product_web_search_takes_the_result_kind_from_each_turns_model() {
    let endpoint = responses_endpoint().await;
    let product = ProductModels::new();
    configure(&product, &endpoint, "", "");
    let (server, _services) = product.standard_server();
    let mut connection = server.connect(1).await;
    let (thread, _) = start(&server, &mut connection, json!({})).await;
    for model in ["vendor/text", "vendor/image", "vendor/text"] {
        server
            .call(
                &mut connection,
                "turn/start",
                json!({"threadId": thread, "input": [{"type": "text", "text": "Search"}],
                       "model": model}),
            )
            .await;
        connection.notification("turn/completed").await;
    }

    let kinds = declarations(&endpoint)
        .await
        .into_iter()
        .map(|tool| tool.unwrap().get("search_content_types").cloned())
        .collect::<Vec<_>>();
    assert_eq!(kinds, [None, Some(json!(["text", "image"])), None]);
}

/// Places `settings` before the first table of the Bedrock configuration.
#[cfg(unix)]
fn prepend_config(product: &ProductModels, settings: &str) {
    let path = product.home.path().join("config.toml");
    let config = std::fs::read_to_string(&path).unwrap();
    std::fs::write(path, format!("{settings}\n{config}")).unwrap();
}

/// Runs one full-access turn and returns the request's search declaration.
#[cfg(unix)]
async fn bedrock_search_declaration(
    product: &ProductModels,
    endpoint: &MockServer,
) -> Option<Value> {
    let (server, _services) = product.standard_server();
    let mut connection = server.connect(1).await;
    let (thread, _) = start(
        &server,
        &mut connection,
        json!({"permissions": ":danger-full-access"}),
    )
    .await;
    let result = completed_turn(&server, &mut connection, &thread, "Search").await;
    assert_eq!(result["turn"]["status"], "completed", "{result}");
    let requests = endpoint.received_requests().await.unwrap();
    let body: Value = super::bedrock::responses_requests(&requests)[0]
        .body_json()
        .unwrap();
    body["tools"].as_array().and_then(|tools| {
        tools
            .iter()
            .find(|tool| tool["type"] == "web_search")
            .cloned()
    })
}

/// As Codex's `amazon_bedrock_web_search_uses_text_only_hosted_tools`: Mantle searches without
/// live access, so a live or indexed preference falls back to cached text-only search.
#[cfg(unix)]
#[tokio::test]
async fn bedrock_mantle_web_search_is_cached_and_text_only() {
    for settings in ["", "web_search = \"live\"", "web_search = \"indexed\""] {
        let endpoint = MockServer::start().await;
        let product = ProductModels::new();
        super::bedrock::configure_credential_export(
            &product,
            "amazon-bedrock",
            &format!("{}/mantle/openai/v1", endpoint.uri()),
        );
        prepend_config(&product, settings);
        Mock::given(method("POST"))
            .and(path("/mantle/openai/v1/responses"))
            .respond_with(super::bedrock::completion())
            .mount(&endpoint)
            .await;
        assert_eq!(
            bedrock_search_declaration(&product, &endpoint).await,
            Some(json!({"type": "web_search", "external_web_access": false})),
            "{settings}"
        );
    }
}

/// As Codex's `amazon_bedrock_runtime_preserves_cross_region_models_without_web_search`.
#[cfg(unix)]
#[tokio::test]
async fn bedrock_runtime_declares_no_web_search() {
    let endpoint = MockServer::start().await;
    let product = ProductModels::new();
    super::bedrock::configure_credential_export(
        &product,
        "amazon-bedrock-runtime",
        &format!("{}/runtime/openai/v1", endpoint.uri()),
    );
    prepend_config(&product, "web_search = \"cached\"");
    Mock::given(method("POST"))
        .and(path("/runtime/openai/v1/responses"))
        .respond_with(super::bedrock::completion())
        .mount(&endpoint)
        .await;
    assert_eq!(bedrock_search_declaration(&product, &endpoint).await, None);
}

/// As Codex's `amazon_bedrock_web_search_is_disabled_when_managed_requirements_prohibit_cached_search`:
/// requirements that allow only live or indexed search leave Mantle with no mode it supports.
#[cfg(unix)]
#[tokio::test]
async fn bedrock_web_search_is_disabled_when_requirements_prohibit_cached_search() {
    for allowed in ["live", "indexed"] {
        let endpoint = MockServer::start().await;
        let product = ProductModels::new();
        super::bedrock::configure_credential_export(
            &product,
            "amazon-bedrock",
            &format!("{}/mantle/openai/v1", endpoint.uri()),
        );
        product.write(
            "requirements.toml",
            &format!("allowed_web_search_modes = [\"{allowed}\"]\n"),
        );
        Mock::given(method("POST"))
            .and(path("/mantle/openai/v1/responses"))
            .respond_with(super::bedrock::completion())
            .mount(&endpoint)
            .await;
        assert_eq!(
            bedrock_search_declaration(&product, &endpoint).await,
            None,
            "{allowed}"
        );
    }
}
