// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Product threads on a provider behind gateway OAuth, through local catalog, token, and
//! Responses endpoints.
//!
//! Ported from Codex `codex-rs/model-provider/src/shared_state_tests.rs` and
//! `codex-rs/core/tests/suite/gateway_auth.rs`. Codex drives its provider factory and models
//! manager directly and seeds a manager into its provider registry; here a product thread drives
//! both, and the stored credential sits behind an in-memory keyring that every gateway manager for
//! the product home uses. The WebSocket case is local: it covers the handshake path of the
//! Responses adapter's accompanying-credential hook.

use std::path::PathBuf;

use ra_coding::config::gateway_auth_test_api::{
    KeyringOverride, override_keyring, seed_gateway_auth, shared_manager,
};
use ra_config::{GatewayOAuthConfig, GatewayOAuthDelivery};
use ra_keyring_store::tests::MockKeyringStore;
use ra_login::test_api::Token;
use wiremock::matchers::header;

use super::*;

const GATEWAY_FAILURE: &str =
    "Gateway OAuth authentication failed; check the gateway configuration and credential store.";

/// A product home whose gateway credentials live in an in-memory keyring.
pub(super) struct GatewayProduct {
    pub(super) product: ProductModels,
    pub(super) home: PathBuf,
    keyring: Arc<MockKeyringStore>,
    _keyring: KeyringOverride,
}

impl GatewayProduct {
    pub(super) fn new() -> Self {
        let product = ProductModels::new();
        let home = product.home.path().canonicalize().unwrap();
        let keyring = Arc::new(MockKeyringStore::default());
        let guard = override_keyring(&home, keyring.clone());
        Self {
            product,
            home,
            keyring,
            _keyring: guard,
        }
    }

    pub(super) fn seed(&self, config: &GatewayOAuthConfig, token: Token) {
        seed_gateway_auth(config, &self.home, self.keyring.clone(), &token).unwrap();
    }
}

pub(super) fn token(access_token: &str, refresh_token: Option<&str>, expires_at: i64) -> Token {
    Token {
        access_token: access_token.into(),
        refresh_token: refresh_token.map(Into::into),
        expires_at: Some(expires_at),
    }
}

pub(super) fn gateway_config(uri: &str, delivery: GatewayOAuthDelivery) -> GatewayOAuthConfig {
    GatewayOAuthConfig {
        authorization_url: format!("{uri}/authorize"),
        token_url: format!("{uri}/token"),
        client_id: "client".into(),
        resource: None,
        scopes: Vec::new(),
        redirect_port: None,
        delivery,
    }
}

pub(super) fn header_delivery() -> GatewayOAuthDelivery {
    GatewayOAuthDelivery::Header {
        name: "x-gateway-auth".into(),
        scheme: "Bearer".into(),
    }
}

/// Writes a provider with a primary bearer, the gateway OAuth table, and `provider_settings`.
pub(super) fn configure(
    product: &ProductModels,
    base_url: &str,
    gateway: &GatewayOAuthConfig,
    provider_settings: &str,
) {
    let delivery = match &gateway.delivery {
        GatewayOAuthDelivery::Header { name, scheme } => {
            format!("{{ kind = \"header\", name = {name:?}, scheme = {scheme:?} }}")
        }
        GatewayOAuthDelivery::Cookie { name } => {
            format!("{{ kind = \"cookie\", name = {name:?} }}")
        }
    };
    let resource = gateway
        .resource
        .as_ref()
        .map_or_else(String::new, |resource| format!("resource = {resource:?}"));
    let redirect_port = gateway
        .redirect_port
        .map_or_else(String::new, |port| format!("redirect_port = {port}"));
    product.write(
        "config.toml",
        &format!(
            r#"
approval_policy = "never"
sandbox_mode = "read-only"
model = "local-model"
model_provider = "gateway"
[model_providers.gateway]
name = "Gateway"
base_url = "{base_url}"
experimental_bearer_token = "primary-token"
request_max_retries = 0
{provider_settings}
[model_providers.gateway.gateway_oauth]
authorization_url = {authorization_url:?}
token_url = {token_url:?}
client_id = {client_id:?}
{resource}
{redirect_port}
delivery = {delivery}
"#,
            authorization_url = gateway.authorization_url,
            token_url = gateway.token_url,
            client_id = gateway.client_id,
        ),
    );
}

