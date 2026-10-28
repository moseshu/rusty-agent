// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Anonymous featured metadata, source cache behavior and actual product wire lowering.

use ra_coding::{
    plugins::{
        PluginConfigManager, PluginRuntimeConfig, PluginServices, PluginsConfigInput,
        PluginsManager,
        manager_test_api::expire_featured_plugin_ids_cache,
        remote::RemotePluginServiceConfig,
        remote_legacy::{RemotePluginFetchError, fetch_remote_featured_plugin_ids},
        startup_sync::{curated_plugins_api_marketplace_path, curated_plugins_repo_path},
    },
    skills::HostSkillsService,
};
use ra_config::{ConfigLoadOptions, LoaderOverrides, NoopThreadConfigLoader};
use ra_http::{HttpClientFactory, NetworkPolicyController, OutboundProxyPolicy};
use ra_paths::AbsolutePathBuf;
use ra_protocol::server::protocol::{PluginListParams, PluginManagementRequest, Product};
use serde_json::json;
use std::{path::Path, sync::Arc};
use tempfile::TempDir;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path, query_param},
};

fn factory() -> HttpClientFactory {
    HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault)
}
fn absolute(path: &Path) -> AbsolutePathBuf {
    AbsolutePathBuf::from_absolute_path(path).unwrap()
}
fn write(path: &Path, contents: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}
fn config_manager(home: &Path, http: HttpClientFactory) -> PluginConfigManager {
    PluginConfigManager::new(
        ra_executor::LOCAL_FS.clone(),
        absolute(home),
        absolute(home),
        ConfigLoadOptions::from(LoaderOverrides::without_managed_config_for_tests()),
        Arc::new(NoopThreadConfigLoader),
        http,
    )
}
async fn config(home: &Path, server: &MockServer) -> PluginsConfigInput {
    write(
        &home.join("config.toml"),
        &format!(
            "chatgpt_base_url = {:?}\n[features]\nplugins = true\n",
            format!("{}/backend-api/", server.uri())
        ),
    );
    config_manager(home, factory())
        .load_non_project_config()
        .await
        .unwrap()
        .plugins_config_input()
}
fn manager(home: &Path, product: Option<Product>) -> PluginsManager {
    PluginsManager::new_with_options(
        home.to_path_buf(),
        product,
        Arc::new(HostSkillsService::new(absolute(home), false)),
    )
}
async fn featured_response(server: &MockServer, body: &str, expected: u64) {
    Mock::given(method("GET"))
        .and(path("/backend-api/plugins/featured"))
        .and(query_param("platform", "codex"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .expect(expected)
        .mount(server)
        .await;
}

#[tokio::test]
async fn featured_plugin_ids_for_config_defaults_query_param_to_codex() {
    let home = TempDir::new().unwrap();
    let server = MockServer::start().await;
    featured_response(&server, r#"["codex-plugin"]"#, 1).await;
    let config = config(home.path(), &server).await;
    let ids = manager(home.path(), None)
        .featured_plugin_ids_for_config(&config)
        .await
        .unwrap();
    assert_eq!(ids, vec!["codex-plugin"]);
    let requests = server.received_requests().await.unwrap();
    assert!(!requests[0].headers.contains_key("authorization"));
    assert!(!requests[0].headers.contains_key("chatgpt-account-id"));
    assert!(!requests[0].headers.contains_key("cookie"));
}

#[tokio::test]
async fn featured_plugin_ids_for_config_uses_restriction_product_query_param() {
    let home = TempDir::new().unwrap();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/backend-api/plugins/featured"))
        .and(query_param("platform", "chat"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!(["chat-plugin"])))
        .expect(1)
        .mount(&server)
        .await;
    let config = config(home.path(), &server).await;
    let ids = manager(home.path(), Some(Product::Chatgpt))
        .featured_plugin_ids_for_config(&config)
        .await
        .unwrap();
    assert_eq!(ids, vec!["chat-plugin"]);
}

#[tokio::test]
async fn successful_cache_entries_expire_and_are_keyed_by_endpoint() {
    let home = TempDir::new().unwrap();
    let first = MockServer::start().await;
    let second = MockServer::start().await;
    featured_response(&first, r#"["first"]"#, 2).await;
    featured_response(&second, r#"["second"]"#, 1).await;
    let mut config = config(home.path(), &first).await;
    let manager = manager(home.path(), None);
    assert_eq!(
        manager
            .featured_plugin_ids_for_config(&config)
            .await
            .unwrap(),
        vec!["first"]
    );
    assert_eq!(
        manager
            .featured_plugin_ids_for_config(&config.clone())
            .await
            .unwrap(),
        vec!["first"]
    );
    expire_featured_plugin_ids_cache(&manager);
    assert_eq!(
        manager
            .featured_plugin_ids_for_config(&config)
            .await
            .unwrap(),
        vec!["first"]
    );
    config.chatgpt_base_url = format!("{}/backend-api", second.uri());
    assert_eq!(
        manager
            .featured_plugin_ids_for_config(&config)
            .await
            .unwrap(),
        vec!["second"]
    );
}

#[tokio::test]
async fn disabled_plugins_do_not_request_featured_metadata() {
    let home = TempDir::new().unwrap();
    let server = MockServer::start().await;
    let mut config = config(home.path(), &server).await;
    config.plugins_enabled = false;
    assert!(
        manager(home.path(), None)
            .featured_plugin_ids_for_config(&config)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn status_and_decode_failures_are_not_cached_as_success() {
    let home = TempDir::new().unwrap();
    let server = MockServer::start().await;
    let config = config(home.path(), &server).await;
    let manager = manager(home.path(), None);
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(503).set_body_string("unavailable"))
        .expect(1)
        .mount(&server)
        .await;
    assert!(
        matches!(manager.featured_plugin_ids_for_config(&config).await,
        Err(RemotePluginFetchError::UnexpectedStatus { status, body, .. })
        if status.as_u16() == 503 && body == "unavailable")
    );
    server.reset().await;
    featured_response(&server, "not json", 1).await;
    assert!(matches!(
        manager.featured_plugin_ids_for_config(&config).await,
        Err(RemotePluginFetchError::Decode { .. })
    ));
    server.reset().await;
    featured_response(&server, r#"["recovered"]"#, 1).await;
    assert_eq!(
        manager
            .featured_plugin_ids_for_config(&config)
            .await
            .unwrap(),
        vec!["recovered"]
    );
}

#[tokio::test]
async fn featured_transport_honors_the_application_destination_policy() {
    let server = MockServer::start().await;
    let policy = NetworkPolicyController::default();
    let service = RemotePluginServiceConfig::new(
        format!("{}/backend-api", server.uri()),
        factory().with_network_policy(policy.policy()),
    );
    assert!(matches!(
        fetch_remote_featured_plugin_ids(&service, None).await,
        Err(RemotePluginFetchError::Request { .. })
    ));
    assert!(server.received_requests().await.unwrap().is_empty());
    let invalid = RemotePluginServiceConfig::new(":invalid".into(), factory());
    assert!(matches!(
        fetch_remote_featured_plugin_ids(&invalid, None).await,
        Err(RemotePluginFetchError::InvalidBaseUrl(_))
    ));
}

fn curated_fixture(home: &Path) {
    write(
        &curated_plugins_api_marketplace_path(home),
        r#"{
        "name":"openai-api-curated","plugins":[{"name":"sample","source":{"source":"local","path":"./plugins/sample"}}]
    }"#,
    );
    write(
        &curated_plugins_repo_path(home).join("plugins/sample/.codex-plugin/plugin.json"),
        r#"{"name":"sample"}"#,
    );
}
async fn list(services: &Arc<PluginServices>, force_refetch: bool) -> serde_json::Value {
    services
        .protocol_adapter()
        .handle(PluginManagementRequest::PluginList(PluginListParams {
            cwds: None,
            marketplace_kinds: None,
            force_refetch,
        }))
        .await
        .unwrap()
}

#[tokio::test]
async fn product_list_skips_featured_fetch_for_api_key_curated_catalog() {
    let home = TempDir::new().unwrap();
    let server = MockServer::start().await;
    config(home.path(), &server).await;
    curated_fixture(home.path());
    featured_response(&server, r#"["sample@openai-api-curated"]"#, 0).await;
    let services = PluginServices::new(
        config_manager(home.path(), factory()),
        Arc::new(HostSkillsService::new(absolute(home.path()), false)),
    );
    for force_refetch in [false, true] {
        let response = list(&services, force_refetch).await;
        assert_eq!(
            response["marketplaces"][0]["name"], "openai-api-curated",
            "{response}"
        );
        assert_eq!(response["featuredPluginIds"], json!([]));
        let summary = &response["marketplaces"][0]["plugins"][0];
        assert_eq!(summary.get("installPolicySource"), Some(&json!(null)));
        assert_eq!(
            summary.get("mustShowInstallationInterstitial"),
            Some(&json!(null))
        );
    }
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn product_list_skips_featured_fetch_without_a_curated_catalog() {
    let home = TempDir::new().unwrap();
    let server = MockServer::start().await;
    config(home.path(), &server).await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;
    let services = PluginServices::new(
        config_manager(home.path(), factory()),
        Arc::new(HostSkillsService::new(absolute(home.path()), false)),
    );
    assert_eq!(list(&services, false).await["featuredPluginIds"], json!([]));
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn endpoint_override_survives_runtime_projection() {
    let home = TempDir::new().unwrap();
    let server = MockServer::start().await;
    let input = config(home.path(), &server).await;
    assert_eq!(
        input.remote_plugin_service_config().chatgpt_base_url,
        format!("{}/backend-api/", server.uri())
    );
    let runtime: PluginRuntimeConfig = config_manager(home.path(), factory())
        .load_non_project_config()
        .await
        .unwrap();
    assert_eq!(runtime.chatgpt_base_url, input.chatgpt_base_url);
}

#[tokio::test]
async fn featured_fetch_uses_the_source_ten_second_timeout() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("[]")
                .set_delay(std::time::Duration::from_secs(11)),
        )
        .expect(1)
        .mount(&server)
        .await;
    let service =
        RemotePluginServiceConfig::new(format!("{}/backend-api", server.uri()), factory());
    let started = std::time::Instant::now();
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        fetch_remote_featured_plugin_ids(&service, None),
    )
    .await
    .unwrap();
    assert!(matches!(
        result,
        Err(RemotePluginFetchError::Request { .. })
    ));
    assert!(started.elapsed() >= std::time::Duration::from_secs(9));
}

#[tokio::test]
async fn an_empty_successful_featured_response_is_cached() {
    let home = TempDir::new().unwrap();
    let server = MockServer::start().await;
    featured_response(&server, "[]", 1).await;
    let config = config(home.path(), &server).await;
    let manager = manager(home.path(), None);
    for _ in 0..2 {
        assert!(
            manager
                .featured_plugin_ids_for_config(&config)
                .await
                .unwrap()
                .is_empty()
        );
    }
}
