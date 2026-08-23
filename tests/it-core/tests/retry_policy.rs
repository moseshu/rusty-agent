//! The executable retry-policy seam stays outside persisted model settings.

use std::{sync::Arc, time::Duration};

use ra_core::{
    error::ProviderErrorKind,
    model::{
        ModelRetryPolicy, ModelRetrySettings, NetworkErrorRetryPolicy, NormalizedProviderError,
        ProviderSuggestedRetryPolicy, ReplaySafety, RetryAdvice, RetryDecision, RetryPolicyContext,
    },
};

#[derive(Debug)]
struct AlwaysRetry;

#[async_trait::async_trait]
impl ModelRetryPolicy for AlwaysRetry {
    async fn evaluate(&self, _context: &RetryPolicyContext<'_>) -> RetryDecision {
        RetryDecision::retry()
    }
}

#[test]
fn a_policy_never_enters_the_serialized_settings_snapshot() {
    let settings = ModelRetrySettings::new()
        .with_max_retries(2)
        .with_policy(Arc::new(AlwaysRetry));

    let encoded = serde_json::to_value(&settings).unwrap();
    assert_eq!(encoded["max_retries"], 2);
    assert!(encoded.get("policy").is_none());

    let restored: ModelRetrySettings = serde_json::from_value(encoded).unwrap();
    assert!(restored.policy().is_none());
}

#[tokio::test]
async fn built_in_policies_read_normalized_facts_and_preserve_provider_replay_approval() {
    let error =
        NormalizedProviderError::new(ProviderErrorKind::Network, "connection reset").into_error();
    let network = NetworkErrorRetryPolicy;
    let decision = network
        .evaluate(&RetryPolicyContext::new(&error, 0, 2, false, None))
        .await;
    assert!(decision.should_retry());

    let advice = RetryAdvice::new()
        .with_suggested(true)
        .with_retry_after(Duration::from_millis(25))
        .with_replay_safety(ReplaySafety::Safe)
        .with_reason("endpoint_should_retry_header");
    let provider = ProviderSuggestedRetryPolicy;
    let decision = provider
        .evaluate(&RetryPolicyContext::new(&error, 0, 2, false, Some(&advice)))
        .await;
    assert!(decision.should_retry());
    assert_eq!(decision.delay(), Some(Duration::from_millis(25)));
    assert!(decision.replay_approved());
    assert_eq!(decision.reason(), Some("endpoint_should_retry_header"));
}