pub(super) fn completion() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(
        format!("data: {}\n\n", websocket_completion(1)),
        "text/event-stream",
    )
}

fn catalog_model(slug: &str) -> Value {
    json!({
        "slug": slug, "display_name": slug, "default_reasoning_level": "medium",
        "supported_reasoning_levels": [], "shell_type": "unified_exec", "visibility": "list",
        "supported_in_api": true, "priority": 1, "support_verbosity": true,
        "default_verbosity": "medium", "truncation_policy": {"mode": "tokens", "limit": 1000},
        "experimental_supported_tools": [], "context_window": 8000,
        "model_messages": {"instructions_template": format!("Instructions for {slug}")}
    })
}

#[tokio::test]
async fn gateway_credentials_accompany_primary_auth_in_catalog_and_responses() {
    for delivery in [
        header_delivery(),
        GatewayOAuthDelivery::Cookie {
            name: "gateway_session".into(),
        },
    ] {
        let endpoint = MockServer::start().await;
        let gateway = GatewayProduct::new();
        let config = gateway_config(&endpoint.uri(), delivery.clone());
        configure(
            &gateway.product,
            &format!("{}/product/v1", endpoint.uri()),
            &config,
            &format!("model_catalog_url = \"{}/models\"", endpoint.uri()),
        );
        gateway.seed(
            &config,
            token("gateway-token", Some("refresh-secret"), i64::MAX),
        );
        Mock::given(method("GET"))
            .and(path("/models"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"models": [catalog_model("local-model")]})),
            )
            .expect(1)
            .mount(&endpoint)
            .await;
        Mock::given(method("POST"))
            .and(path("/product/v1/responses"))
            .respond_with(completion())
            .expect(1)
            .mount(&endpoint)
            .await;

        let server = gateway.product.server(None);
        let mut connection = server.connect(1).await;
        let (thread, _) = start(&server, &mut connection, json!({})).await;
        let completed = completed_turn(&server, &mut connection, &thread, "hello").await;
        assert_eq!(completed["turn"]["status"], "completed", "{completed}");

        let (name, value) = match delivery {
            GatewayOAuthDelivery::Header { .. } => ("x-gateway-auth", "Bearer gateway-token"),
            GatewayOAuthDelivery::Cookie { .. } => ("cookie", "gateway_session=gateway-token"),
        };
        let requests = endpoint.received_requests().await.unwrap();
        assert_eq!(requests.len(), 2);
        for request in requests {
            assert_eq!(request.headers["authorization"], "Bearer primary-token");
            assert_eq!(request.headers[name], value);
        }
    }
}

#[tokio::test]
async fn gateway_refresh_preserves_primary_auth_and_hides_issuer_errors() {
    for succeeds in [true, false] {
        let endpoint = MockServer::start().await;
        let gateway = GatewayProduct::new();
        let mut config = gateway_config(&endpoint.uri(), header_delivery());
        config.token_url = format!("{}/token?credential=query-secret", endpoint.uri());
        configure(
            &gateway.product,
            &format!("{}/product/v1", endpoint.uri()),
            &config,
            // One stream retry shows that issuer failures stay retryable.
            "stream_max_retries = 1",
        );
        gateway.seed(&config, token("expired", Some("refresh-secret"), 0));
        let body = if succeeds {
            json!({"access_token": "rotated-token", "expires_in": 3600})
        } else {
            json!({
                "error": "server_error",
                "error_description": format!("query-secret {}", "sensitive ".repeat(4000)),
            })
        };
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(
                ResponseTemplate::new(if succeeds { 200 } else { 500 }).set_body_json(body),
            )
            // A failed refresh is retried by the turn, as Codex retries its `Io` error.
            .expect(if succeeds { 1 } else { 2 })
            .mount(&endpoint)
            .await;
        Mock::given(method("POST"))
            .and(path("/product/v1/responses"))
            .and(header("authorization", "Bearer primary-token"))
            .and(header("x-gateway-auth", "Bearer rotated-token"))
            .respond_with(completion())
            .expect(u64::from(succeeds))
            .mount(&endpoint)
            .await;

        let server = gateway.product.server(None);
        let mut connection = server.connect(1).await;
        let (thread, _) = start(&server, &mut connection, json!({})).await;
        let result = completed_turn(&server, &mut connection, &thread, "hello").await;
        if succeeds {
            assert_eq!(result["turn"]["status"], "completed", "{result}");
        } else {
            assert_eq!(result["turn"]["status"], "failed", "{result}");
            let message = result["turn"]["error"]["message"].as_str().unwrap();
            assert!(message.contains(GATEWAY_FAILURE), "{message}");
            assert!(!result.to_string().contains("query-secret"), "{result}");
        }
        let requests = endpoint.received_requests().await.unwrap();
        let grant = &requests[0];
        assert_eq!(grant.url.path(), "/token");
        assert!(!grant.headers.contains_key("authorization"));
        assert!(!grant.headers.contains_key("x-gateway-auth"));
        assert_eq!(
            std::str::from_utf8(&grant.body)
                .unwrap()
                .split('&')
                .collect::<std::collections::BTreeSet<_>>(),
            std::collections::BTreeSet::from([
                "grant_type=refresh_token",
                "refresh_token=refresh-secret",
                "client_id=client"
            ])
        );
    }
}

