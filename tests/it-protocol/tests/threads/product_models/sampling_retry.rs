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
    Case {
        status: 403,
        code: None,
        error_type: None,
        retry_after: None,
        retried: true,
    },
    Case {
        status: 404,
        code: None,
        error_type: None,
        retry_after: None,
        retried: true,
    },
    Case {
        status: 422,
        code: None,
        error_type: None,
        retry_after: None,
        retried: true,
    },
    Case {
        status: 502,
        code: None,
        error_type: None,
        retry_after: None,
        retried: true,
    },
    Case {
        status: 400,
        code: None,
        error_type: None,
        retry_after: None,
        retried: false,
    },
    Case {
        status: 400,
        code: Some("context_length_exceeded"),
        error_type: None,
        retry_after: None,
        retried: false,
    },
    Case {
        status: 403,
        code: Some("misalignment_policy_violation"),
        error_type: None,
        retry_after: None,
        retried: false,
    },
    Case {
        status: 503,
        code: Some("flex_unavailable"),
        error_type: None,
        retry_after: None,
        retried: true,
    },
    Case {
        status: 500,
        code: Some("server_is_overloaded"),
        error_type: None,
        retry_after: None,
        retried: true,
    },
    Case {
        status: 500,
        code: Some("insufficient_quota"),
        error_type: Some("usage_limit_reached"),
        retry_after: None,
        retried: true,
    },
    Case {
        status: 503,
        code: Some("rate_limit_exceeded"),
        error_type: Some("usage_not_included"),
        retry_after: None,
        retried: true,
    },
    Case {
        status: 429,
        code: Some("flex_unavailable"),
        error_type: None,
        retry_after: Some("0"),
        retried: false,
    },
    // A type-only value is not the explicit code read by Codex's status-specific branches.
    Case {
        status: 429,
        code: None,
        error_type: Some("flex_unavailable"),
        retry_after: Some("0"),
        retried: true,
    },
    Case {
        status: 429,
        code: None,
        error_type: Some("credit_balance_exhausted"),
        retry_after: Some("0"),
        retried: true,
    },
    Case {
        status: 503,
        code: None,
        error_type: Some("server_is_overloaded"),
        retry_after: None,
        retried: true,
    },
    Case {
        status: 401,
        code: None,
        error_type: None,
        retry_after: None,
        retried: true,
    },
];

#[tokio::test]
async fn sampling_failures_follow_codex_turn_retry_rules() {
    for case in CASES {
        let body =
            json!({"error": {"message": "failed", "code": case.code, "type": case.error_type}});
        let mut failure = ResponseTemplate::new(case.status).set_body_json(body);
        if let Some(retry_after) = case.retry_after {
            failure = failure.insert_header("retry-after", retry_after);
        }
        run_failure(failure, case.retried, false).await;
    }
}

async fn run_failure(failure: ResponseTemplate, retried: bool, content_filter: bool) {
    run_failure_with_config(failure, retried, content_filter, 1, None).await;
}

async fn run_failure_with_config(
    failure: ResponseTemplate,
    retried: bool,
    content_filter: bool,
    budget: u32,
    guidance: Option<&str>,
) {
    let endpoint = MockServer::start().await;
    let product = ProductModels::new();
    let mut catalog_config = String::new();
    if let Some(guidance) = guidance {
        let catalog = json!({"models": [{
            "slug": "local-model", "display_name": "Local model", "supported_reasoning_levels": [],
            "shell_type": "unified_exec", "visibility": "list", "supported_in_api": true,
            "priority": 1, "support_verbosity": true, "experimental_supported_tools": [],
            "truncation_policy": {"mode": "tokens", "limit": 1000},
            "model_messages": {"content_filter_guidance": guidance, "instructions_template": "Test product instructions"},
        }]});
        product.write("catalog.json", &catalog.to_string());
        catalog_config = format!(
            "model_catalog_json = {:?}",
            product.home.path().join("catalog.json")
        );
    }
    product.write(
        "config.toml",
        &format!(
            r#"
{catalog_config}
approval_policy = "never"
sandbox_mode = "read-only"
model = "local-model"
model_provider = "gateway"
[model_providers.gateway]
name = "Gateway"
base_url = "{}/product/v1"
experimental_bearer_token = "test-key"
request_max_retries = 0
stream_max_retries = {budget}
"#,
            endpoint.uri()
        ),
    );
    Mock::given(method("POST"))
        .and(path("/product/v1/responses"))
        .respond_with(failure)
        .up_to_n_times(1)
        .expect(1)
        .mount(&endpoint)
        .await;
    let mut success = Mock::given(method("POST")).and(path("/product/v1/responses"));
    if content_filter {
        success = success.and(wiremock::matchers::body_string_contains(
            "<content_filter_guidance>",
        ));
        let expected_guidance = guidance
            .filter(|text| !text.trim().is_empty() && text.len() <= 512)
            .unwrap_or("Your previous response was blocked by a content filter.");
        success = success.and(wiremock::matchers::body_string_contains(expected_guidance));
    }
    success
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            format!("data: {}\n\n", websocket_completion(1)),
            "text/event-stream",
        ))
        .expect(u64::from(retried) + u64::from(content_filter))
        .mount(&endpoint)
        .await;

    let server = product.server(None);
    let mut connection = server.connect(1).await;
    let (thread, _) = start(&server, &mut connection, json!({})).await;
    server.turn_start(&mut connection, &thread, "hello").await;
    if retried {
        let notice = connection.notification("error").await;
        assert_eq!(notice["willRetry"], true, "{notice}");
        assert_eq!(
            notice["error"]["message"], "Reconnecting... 1/1",
            "{notice}"
        );
    }
    let completed = connection.notification("turn/completed").await;
    let expected = if retried { "completed" } else { "failed" };
    assert_eq!(completed["turn"]["status"], expected, "{completed}");
    if content_filter {
        let next = completed_turn(&server, &mut connection, &thread, "continue").await;
        assert_eq!(next["turn"]["status"], "completed", "{next}");
    }
    endpoint.verify().await;
}

