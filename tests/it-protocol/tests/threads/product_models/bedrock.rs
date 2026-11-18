//! Product threads on Amazon Bedrock providers through a local Responses endpoint.

use super::*;
use wiremock::matchers::{header, header_regex};

const SESSION_TOKEN: &str = "exported-session-token";

/// Configures a Bedrock provider whose credentials come from an exporter script.
///
/// The script appends a line to `invocations`, then prints `keys.json` on its first run and
/// `rotated.json`, when present, on later runs.
fn configure_credential_export(product: &ProductModels, provider: &str, base_url: &str) {
    let home = product.home.path().canonicalize().unwrap();
    product.write(
        "export.sh",
        "printf 'invoked\\n' >> \"$1/invocations\"\n\
if [ \"$(wc -l < \"$1/invocations\")\" -gt 1 ] && [ -f \"$1/rotated.json\" ]; then\n\
  cat \"$1/rotated.json\"\nelse\n  cat \"$1/keys.json\"\nfi\n",
    );
    write_keys(product, "keys.json", "exported");
    product.write(
        "config.toml",
        &format!(
            r#"
approval_policy = "never"
sandbox_mode = "read-only"
model_provider = "{provider}"
[model_providers.{provider}]
base_url = "{base_url}"
[model_providers.{provider}.aws]
region = "us-west-2"
[model_providers.{provider}.aws.credential_export]
command = "/bin/sh"
args = [{script:?}, {home:?}]
"#,
            script = home.join("export.sh"),
        ),
    );
}

fn write_keys(product: &ProductModels, file: &str, access_key: &str) {
    product.write(
        file,
        &json!({
            "Version": 1,
            "AccessKeyId": access_key,
            "SecretAccessKey": "exported-secret",
            "SessionToken": SESSION_TOKEN,
        })
        .to_string(),
    );
}

fn invocations(product: &ProductModels) -> usize {
    std::fs::read_to_string(product.home.path().join("invocations"))
        .map(|text| text.lines().count())
        .unwrap_or(0)
}

fn completion() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(
        format!("data: {}\n\n", websocket_completion(1)),
        "text/event-stream",
    )
}

fn responses_requests(requests: &[wiremock::Request]) -> Vec<&wiremock::Request> {
    requests
        .iter()
        .filter(|request| request.url.path().ends_with("/responses"))
        .collect()
}

#[cfg(unix)]
#[tokio::test]
async fn bedrock_mantle_signs_requests_with_exported_credentials() {
    let endpoint = MockServer::start().await;
    let product = ProductModels::new();
    configure_credential_export(
        &product,
        "amazon-bedrock",
        &format!("{}/mantle/openai/v1", endpoint.uri()),
    );
    Mock::given(method("POST"))
        .and(path("/mantle/openai/v1/responses"))
        .and(header_regex(
            "authorization",
            r"^AWS4-HMAC-SHA256 Credential=exported/\d{8}/us-west-2/bedrock-mantle/aws4_request, SignedHeaders=[^,]+, Signature=[0-9a-f]{64}$",
        ))
        .and(header("x-amz-security-token", SESSION_TOKEN))
        .and(header("x-amzn-mantle-client-agent", "codex"))
        .respond_with(completion())
        .expect(1)
        .mount(&endpoint)
        .await;

    let server = product.server(None);
    let mut connection = server.connect(1).await;
    let (thread, started) = start(&server, &mut connection, json!({})).await;
    assert_eq!(started["model"], "openai.gpt-6.1-sol");
    assert_eq!(invocations(&product), 0, "credentials load lazily");
    let result = completed_turn(&server, &mut connection, &thread, "Sign this").await;
    assert_eq!(result["turn"]["status"], "completed", "{result}");

    let requests = endpoint.received_requests().await.unwrap();
    let request = responses_requests(&requests)[0];
    let body: Value = request.body_json().unwrap();
    assert_eq!(body["model"], "openai.gpt-6.1-sol");
    assert!(request.headers.contains_key("x-amz-date"));
    assert!(
        request
            .headers
            .keys()
            .all(|name| !name.as_str().contains('_')),
        "{:?}",
        request.headers
    );
    let signed_headers = request.headers["authorization"]
        .to_str()
        .unwrap()
        .split("SignedHeaders=")
        .nth(1)
        .unwrap()
        .split(',')
        .next()
        .unwrap()
        .to_owned();
    for name in ["content-type", "host", "x-amz-date", "x-amz-security-token"] {
        assert!(
            signed_headers.split(';').any(|signed| signed == name),
            "{name} in {signed_headers}"
        );
    }
    assert!(request.headers.get("x-api-key").is_none());
    assert_eq!(invocations(&product), 1);
    endpoint.verify().await;
}

#[cfg(unix)]
#[tokio::test]
async fn bedrock_expired_token_reexports_credentials_and_retries_once() {
    let endpoint = MockServer::start().await;
    let product = ProductModels::new();
    configure_credential_export(
        &product,
        "amazon-bedrock",
        &format!("{}/mantle/openai/v1", endpoint.uri()),
    );
    Mock::given(method("POST"))
        .and(path("/mantle/openai/v1/responses"))
        .and(header_regex("authorization", "Credential=exported/"))
        .respond_with(
            ResponseTemplate::new(403)
                .set_body_json(json!({"__type": "ExpiredTokenException", "message": "expired"})),
        )
        .expect(1)
        .mount(&endpoint)
        .await;
    Mock::given(method("POST"))
        .and(path("/mantle/openai/v1/responses"))
        .and(header_regex("authorization", "Credential=rotated/"))
        .respond_with(completion())
        .expect(1)
        .mount(&endpoint)
        .await;

    let server = product.server(None);
    let mut connection = server.connect(1).await;
    let (thread, _) = start(&server, &mut connection, json!({})).await;
    write_keys(&product, "rotated.json", "rotated");
    let result = completed_turn(&server, &mut connection, &thread, "Recover").await;
    assert_eq!(result["turn"]["status"], "completed", "{result}");
    assert_eq!(invocations(&product), 2);
    endpoint.verify().await;
}