#[tokio::test]
async fn catalog_cache_is_reused_only_for_matching_gateway_configuration() {
    let endpoint = MockServer::start().await;
    let gateway = GatewayProduct::new();
    let mut config = gateway_config(&endpoint.uri(), header_delivery());
    for resource in ["first", "second"] {
        config.resource = Some(format!("{}/{resource}", endpoint.uri()));
        configure(
            &gateway.product,
            &format!("{}/product/v1", endpoint.uri()),
            &config,
            &format!("model_catalog_url = \"{}/models\"", endpoint.uri()),
        );
        gateway.seed(&config, token(resource, Some("refresh-secret"), i64::MAX));
        let slug = format!("catalog-{resource}");
        Mock::given(method("GET"))
            .and(path("/models"))
            .and(header("x-gateway-auth", format!("Bearer {resource}")))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"models": [catalog_model(&slug)]})),
            )
            .expect(1)
            .mount(&endpoint)
            .await;
        // A second app server reads the same disk cache: the first start for each gateway
        // configuration fetches, the second reuses that configuration's catalog.
        for _ in 0..2 {
            let server = gateway.product.server(None);
            let mut connection = server.connect(1).await;
            let (_, started) = start(&server, &mut connection, json!({"model": null})).await;
            assert_eq!(started["model"], "local-model");
        }
    }
    let fetched: Vec<_> = endpoint
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|request| {
            request.headers["x-gateway-auth"]
                .to_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    assert_eq!(fetched, ["Bearer first", "Bearer second"]);
}

