// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
use std::sync::Arc;

use ra_executor::test_api::client::*;
fn registry_error(status: http::StatusCode, code: Option<&str>) -> ExecServerError {
    ExecServerError::EnvironmentRegistryHttp {
        status,
        code: code.map(str::to_string),
        message: "registry unavailable".to_string(),
    }
}
#[test]
fn recovery_retries_transient_registry_errors() {
    for status in [
        http::StatusCode::REQUEST_TIMEOUT,
        http::StatusCode::TOO_MANY_REQUESTS,
        http::StatusCode::INTERNAL_SERVER_ERROR,
        http::StatusCode::BAD_GATEWAY,
        http::StatusCode::SERVICE_UNAVAILABLE,
    ] {
        let error = registry_error(status, None);
        assert!(is_retryable_registry_error(&error));
        assert!(is_retryable_recovery_error(&error));
        assert!(is_retryable_recovery_error(
            &ExecServerError::ConnectionAttempt(Arc::new(error))
        ));
    }
}
#[test]
fn recovery_retries_registry_request_timeouts() {
    let error =
        ExecServerError::EnvironmentRegistryRequest(ra_http::RouteAwareRequestError::Timeout);
    assert!(is_retryable_registry_error(&error));
    assert!(is_retryable_recovery_error(&error));
}
#[test]
fn recovery_retries_environment_offline_conflicts() {
    let error = registry_error(http::StatusCode::CONFLICT, Some("environment_offline"));
    assert!(is_retryable_registry_error(&error));
    assert!(is_retryable_recovery_error(&error));
}
#[test]
fn recovery_does_not_retry_other_registry_conflicts() {
    let error = registry_error(http::StatusCode::CONFLICT, Some("registration_conflict"));
    assert!(!is_retryable_registry_error(&error));
    assert!(!is_retryable_recovery_error(&error));
    assert!(!is_retryable_recovery_error(
        &ExecServerError::ConnectionAttempt(Arc::new(error))
    ));
}
