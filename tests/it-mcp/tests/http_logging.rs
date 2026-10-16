//! Transport diagnostics never retain credential-bearing requests, responses or auth failures.

use ra_core::error::Error;
use ra_mcp::client::{McpClient, McpClientOptions, http::McpHttpParams};
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::any};

#[derive(Clone, Default)]
struct CapturedLogs(Arc<Mutex<Vec<u8>>>);
impl std::io::Write for CapturedLogs {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturedLogs {
    type Writer = Self;
    fn make_writer(&'a self) -> Self {
        self.clone()
    }
}

#[tokio::test]
async fn http_errors_are_credential_safe_in_display_debug_sources_and_logs() {
    let captured = CapturedLogs::default();
    tracing::subscriber::set_global_default(
        tracing_subscriber::fmt()
            .with_writer(captured.clone())
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .without_time()
            .finish(),
    )
    .unwrap();
    let server = MockServer::start().await;
    let mode = Arc::new(AtomicUsize::new(0));
    let response_mode = mode.clone();
    Mock::given(any())
        .respond_with(move |request: &wiremock::Request| {
            let payload: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
            let method = payload["method"].as_str().unwrap_or("");
            let mode = response_mode.load(Ordering::SeqCst);
            if request.method.as_str() == "GET" {
                return ResponseTemplate::new(405);
            }
            if request.method.as_str() == "DELETE" {
                return ResponseTemplate::new(500).set_body_string("body-credential");
            }
            if mode < 3 {
                // A JSON-RPC error body is a protocol answer; any other error body is transport data.
                return ResponseTemplate::new([401, 403, 503][mode])
                    .insert_header("www-authenticate", "Bearer realm=challenge-credential")
                    .set_body_json(json!({"detail":"body-credential","id":payload["id"]}));
            }
            match method {
                "initialize" => ResponseTemplate::new(200)
                    .insert_header("mcp-session-id", "session")
                    .set_body_json(json!({"jsonrpc":"2.0","id":payload["id"],"result":{
                    "protocolVersion":"2025-11-25","serverInfo":{"name":"fixture","version":"1"},
                    "capabilities":{"tools":{},"resources":{},"prompts":{}}}})),
                "notifications/initialized" => ResponseTemplate::new(202),
                _ if method.starts_with("notifications/") => ResponseTemplate::new(202),
                _ if mode == 3 => ResponseTemplate::new(500).set_body_string("body-credential"),
                _ if mode == 4 => ResponseTemplate::new(200)
                    .insert_header("content-type", "type-credential")
                    .set_body_string("body-credential"),
                _ if mode == 5 => ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .set_body_string("{\"body-credential\":[]}"),
                _ => ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string("data: {\"body-credential\":[]}\n\n"),
            }
        })
        .mount(&server)
        .await;
    let params = || {
        McpHttpParams::new(format!(
            "http://url-user:url-password@{}/mcp?token=query-credential#fragment-credential",
            server.address()
        ))
        .with_headers(std::collections::BTreeMap::from([
            ("authorization".into(), "Bearer header-credential".into()),
            ("x-api-key".into(), "custom-credential".into()),
        ]))
    };
    let mut errors = Vec::new();
    for response_mode in 0..3 {
        mode.store(response_mode, Ordering::SeqCst);
        errors.push(
            McpClient::connect_http("http", params(), McpClientOptions::default())
                .await
                .err()
                .unwrap(),
        );
    }
    for response_mode in 3..7 {
        mode.store(response_mode, Ordering::SeqCst);
        let client = McpClient::connect_http("http", params(), McpClientOptions::default())
            .await
            .unwrap();
        errors.push(client.list_tools().await.unwrap_err());
        errors.push(client.list_prompts().await.unwrap_err());
        errors.push(client.get_prompt("review", None).await.unwrap_err());
        errors.push(client.list_resources(None).await.unwrap_err());
        errors.push(client.list_resource_templates(None).await.unwrap_err());
        errors.push(client.read_resource("memo://note").await.unwrap_err());
        errors.push(client.call_tool("echo", None, None).await.unwrap_err());
        // The SDK logs teardown failures without raising them.
        client.cleanup().await.unwrap();
    }
    errors.push(
        McpClient::connect_http(
            "http",
            params().with_auth(|_| async { Err(Error::config("auth-credential")) }),
            McpClientOptions::default(),
        )
        .await
        .err()
        .unwrap(),
    );
    let mut rendered = String::new();
    for error in errors {
        rendered.push_str(&format!("{error:?} {error}"));
        let mut source = std::error::Error::source(&error);
        while let Some(error) = source {
            rendered.push_str(&format!("{error:?} {error}"));
            source = error.source();
        }
    }
    rendered.push_str(&String::from_utf8(captured.0.lock().unwrap().clone()).unwrap());
    for secret in [
        "url-user",
        "url-password",
        "query-credential",
        "fragment-credential",
        "header-credential",
        "custom-credential",
        "challenge-credential",
        "body-credential",
        "type-credential",
        "auth-credential",
    ] {
        assert!(!rendered.contains(secret), "diagnostics retained {secret}");
    }
}
