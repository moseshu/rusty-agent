// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! The `account/gatewayOAuth/*` methods, their status notifications and the gateway login policy
//! selected at `initialize`, through an app server built on the coding product.
//!
//! Ported from Codex `codex-rs/app-server/tests/suite/v2/gateway_oauth_tests.rs` and
//! `codex-rs/app-server/src/message_processor_gateway_oauth_tests.rs`. Codex probes readiness with
//! `model/list`, which is not ported here; a product turn, which needs the same gateway credential,
//! stands in for it. Codex's primary-account and managed-requirements cases are not ported: that
//! account sign-in and `model/list` are not part of this server.

use std::collections::HashMap;

use ra_coding::config::gateway_auth_test_api::shared_manager;
use ra_coding::gateway_oauth::CodingGatewayOAuth;
use ra_protocol::server::protocol::{GatewayOAuthChangedNotification, GatewayOAuthStatus};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use wiremock::matchers::{body_string_contains, header};

use super::gateway::{
    GatewayProduct, completion, configure, gateway_config, header_delivery, token,
};
use super::*;

const CHANGED: &str = "account/gatewayOAuth/changed";
const SIGN_IN_REQUIRED: &str =
    "Gateway sign-in required. Choose Sign in or Reconnect, then retry your request";
const SIGN_IN_CANCELED: &str = "Gateway sign-in was canceled";
const SIGN_IN_FAILED: &str =
    "Gateway sign-in failed; check the gateway configuration and credential store.";

/// An app server on `product` with the product's gateway sign-in installed.
async fn gateway_server(product: &ProductModels) -> Server {
    let (host, settings, builder) = product.parts();
    let startup = builder.clone().build().await.unwrap();
    let factory = CodingThreadFactory::from_product_config(host, settings, builder.clone());
    let store: Arc<dyn ThreadStore> = Arc::new(InMemoryThreadStore::new());
    let server = AppServer::new(std::env::temp_dir().join("rusty-agent-gateway-oauth-test"))
        .unwrap()
        .with_thread_store(Arc::clone(&store))
        .with_thread_factory(Arc::new(factory))
        .with_gateway_oauth(CodingGatewayOAuth::new(builder, &startup));
    Server::run(server, store)
}

fn explicit() -> InitializeCapabilities {
    InitializeCapabilities::new()
        .with_experimental_api(true)
        .with_explicit_gateway_oauth(true)
}

/// The next status change sent to `connection` that carries an authorization URL.
async fn authorization_url(connection: &mut Connection) -> url::Url {
    let changed = connection
        .until(|message| message["method"] == CHANGED && message["params"]["authUrl"].is_string())
        .await;
    assert_eq!(changed["params"]["status"], "started", "{changed}");
    url::Url::parse(changed["params"]["authUrl"].as_str().unwrap()).unwrap()
}

/// The next status change sent to `connection` with `status`.
async fn changed_to(connection: &mut Connection, status: &str) -> Value {
    connection
        .until(|message| message["method"] == CHANGED && message["params"]["status"] == status)
        .await["params"]
        .clone()
}

