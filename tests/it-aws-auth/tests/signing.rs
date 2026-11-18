//! SigV4 signing, caller-supplied credentials, and error classification.
//!
//! Ported from the unit tests in Codex `codex-rs/aws-auth/src/lib.rs`
//! (openai/codex @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052).

use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use aws_credential_types::Credentials;
use aws_credential_types::provider::SharedCredentialsProvider;
use aws_credential_types::provider::error::CredentialsError;
use bytes::Bytes;
use http::{HeaderMap, Method};
use ra_aws_auth::{
    AwsAccessKeys, AwsAuthConfig, AwsAuthContext, AwsAuthError, AwsCredentialsProvider,
    AwsRequestToSign, test_api,
};

fn test_context(session_token: Option<&str>) -> AwsAuthContext {
    test_api::context(
        ra_http::NetworkPolicy::default(),
        SharedCredentialsProvider::new(Credentials::new(
            "AKIDEXAMPLE",
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            session_token.map(str::to_string),
            /*expires_after*/ None,
            "unit-test",
        )),
        "us-east-1",
        "bedrock",
    )
}

fn test_request() -> AwsRequestToSign {
    let mut headers = HeaderMap::new();
    headers.insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("application/json"),
    );
    headers.insert("x-test-header", http::HeaderValue::from_static("present"));
    AwsRequestToSign {
        method: Method::POST,
        url: "https://bedrock-runtime.us-east-1.amazonaws.com/v1/responses".to_string(),
        headers,
        body: Bytes::from_static(br#"{"model":"openai.gpt-oss-120b-1:0"}"#),
    }
}

fn header_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

#[tokio::test]
async fn sign_adds_sigv4_headers_and_preserves_existing_headers() {
    let signed = test_api::sign_at(
        &test_context(None),
        test_request(),
        UNIX_EPOCH + Duration::from_secs(1_700_000_000),
    )
    .await
    .unwrap();

    assert_eq!(
        header_value(&signed.headers, http::header::CONTENT_TYPE.as_str()),
        Some("application/json".to_string())
    );
    assert_eq!(
        header_value(&signed.headers, "x-test-header"),
        Some("present".to_string())
    );
    assert_eq!(
        signed.url,
        "https://bedrock-runtime.us-east-1.amazonaws.com/v1/responses"
    );
    let authorization =
        header_value(&signed.headers, http::header::AUTHORIZATION.as_str()).unwrap();
    assert!(authorization.starts_with(
        "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20231114/us-east-1/bedrock/aws4_request"
    ));
    assert_eq!(
        header_value(&signed.headers, "x-amz-date"),
        Some("20231114T221320Z".to_string())
    );
}

#[tokio::test]
async fn sign_includes_session_token_when_credentials_have_one() {
    let signed = test_api::sign_at(
        &test_context(Some("session-token")),
        test_request(),
        UNIX_EPOCH + Duration::from_secs(1_700_000_000),
    )
    .await
    .unwrap();

    assert_eq!(
        header_value(&signed.headers, "x-amz-security-token"),
        Some("session-token".to_string())
    );
}

#[tokio::test]
async fn credentials_provider_adapter_converts_keys_and_provider_failures() {
    #[derive(Debug)]
    struct TestCredentialsProvider(Result<AwsAccessKeys, std::io::ErrorKind>);

    impl AwsCredentialsProvider for TestCredentialsProvider {
        async fn credentials(&self) -> std::io::Result<AwsAccessKeys> {
            self.0
                .clone()
                .map_err(|kind| std::io::Error::new(kind, "credential export failed"))
        }
    }

    let credentials = test_api::adapted_credentials(
        Arc::new(TestCredentialsProvider(Ok(test_api::access_keys(
            "access-key-id",
            "secret-access-key",
            Some("session-token"),
        )))),
        ra_http::NetworkPolicy::default(),
    )
    .await
    .unwrap();
    assert_eq!(
        credentials,
        Credentials::new(
            "access-key-id",
            "secret-access-key",
            Some("session-token".to_string()),
            /*expires_after*/ None,
            "rusty-agent-bedrock-credential-export",
        )
    );

    for (kind, retryable) in [
        (std::io::ErrorKind::Other, true),
        (std::io::ErrorKind::TimedOut, true),
        (std::io::ErrorKind::InvalidData, false),
        (std::io::ErrorKind::InvalidInput, false),
        (std::io::ErrorKind::NotFound, false),
        (std::io::ErrorKind::PermissionDenied, false),
    ] {
        let error = test_api::adapted_credentials(
            Arc::new(TestCredentialsProvider(Err(kind))),
            ra_http::NetworkPolicy::default(),
        )
        .await
        .unwrap_err();
        let error = AwsAuthError::Credentials(error);
        assert_eq!(
            (
                error.is_retryable(),
                error
                    .credentials_provider_error()
                    .map(|error| (error.kind(), error.to_string())),
            ),
            (
                retryable,
                Some((kind, "credential export failed".to_string()))
            ),
        );
    }
}