#[tokio::test]
async fn requests_and_catalog_observe_gateway_rotation() {
    let endpoint = MockServer::start().await;
    let gateway = GatewayProduct::new();
    let config = gateway_config(&endpoint.uri(), header_delivery());
    configure(
        &gateway.product,
        &format!("{}/product/v1", endpoint.uri()),
        &config,
        &format!("model_catalog_url = \"{}/models\"", endpoint.uri()),
    );
    gateway.seed(&config, token("first", Some("refresh-secret"), i64::MAX));
    for (token, etag) in [("first", "etag-1"), ("second", "etag-2")] {
        Mock::given(method("GET"))
            .and(path("/models"))
            .and(header("x-gateway-auth", format!("Bearer {token}")))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("etag", etag)
                    .set_body_json(json!({"models": [catalog_model("local-model")]})),
            )
            .expect(1)
            .mount(&endpoint)
            .await;
    }
    Mock::given(method("POST"))
        .and(path("/product/v1/responses"))
        .and(header("authorization", "Bearer primary-token"))
        .and(header("x-gateway-auth", "Bearer second"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("x-models-etag", "etag-2")
                .set_body_raw(
                    format!("data: {}\n\n", websocket_completion(1)),
                    "text/event-stream",
                ),
        )
        .expect(1)
        .mount(&endpoint)
        .await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"access_token": "second", "expires_in": 3600})),
        )
        .expect(1)
        .mount(&endpoint)
        .await;

    let server = gateway.product.server(None);
    let mut connection = server.connect(1).await;
    let (thread, _) = start(&server, &mut connection, json!({})).await;
    // The gateway rejected the first token elsewhere; the shared manager rotates it.
    let manager = shared_manager(&config, &gateway.home).expect("thread shares the gateway");
    assert_eq!(
        manager.refresh_access_token("first").await.unwrap(),
        "second"
    );
    let completed = completed_turn(&server, &mut connection, &thread, "hello").await;
    assert_eq!(completed["turn"]["status"], "completed", "{completed}");
    // The new catalog revision advertised by the response refetches with the rotated token.
    timeout(PATIENCE, async {
        loop {
            let requests = endpoint.received_requests().await.unwrap();
            if requests.iter().any(|request| {
                request.url.path() == "/models"
                    && request.headers["x-gateway-auth"] == "Bearer second"
            }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn invalid_gateway_configuration_fails_thread_start() {
    let gateway = GatewayProduct::new();
    let mut config = gateway_config("http://127.0.0.1:9", header_delivery());
    config.client_id = String::new();
    configure(&gateway.product, "http://127.0.0.1:9/v1", &config, "");
    let server = gateway.product.server(None);
    let mut connection = server.connect(1).await;
    let error = server
        .call_error(&mut connection, "thread/start", json!({}))
        .await;
    let message = error["message"].as_str().unwrap();
    assert!(
        message.contains("gateway_oauth requires a nonempty client_id"),
        "{message}"
    );
}

#[tokio::test]
async fn gateway_login_states_stop_request_retries() {
    let endpoint = MockServer::start().await;
    let gateway = GatewayProduct::new();
    let config = gateway_config(&endpoint.uri(), header_delivery());
    configure(
        &gateway.product,
        &format!("{}/product/v1", endpoint.uri()),
        &config,
        "stream_max_retries = 3",
    );
    let server = gateway.product.server(None);
    let mut connection = server.connect(1).await;
    let (thread, _) = start(&server, &mut connection, json!({})).await;
    let manager = shared_manager(&config, &gateway.home).expect("thread shares the gateway");
    // No credential is stored; explicit sign-in keeps the manager from opening a browser.
    ra_login::test_api::control(&manager).require_explicit_login();

    let result = completed_turn(&server, &mut connection, &thread, "hello").await;
    assert_eq!(result["turn"]["status"], "failed", "{result}");
    let message = result["turn"]["error"]["message"].as_str().unwrap();
    assert!(
        message.contains(&ra_login::GatewayAuthError::LoginRequired.to_string()),
        "{message}"
    );

    let cancel = tokio::sync::Notify::new();
    let (url_tx, url_rx) = tokio::sync::oneshot::channel();
    let login = manager.login_with_browser(cancel.notified(), |_| {
        let _ = url_tx.send(());
    });
    let check = async {
        url_rx.await.unwrap();
        let result = completed_turn(&server, &mut connection, &thread, "again").await;
        assert_eq!(result["turn"]["status"], "failed", "{result}");
        let message = result["turn"]["error"]["message"].as_str().unwrap();
        assert!(
            message.contains(&ra_login::GatewayAuthError::LoginInProgress.to_string()),
            "{message}"
        );
        cancel.notify_one();
    };
    let (login, ()) = tokio::join!(login, check);
    assert_eq!(login.unwrap_err().kind(), std::io::ErrorKind::Interrupted);
    assert!(endpoint.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn websocket_handshake_carries_primary_and_gateway_credentials() {
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::{
        accept_hdr_async,
        tungstenite::{
            Message as Frame,
            handshake::server::{Request, Response},
        },
    };

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let gateway = GatewayProduct::new();
    let config = gateway_config(&format!("http://{address}"), header_delivery());
    configure(
        &gateway.product,
        &format!("http://{address}/product/v1"),
        &config,
        "supports_websockets = true\nstream_max_retries = 0",
    );
    gateway.seed(&config, token("gateway-token", None, i64::MAX));
    let peer = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut ws = accept_hdr_async(socket, |request: &Request, response: Response| {
            assert_eq!(request.headers()["authorization"], "Bearer primary-token");
            assert_eq!(request.headers()["x-gateway-auth"], "Bearer gateway-token");
            Ok(response)
        })
        .await
        .unwrap();
        let frame = ws.next().await.unwrap().unwrap();
        let request: Value = serde_json::from_str(frame.to_text().unwrap()).unwrap();
        assert_eq!(request["type"], "response.create");
        ws.send(Frame::Text(websocket_completion(1).to_string().into()))
            .await
            .unwrap();
    });

    let server = gateway.product.server(None);
    let mut connection = server.connect(1).await;
    let (thread, _) = start(&server, &mut connection, json!({})).await;
    let completed = completed_turn(&server, &mut connection, &thread, "hello").await;
    assert_eq!(completed["turn"]["status"], "completed", "{completed}");
    timeout(PATIENCE, peer).await.unwrap().unwrap();
}