#[cfg(unix)]
#[tokio::test]
async fn bedrock_command_bearer_refreshes_once_and_explains_an_expired_signature() {
    let endpoint = MockServer::start().await;
    let product = ProductModels::new();
    product.write(
        "token.sh",
        "printf 'invoked\\n' >> invocations\nprintf 'bedrock-token\\n'\n",
    );
    product.write(
        "config.toml",
        &format!(
            r#"
approval_policy = "never"
sandbox_mode = "read-only"
model_provider = "amazon-bedrock"
[model_providers.amazon-bedrock]
base_url = "{}/mantle/openai/v1"
[model_providers.amazon-bedrock.auth]
command = "sh"
args = ["token.sh"]
cwd = {:?}
refresh_interval_ms = 0
"#,
            endpoint.uri(),
            product.home.path().canonicalize().unwrap(),
        ),
    );
    Mock::given(method("POST"))
        .and(path("/mantle/openai/v1/responses"))
        .and(header("authorization", "Bearer bedrock-token"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "error": {"message": "Signature expired: 20260609T133205Z is now earlier than 20260614T062525Z"}
        })))
        .expect(2)
        .mount(&endpoint)
        .await;

    let server = product.server(None);
    let mut connection = server.connect(1).await;
    let (thread, started) = start(&server, &mut connection, json!({})).await;
    assert_eq!(started["model"], "openai.gpt-6.1-sol");
    let result = completed_turn(&server, &mut connection, &thread, "Expired").await;
    assert_eq!(result["turn"]["status"], "failed", "{result}");
    let message = result["turn"]["error"]["message"].as_str().unwrap();
    assert!(
        message
            .contains("Amazon Bedrock rejected the request because its AWS signature has expired."),
        "{message}"
    );
    assert_eq!(invocations(&product), 2);
    endpoint.verify().await;
}

#[cfg(unix)]
#[tokio::test]
async fn bedrock_runtime_uses_runtime_service_and_routed_catalog() {
    let endpoint = MockServer::start().await;
    let product = ProductModels::new();
    configure_credential_export(
        &product,
        "amazon-bedrock-runtime",
        &format!("{}/runtime/openai/v1", endpoint.uri()),
    );
    Mock::given(method("POST"))
        .and(path("/runtime/openai/v1/responses"))
        .and(header_regex(
            "authorization",
            r"Credential=exported/\d{8}/us-west-2/bedrock/aws4_request",
        ))
        .respond_with(completion())
        .expect(1)
        .mount(&endpoint)
        .await;

    let server = product.server(None);
    let mut connection = server.connect(1).await;
    let (thread, started) = start(&server, &mut connection, json!({})).await;
    assert_eq!(started["model"], "global.openai.gpt-6.1-sol");
    let result = completed_turn(&server, &mut connection, &thread, "Runtime").await;
    assert_eq!(result["turn"]["status"], "completed", "{result}");
    let requests = endpoint.received_requests().await.unwrap();
    let body: Value = responses_requests(&requests)[0].body_json().unwrap();
    assert_eq!(body["model"], "global.openai.gpt-6.1-sol");
    endpoint.verify().await;
}

#[cfg(unix)]
#[tokio::test]
async fn bedrock_govcloud_region_selects_the_govcloud_catalog() {
    let product = ProductModels::new();
    let home = product.home.path().canonicalize().unwrap();
    product.write("export.sh", "printf 'invoked\\n' >> \"$1/invocations\"\n");
    product.write(
        "config.toml",
        &format!(
            r#"
approval_policy = "never"
sandbox_mode = "read-only"
model_provider = "amazon-bedrock"
[model_providers.amazon-bedrock.aws]
region = "us-gov-west-1"
[model_providers.amazon-bedrock.aws.credential_export]
command = "/bin/sh"
args = [{script:?}, {home:?}]
"#,
            script = home.join("export.sh"),
        ),
    );
    let server = product.server(None);
    let mut connection = server.connect(1).await;
    // Only the region is configured; the derived GovCloud endpoint chooses the catalog.
    let (_, started) = start(&server, &mut connection, json!({})).await;
    assert_eq!(started["model"], "openai.gpt-5.6-terra");
    assert_eq!(invocations(&product), 0);
}

#[tokio::test]
async fn bedrock_rejects_an_unsupported_mantle_region() {
    let product = ProductModels::new();
    product.write(
        "config.toml",
        r#"
approval_policy = "never"
sandbox_mode = "read-only"
model_provider = "amazon-bedrock"
[model_providers.amazon-bedrock.aws]
region = "us-west-1"
[model_providers.amazon-bedrock.aws.credential_export]
command = "/bin/false"
"#,
    );
    let server = product.server(None);
    let mut connection = server.connect(1).await;
    let error = server
        .call_error(&mut connection, "thread/start", json!({}))
        .await;
    let message = error["message"].as_str().unwrap();
    assert!(
        message.contains("Amazon Bedrock does not support region `us-west-1`"),
        "{message}"
    );
}