#[test]
fn credentials_provider_failures_are_retryable() {
    assert!(
        AwsAuthError::Credentials(CredentialsError::provider_error("temporarily unavailable"))
            .is_retryable()
    );
    assert!(
        AwsAuthError::Credentials(CredentialsError::provider_timed_out(Duration::from_secs(1)))
            .is_retryable()
    );
}

#[test]
fn deterministic_aws_auth_errors_are_not_retryable() {
    assert!(!AwsAuthError::EmptyService.is_retryable());
    assert!(!AwsAuthError::MissingProfile.is_retryable());
    assert!(!AwsAuthError::Credentials(CredentialsError::not_loaded_no_source()).is_retryable());
    assert!(
        !AwsAuthError::Credentials(CredentialsError::invalid_configuration("bad profile"))
            .is_retryable()
    );
    assert!(
        !AwsAuthError::Credentials(CredentialsError::unhandled("unexpected response"))
            .is_retryable()
    );
}

#[tokio::test]
async fn load_rejects_invalid_configuration() {
    let factory = ra_http::HttpClientFactory::new(ra_http::OutboundProxyPolicy::ReqwestDefault);
    let error = AwsAuthContext::load(
        AwsAuthConfig {
            profile: None,
            region: None,
            service: "   ".to_string(),
        },
        factory.clone(),
    )
    .await
    .unwrap_err();
    assert_eq!(error.to_string(), "AWS service name must not be empty");

    let error = AwsAuthContext::load_profile(
        AwsAuthConfig {
            profile: None,
            region: Some("us-east-1".to_string()),
            service: "bedrock".to_string(),
        },
        factory,
    )
    .await
    .unwrap_err();
    assert_eq!(error.to_string(), "AWS profile must be configured");
}

#[tokio::test]
async fn access_keys_context_uses_configured_region_and_service() {
    let context = AwsAuthContext::load_with_access_keys(
        AwsAuthConfig {
            profile: None,
            region: Some("eu-central-1".to_string()),
            service: " bedrock-mantle ".to_string(),
        },
        test_api::access_keys("access-key-id", "secret-access-key", None),
        ra_http::HttpClientFactory::new(ra_http::OutboundProxyPolicy::ReqwestDefault),
    )
    .await
    .unwrap();
    assert_eq!(
        (context.region(), context.service()),
        ("eu-central-1", "bedrock-mantle")
    );
    assert!(!format!("{context:?}").contains("secret-access-key"));
}

