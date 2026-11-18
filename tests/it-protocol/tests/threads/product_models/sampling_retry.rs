// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Turn-level retry of product sampling requests through a local Responses endpoint.
//!
//! Follows Codex `codex-rs/core/src/responses_retry.rs` and `CodexErr::retry_delay` in
//! `codex-rs/protocol/src/error.rs`: transient failures are retried with backoff, a rate limit or
//! an overloaded server only on the server's advice, and usage or quota limits never.

use super::*;

/// One failing sampling response, then a successful one if the turn retries.
struct Case {
    status: u16,
    code: Option<&'static str>,
    error_type: Option<&'static str>,
    retry_after: Option<&'static str>,
    retried: bool,
}

const CASES: &[Case] = &[
    // Codex's `InternalServerError`, and `RateLimitExceeded` for a 503 `slow_down`.
    Case {
        status: 500,
        code: None,
        error_type: None,
        retry_after: None,
        retried: true,
    },
    Case {
        status: 503,
        code: Some("slow_down"),
        error_type: None,
        retry_after: None,
        retried: true,
    },
    // Codex's `RetryLimit`: a 429 that is not a usage limit waits for the server's advice.
    Case {
        status: 429,
        code: Some("rate_limit_exceeded"),
        error_type: None,
        retry_after: None,
        retried: false,
    },
    Case {
        status: 429,
        code: Some("rate_limit_exceeded"),
        error_type: None,
        retry_after: Some("0"),
        retried: true,
    },
    // Codex's `QuotaExceeded` is final even with advice.
    Case {
        status: 429,
        code: Some("insufficient_quota"),
        error_type: Some("rate_limit_error"),
        retry_after: Some("0"),
        retried: false,
    },
    Case {
        status: 429,
        code: Some("rate_limit_exceeded"),
        error_type: Some("rate_limit_error"),
        retry_after: Some("0"),
        retried: true,
    },
    // Terminal usage types take precedence over a generic rate-limit code.
    Case {
        status: 429,
        code: Some("rate_limit_exceeded"),
        error_type: Some("insufficient_quota"),
        retry_after: Some("0"),
        retried: false,
    },
    Case {
        status: 429,
        code: Some("rate_limit_exceeded"),
        error_type: Some("usage_limit_reached"),
        retry_after: Some("0"),
        retried: false,
    },
    Case {
        status: 429,
        code: Some("rate_limit_exceeded"),
        error_type: Some("usage_not_included"),
        retry_after: Some("0"),
        retried: false,
    },
    // Codex's `ServerOverloaded` waits for the server's advice too.
    Case {
        status: 503,
        code: Some("server_is_overloaded"),
        error_type: None,
        retry_after: None,
        retried: false,
    },
    Case {
        status: 503,
        code: Some("server_is_overloaded"),
        error_type: None,
        retry_after: Some("0"),
        retried: true,
    },
];

#[tokio::test]
async fn sampling_failures_follow_codex_turn_retry_rules() {
    for case in CASES {
        let endpoint = MockServer::start().await;
        let product = ProductModels::new();
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
base_url = "{}/product/v1"
experimental_bearer_token = "test-key"
request_max_retries = 0
stream_max_retries = 1
"#,
                endpoint.uri()
            ),
        );
        let body = json!({"error": {
            "message": "failed",
            "code": case.code,
            "type": case.error_type,
        }});
        let mut failure = ResponseTemplate::new(case.status).set_body_json(body);
        if let Some(retry_after) = case.retry_after {
            failure = failure.insert_header("retry-after", retry_after);
        }
        Mock::given(method("POST"))
            .and(path("/product/v1/responses"))
            .respond_with(failure)
            .up_to_n_times(1)
            .expect(1)
            .mount(&endpoint)
            .await;
        Mock::given(method("POST"))
            .and(path("/product/v1/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                format!("data: {}\n\n", websocket_completion(1)),
                "text/event-stream",
            ))
            .expect(u64::from(case.retried))
            .mount(&endpoint)
            .await;

        let server = product.server(None);
        let mut connection = server.connect(1).await;
        let (thread, _) = start(&server, &mut connection, json!({})).await;
        server.turn_start(&mut connection, &thread, "hello").await;
        let label = format!(
            "{} {:?} {:?} {:?}",
            case.status, case.code, case.error_type, case.retry_after
        );
        if case.retried {
            let notice = connection.notification("error").await;
            assert_eq!(notice["willRetry"], true, "{label}: {notice}");
            assert_eq!(
                notice["error"]["message"], "Reconnecting... 1/1",
                "{label}: {notice}"
            );
        }
        let completed = connection.notification("turn/completed").await;
        let expected = if case.retried { "completed" } else { "failed" };
        assert_eq!(
            completed["turn"]["status"], expected,
            "{label}: {completed}"
        );
        endpoint.verify().await;
    }
}