#[tokio::test]
async fn stream_failures_reach_the_product_retry_policy_and_filter_guidance_is_persisted() {
    let mut cases = vec![
        ("data: not-json\n\n".to_owned(), true, false),
        (
            format!(
                "data: {}\n\n",
                json!({"type": "response.created", "response": {"id": "lost"}})
            ),
            true,
            false,
        ),
        (
            format!(
                "data: {}\n\ndata: {}\n\n",
                json!({"type": "response.created", "response": {"id": "lost"}}),
                json!({"type": "response.output_text.delta", "item_id": "lost-item", "delta": "partial"})
            ),
            true,
            false,
        ),
    ];
    for (code, retried, retry_after) in [
        ("server_error", true, None),
        ("unknown_code", true, None),
        ("rate_limit_exceeded", true, None),
        ("slow_down", true, Some("0")),
        ("server_is_overloaded", false, None),
        ("server_is_overloaded", true, Some("0")),
        ("context_length_exceeded", false, None),
        ("insufficient_quota", false, None),
        ("usage_not_included", false, None),
        ("flex_unavailable", false, None),
        ("cyber_policy", false, None),
        ("bio_policy", false, None),
        ("invalid_prompt", false, None),
        ("misalignment_policy_violation", false, None),
        // This type/code is not classified as a usage limit on response.failed by Codex.
        ("usage_limit_reached", true, None),
    ] {
        let mut error = json!({"code": code, "message": "failed"});
        if let Some(delay) = retry_after {
            error["headers"] = json!({"retry-after": delay});
        }
        cases.push((
            format!(
                "data: {}\n\n",
                json!({"type": "response.failed", "response": {"status": "failed", "error": error}})
            ),
            retried,
            false,
        ));
    }
    for (reason, retried) in [
        ("content_filter", true),
        ("max_output_tokens", false),
        ("unknown_reason", false),
    ] {
        cases.push((format!("data: {}\n\n", json!({"type": "response.incomplete", "response": {"status": "incomplete", "incomplete_details": {"reason": reason}}})), retried, reason == "content_filter"));
    }
    for (body, retried, content_filter) in cases {
        run_failure(
            ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"),
            retried,
            content_filter,
        )
        .await;
    }
}

#[test]
fn product_retry_backoff_matches_codex_and_zero_server_delay_is_not_replaced() {
    use ra_coding::config::responses_retry_test_api::delay;
    use ra_core::{error::ProviderErrorKind, model::NormalizedProviderError};
    for (attempt, low, high) in [(0, 180, 220), (2, 720, 880)] {
        for _ in 0..64 {
            let error =
                NormalizedProviderError::new(ProviderErrorKind::Network, "connection reset")
                    .into_error();
            let duration = delay(&error, attempt).unwrap();
            assert!(
                duration >= std::time::Duration::from_millis(low)
                    && duration <= std::time::Duration::from_millis(high),
                "{duration:?}"
            );
        }
    }
    let error = NormalizedProviderError::new(ProviderErrorKind::RateLimit, "slow down")
        .with_status_code(429)
        .with_retry_after(std::time::Duration::ZERO)
        .into_error();
    assert_eq!(delay(&error, 2), Some(std::time::Duration::ZERO));
}

#[test]
fn first_websocket_retry_notification_is_hidden_only_in_release() {
    use ra_coding::config::responses_retry_test_api::reports_retry;
    for debug in [false, true] {
        for websocket in [false, true] {
            assert_eq!(reports_retry(1, debug, websocket), debug || !websocket);
            assert!(reports_retry(2, debug, websocket));
        }
    }
}

#[tokio::test]
async fn content_filter_guidance_uses_bounded_catalog_text_even_after_retry_exhaustion() {
    let oversized = "x".repeat(513);
    let unicode_oversized = "é".repeat(257);
    for guidance in [
        None,
        Some("Offer a permitted alternative from the catalog."),
        Some(" "),
        Some(oversized.as_str()),
        Some(unicode_oversized.as_str()),
    ] {
        let body = format!(
            "data: {}\n\n",
            json!({"type": "response.incomplete", "response": {"status": "incomplete", "incomplete_details": {"reason": "content_filter"}}})
        );
        run_failure_with_config(
            ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"),
            false,
            true,
            0,
            guidance,
        )
        .await;
    }
}