#[tokio::test]
async fn real_imds_credentials_stop_after_policy_revocation() {
    use aws_config::provider_config::ProviderConfig;
    use ra_http::DestinationPolicy;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::time::timeout;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let controller = ra_http::NetworkPolicyController::default();
    let policy = controller.policy();
    let factory = ra_http::HttpClientFactory::new(ra_http::OutboundProxyPolicy::ReqwestDefault)
        .with_network_policy(policy.clone().for_current_account());
    let http_client = test_api::sdk_http_client(factory);
    let imds_client = aws_config::imds::Client::builder()
        .configure(&ProviderConfig::default().with_http_client(http_client))
        .endpoint(format!("http://{}", listener.local_addr().unwrap()))
        .unwrap()
        .build();
    let imds_credentials = || {
        aws_config::imds::credentials::ImdsCredentialsProvider::builder()
            .profile("test-role")
            .imds_client(imds_client.clone())
            .build()
    };
    let context = test_api::context(
        policy.clone().for_current_account(),
        SharedCredentialsProvider::new(imds_credentials()),
        "us-east-1",
        "bedrock",
    );
    let request = test_request();
    let server = async {
        let mut credentials_sent = false;
        loop {
            let (mut socket, _) = listener.accept().await?;
            let mut headers = Vec::new();
            while !headers.ends_with(b"\r\n\r\n") {
                headers.push(socket.read_u8().await?);
            }
            let body = if headers.starts_with(b"PUT /latest/api/token ") {
                "test-token"
            } else if credentials_sent {
                return Ok::<_, std::io::Error>(socket);
            } else {
                credentials_sent = true;
                r#"{"Code":"Success","AccessKeyId":"test-key","SecretAccessKey":"test-secret","Token":"test-session","Expiration":"2099-01-01T00:00:00Z"}"#
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\nx-aws-ec2-metadata-token-ttl-seconds: 21600\r\nConnection: close\r\n\r\n{body}"
            );
            socket.write_all(response.as_bytes()).await?;
        }
    };
    assert!(controller.publish(policy.revision(), DestinationPolicy::Unrestricted));
    let signing = async {
        context.sign(request.clone()).await.unwrap();
        context.sign(request.clone()).await
    };
    tokio::pin!(signing);
    let wait = Duration::from_secs(5);
    let mut socket = tokio::select! {
        result = &mut signing => panic!("signing finished before revocation: {result:?}"),
        socket = timeout(wait, server) => socket.unwrap().unwrap(),
    };
    assert!(controller.publish(
        policy.revision(),
        DestinationPolicy::Restricted {
            allowed_hosts: Default::default(),
        },
    ));
    let revoked = timeout(wait, signing).await.unwrap();
    assert!(
        matches!(
            revoked,
            Err(AwsAuthError::Policy(ra_http::NetworkPolicyDenied::Revoked))
        ),
        "{revoked:?}"
    );
    assert_eq!(
        timeout(wait, socket.read(&mut [0])).await.unwrap().unwrap(),
        0
    );

    // The model host remains allowed while metadata traffic is denied before connecting.
    assert!(controller.publish(
        policy.revision(),
        DestinationPolicy::Restricted {
            allowed_hosts: ["bedrock-runtime.us-east-1.amazonaws.com".to_string()].into(),
        },
    ));
    context.sign(request.clone()).await.unwrap();
    let fresh = test_api::context(
        policy.for_current_account(),
        SharedCredentialsProvider::new(imds_credentials()),
        "us-east-1",
        "bedrock",
    );
    let denied = fresh.sign(request.clone()).await;
    assert!(
        matches!(
            &denied,
            Err(AwsAuthError::Policy(
                ra_http::NetworkPolicyDenied::Destination
            ))
        ),
        "{denied:?}"
    );
    assert!(
        timeout(Duration::from_millis(100), listener.accept())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn denied_destination_is_not_signed() {
    use ra_http::DestinationPolicy;

    let controller = ra_http::NetworkPolicyController::default();
    let policy = controller.policy();
    assert!(controller.publish(
        policy.revision(),
        DestinationPolicy::Restricted {
            allowed_hosts: ["bedrock-runtime.us-east-1.amazonaws.com".to_string()].into(),
        },
    ));
    let factory = ra_http::HttpClientFactory::new(ra_http::OutboundProxyPolicy::ReqwestDefault)
        .with_network_policy(policy.for_current_account());
    let context = AwsAuthContext::load_with_access_keys(
        AwsAuthConfig {
            profile: Some("unused-profile".into()),
            region: Some("us-east-1".into()),
            service: "bedrock".into(),
        },
        test_api::access_keys("static-key", "static-secret", None),
        factory,
    )
    .await
    .unwrap();
    let signed = context.sign(test_request()).await.unwrap();
    assert!(
        header_value(&signed.headers, "authorization")
            .unwrap()
            .contains("Credential=static-key/")
    );
    assert!(controller.publish(
        controller.policy().revision(),
        DestinationPolicy::Restricted {
            allowed_hosts: Default::default(),
        },
    ));
    assert!(matches!(
        context.sign(test_request()).await,
        Err(AwsAuthError::Policy(
            ra_http::NetworkPolicyDenied::Destination
        ))
    ));
}