/// Delivers `code` to the loopback callback the authorization URL redirects to, as the browser
/// would.
async fn authorize(authorization_url: &url::Url, code: &str) {
    let query = authorization_url
        .query_pairs()
        .into_owned()
        .collect::<HashMap<_, _>>();
    let mut callback = url::Url::parse(&query["redirect_uri"]).unwrap();
    callback
        .query_pairs_mut()
        .append_pair("code", code)
        .append_pair("state", &query["state"]);
    let authority = format!(
        "{}:{}",
        callback.host_str().unwrap(),
        callback.port().unwrap()
    );
    let mut stream = tokio::net::TcpStream::connect(&authority).await.unwrap();
    let request = format!(
        "GET {}?{} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n",
        callback.path(),
        callback.query().unwrap()
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
}

/// Changes routing after startup without changing the gateway's OAuth account.
fn change_network_config(product: &ProductModels) {
    let file = product.home.path().join("config.toml");
    let mut config = std::fs::read_to_string(&file).unwrap();
    config.push_str("\n[features]\nsystem_proxy_fallback = false\n");
    std::fs::write(file, config).unwrap();
}

async fn read_gateway(server: &Server, connection: &mut Connection) -> Value {
    server
        .call(connection, "account/gatewayOAuth/read", Value::Null)
        .await
}

async fn turn_error(server: &Server, connection: &mut Connection) -> String {
    let (thread, _) = start(server, connection, json!({})).await;
    let completed = completed_turn(server, connection, &thread, "hello").await;
    assert_eq!(completed["turn"]["status"], "failed", "{completed}");
    completed["turn"]["error"]["message"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[tokio::test]
async fn login_saves_the_credential_for_existing_requests() {
    let endpoint = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .and(body_string_contains("grant_type=authorization_code"))
        .and(body_string_contains("code=accepted"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "new-access", "refresh_token": "new-refresh", "expires_in": 3600
        })))
        .expect(1)
        .mount(&endpoint)
        .await;
    Mock::given(method("POST"))
        .and(path("/product/v1/responses"))
        .and(header("x-gateway-auth", "Bearer new-access"))
        .respond_with(completion())
        .expect(1)
        .mount(&endpoint)
        .await;
    let gateway = GatewayProduct::new();
    let config = gateway_config(&endpoint.uri(), header_delivery());
    configure(
        &gateway.product,
        &format!("{}/product/v1", endpoint.uri()),
        &config,
        "",
    );
    gateway.seed(&config, token("old-access", Some("old-refresh"), i64::MAX));
    let server = gateway_server(&gateway.product).await;
    // A client without the capability keeps automatic login and may still sign in explicitly.
    let mut connection = server.connect(1).await;
    let (thread, _) = start(&server, &mut connection, json!({})).await;
    // The new token must reach the thread started before the routing change.
    change_network_config(&gateway.product);

    let login = server
        .request(&mut connection, "account/gatewayOAuth/login", Value::Null)
        .await;
    let url = authorization_url(&mut connection).await;
    authorize(&url, "accepted").await;
    assert_eq!(connection.response(login).await, json!({}));

    assert_eq!(
        read_gateway(&server, &mut connection).await,
        json!({
            "providerId": "gateway", "providerName": "Gateway", "required": true,
            "status": "succeeded", "error": null,
        })
    );
    let manager = shared_manager(&config, &gateway.home).expect("the product shares the gateway");
    assert_eq!(manager.resolve_access_token().await.unwrap(), "new-access");
    let completed = completed_turn(&server, &mut connection, &thread, "hello").await;
    assert_eq!(completed["turn"]["status"], "completed", "{completed}");
}

#[tokio::test]
async fn token_exchange_failure_completes_login_and_updates_readiness() {
    let endpoint = MockServer::start().await;
    let secret = "issuer-query-credential";
    Mock::given(method("POST"))
        .and(path("/token"))
        .and(body_string_contains("grant_type=authorization_code"))
        .and(body_string_contains("code=accepted"))
        .respond_with(
            ResponseTemplate::new(400)
                .insert_header("x-request-id", secret)
                .set_body_json(json!({"error": "invalid_grant", "error_description": secret})),
        )
        .expect(1)
        .mount(&endpoint)
        .await;
    let gateway = GatewayProduct::new();
    let mut config = gateway_config(&endpoint.uri(), header_delivery());
    config.authorization_url = format!("{}/authorize?issuer_secret={secret}", endpoint.uri());
    configure(
        &gateway.product,
        &format!("{}/v1", endpoint.uri()),
        &config,
        "",
    );
    let server = gateway_server(&gateway.product).await;
    let mut connection = server.connect_with(1, explicit()).await;

    let login = server
        .request(&mut connection, "account/gatewayOAuth/login", Value::Null)
        .await;
    let url = authorization_url(&mut connection).await;
    authorize(&url, "accepted").await;
    let error = connection.error(login).await;
    assert_eq!(error["message"], SIGN_IN_FAILED, "{error}");
    assert_eq!(
        changed_to(&mut connection, "failed").await,
        json!({
            "authUrl": null, "providerId": "gateway", "status": "failed",
            "error": SIGN_IN_FAILED,
        })
    );
    assert_eq!(
        read_gateway(&server, &mut connection).await,
        json!({
            "providerId": "gateway", "providerName": "Gateway", "required": true,
            "status": "failed", "error": SIGN_IN_FAILED,
        })
    );
    for message in connection.drain().await {
        assert!(!message.to_string().contains(secret), "{message}");
    }
}

#[tokio::test]
async fn read_and_cancel_without_gateway_oauth() {
    let product = ProductModels::new();
    product.write(
        "config.toml",
        "approval_policy = \"never\"\nsandbox_mode = \"read-only\"\n",
    );
    let server = gateway_server(&product).await;
    let mut connection = server.connect_with(1, explicit()).await;

    let state = read_gateway(&server, &mut connection).await;
    assert_eq!(
        (&state["required"], &state["status"], &state["error"]),
        (&json!(false), &Value::Null, &Value::Null),
        "{state}"
    );
    assert_eq!(
        server
            .call(&mut connection, "account/gatewayOAuth/cancel", Value::Null)
            .await,
        json!({})
    );
    let error = server
        .call_error(&mut connection, "account/gatewayOAuth/login", Value::Null)
        .await;
    assert_eq!(
        error,
        invalid_request("The current provider does not use gateway OAuth")
    );
}

#[tokio::test]
async fn read_is_passive_and_canceled_login_can_be_retried_immediately() {
    let endpoint = MockServer::start().await;
    let gateway = GatewayProduct::new();
    let config = gateway_config(&endpoint.uri(), header_delivery());
    configure(
        &gateway.product,
        &format!("{}/v1", endpoint.uri()),
        &config,
        "",
    );
    let server = gateway_server(&gateway.product).await;
    let mut connection = server.connect_with(1, explicit()).await;

    assert_eq!(
        read_gateway(&server, &mut connection).await,
        json!({
            "providerId": "gateway", "providerName": "Gateway", "required": true,
            "status": "notReady", "error": null,
        })
    );
    assert!(endpoint.received_requests().await.unwrap().is_empty());
    let expected = json!({
        "authUrl": null, "providerId": "gateway", "status": "notReady", "error": null,
    });
    // Consume the initial readiness change before checking repeated request failures.
    assert_eq!(connection.notification(CHANGED).await, expected);
    for _ in 0..2 {
        let message = turn_error(&server, &mut connection).await;
        assert!(message.contains(SIGN_IN_REQUIRED), "{message}");
        assert_eq!(connection.notification(CHANGED).await, expected);
    }

    let login = server
        .request(&mut connection, "account/gatewayOAuth/login", Value::Null)
        .await;
    let url = authorization_url(&mut connection).await;
    assert!(
        url.as_str()
            .starts_with(&format!("{}/authorize?", endpoint.uri())),
        "{url}"
    );
    assert_eq!(
        read_gateway(&server, &mut connection).await["status"],
        "started"
    );
    let duplicate = server
        .call_error(&mut connection, "account/gatewayOAuth/login", Value::Null)
        .await;
    assert_eq!(
        duplicate,
        invalid_request("Gateway sign-in is already in progress")
    );
    assert_eq!(
        server
            .call(&mut connection, "account/gatewayOAuth/cancel", Value::Null)
            .await,
        json!({})
    );
    // Retry on the cancel acknowledgment, without waiting for the old login response.
    let retry = server
        .request(&mut connection, "account/gatewayOAuth/login", Value::Null)
        .await;
    authorization_url(&mut connection).await;
    server
        .call(&mut connection, "account/gatewayOAuth/cancel", Value::Null)
        .await;
    for request in [login, retry] {
        let error = connection.error(request).await;
        assert_eq!(error["message"], SIGN_IN_CANCELED, "{error}");
    }
    assert!(endpoint.received_requests().await.unwrap().is_empty());

    let state = read_gateway(&server, &mut connection).await;
    assert_eq!(
        (&state["status"], &state["error"]),
        (&json!("failed"), &json!(SIGN_IN_CANCELED)),
        "{state}"
    );
}

#[tokio::test]
async fn disconnect_cancels_only_the_owning_connections_login() {
    let endpoint = MockServer::start().await;
    let gateway = GatewayProduct::new();
    let config = gateway_config(&endpoint.uri(), header_delivery());
    configure(
        &gateway.product,
        &format!("{}/v1", endpoint.uri()),
        &config,
        "",
    );
    let server = gateway_server(&gateway.product).await;
    let mut owner = server.connect_with(1, explicit()).await;
    let mut observer = server.connect(2).await;
    // A legacy connection cannot undo another connection's explicit opt-in.
    let message = turn_error(&server, &mut observer).await;
    assert!(message.contains(SIGN_IN_REQUIRED), "{message}");

    server
        .request(&mut owner, "account/gatewayOAuth/login", Value::Null)
        .await;
    // Readiness may be published before the login request starts authorization.
    let started = changed_to(&mut observer, "started").await;
    assert_eq!(started["authUrl"], Value::Null, "{started}");
    authorization_url(&mut owner).await;
    let error = server
        .call_error(&mut observer, "account/gatewayOAuth/cancel", Value::Null)
        .await;
    assert_eq!(
        error,
        invalid_request("Gateway sign-in belongs to another connection")
    );

    server.disconnect(owner).await;
    let canceled = changed_to(&mut observer, "failed").await;
    assert_eq!(canceled["error"], SIGN_IN_CANCELED, "{canceled}");
    let retry = server
        .request(&mut observer, "account/gatewayOAuth/login", Value::Null)
        .await;
    authorization_url(&mut observer).await;
    server
        .call(&mut observer, "account/gatewayOAuth/cancel", Value::Null)
        .await;
    let error = observer.error(retry).await;
    assert_eq!(error["message"], SIGN_IN_CANCELED, "{error}");
    assert!(endpoint.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn initialization_selects_legacy_or_explicit_gateway_login() {
    for explicit_login in [false, true] {
        let endpoint = MockServer::start().await;
        // Force automatic authorization to fail before launching a real browser.
        let callback = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = callback.local_addr().unwrap().port();
        let gateway = GatewayProduct::new();
        let mut config = gateway_config(&endpoint.uri(), header_delivery());
        config.redirect_port = Some(port);
        let base_url = format!("{}/v1", endpoint.uri());
        configure(
            &gateway.product,
            &base_url,
            &config,
            "stream_max_retries = 0",
        );
        let server = gateway_server(&gateway.product).await;
        let mut connection = server
            .connect_with(
                1,
                InitializeCapabilities::new()
                    .with_experimental_api(true)
                    .with_explicit_gateway_oauth(explicit_login),
            )
            .await;
        // A manager created after initialization observes the selected policy.
        config.client_id = "gateway-after-initialize".into();
        configure(
            &gateway.product,
            &base_url,
            &config,
            "stream_max_retries = 0",
        );

        let message = turn_error(&server, &mut connection).await;
        let expected = if explicit_login {
            SIGN_IN_REQUIRED
        } else {
            "Gateway OAuth authentication failed; check the gateway configuration and credential store."
        };
        assert!(message.contains(expected), "{explicit_login}: {message}");
        assert!(endpoint.received_requests().await.unwrap().is_empty());
        drop(callback);
    }
}

#[tokio::test]
async fn login_requires_status_notifications() {
    let gateway = GatewayProduct::new();
    let config = gateway_config("http://127.0.0.1:9", header_delivery());
    configure(&gateway.product, "http://127.0.0.1:9/v1", &config, "");
    let server = gateway_server(&gateway.product).await;
    let mut connection = server
        .connect_with(1, explicit().with_opt_out_notification_methods([CHANGED]))
        .await;
    let error = server
        .call_error(&mut connection, "account/gatewayOAuth/login", Value::Null)
        .await;
    assert_eq!(
        error,
        invalid_request("Gateway login requires account/gatewayOAuth/changed notifications")
    );
}

#[tokio::test]
async fn gateway_methods_need_an_installed_product_port() {
    let product = ProductModels::new();
    let server = product.server(None);
    let mut connection = server.connect(1).await;
    for method in ["account/gatewayOAuth/read", "account/gatewayOAuth/login"] {
        let error = server
            .call_error(&mut connection, method, Value::Null)
            .await;
        assert_eq!(error, invalid_request("Gateway OAuth is not configured"));
    }
    // With no login running there is nothing to cancel, as in Codex.
    assert_eq!(
        server
            .call(&mut connection, "account/gatewayOAuth/cancel", Value::Null)
            .await,
        json!({})
    );
}

#[test]
fn gateway_wire_records_keep_codex_shapes_and_hide_authorization_urls() {
    assert_eq!(
        serde_json::to_value(InitializeCapabilities::new()).unwrap(),
        json!({"experimentalApi": false, "optOutNotificationMethods": null})
    );
    assert_eq!(
        serde_json::to_value(InitializeCapabilities::new().with_explicit_gateway_oauth(true))
            .unwrap()["explicitGatewayOauth"],
        true
    );
    let changed =
        GatewayOAuthChangedNotification::new("gateway", GatewayOAuthStatus::Started, None)
            .with_auth_url("https://issuer.example/authorize?state=private-state");
    assert!(!format!("{changed:?}").contains("private-state"));
    assert_eq!(
        serde_json::to_value(&changed).unwrap(),
        json!({
            "authUrl": "https://issuer.example/authorize?state=private-state",
            "providerId": "gateway", "status": "started", "error": null,
        })
    );
}

#[tokio::test]
async fn network_config_reload_preserves_explicit_login() {
    let endpoint = MockServer::start().await;
    // Automatic authorization would fail here before launching a real browser.
    let callback = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let gateway = GatewayProduct::new();
    let mut config = gateway_config(&endpoint.uri(), header_delivery());
    config.redirect_port = Some(callback.local_addr().unwrap().port());
    configure(
        &gateway.product,
        &format!("{}/v1", endpoint.uri()),
        &config,
        "stream_max_retries = 0",
    );
    let server = gateway_server(&gateway.product).await;
    let mut connection = server.connect_with(1, explicit()).await;
    change_network_config(&gateway.product);

    let message = turn_error(&server, &mut connection).await;
    assert!(message.contains(SIGN_IN_REQUIRED), "{message}");
    assert!(endpoint.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn network_config_reload_preserves_status_notifications() {
    let endpoint = MockServer::start().await;
    let gateway = GatewayProduct::new();
    let config = gateway_config(&endpoint.uri(), header_delivery());
    configure(
        &gateway.product,
        &format!("{}/v1", endpoint.uri()),
        &config,
        "",
    );
    let server = gateway_server(&gateway.product).await;
    let mut connection = server.connect_with(1, explicit()).await;
    change_network_config(&gateway.product);

    assert_eq!(
        read_gateway(&server, &mut connection).await["status"],
        "notReady"
    );
    assert_eq!(
        changed_to(&mut connection, "notReady").await,
        json!({
            "authUrl": null, "providerId": "gateway", "status": "notReady", "error": null,
        })
    );
    let login = server
        .request(&mut connection, "account/gatewayOAuth/login", Value::Null)
        .await;
    authorization_url(&mut connection).await;
    server
        .call(&mut connection, "account/gatewayOAuth/cancel", Value::Null)
        .await;
    assert_eq!(connection.error(login).await["message"], SIGN_IN_CANCELED);
    assert_eq!(
        changed_to(&mut connection, "failed").await["error"],
        SIGN_IN_CANCELED
    );
}
