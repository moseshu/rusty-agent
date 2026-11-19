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
async fn built_in_policies_read_normalized_facts_and_provider_advice() {
    let error =
        NormalizedProviderError::new(ProviderErrorKind::Network, "connection reset").into_error();
    let network = NetworkErrorRetryPolicy;
    let decision = network
        .evaluate(&RetryPolicyContext::new(&error, 1, 2, false, None))
        .await;
    assert!(decision.should_retry());

    let advice = RetryAdvice::new()
        .with_suggested(true)
        .with_retry_after(Duration::from_millis(25))
        .with_replay_safety(ReplaySafety::Safe)
        .with_reason("endpoint_should_retry_header");
    let provider = ProviderSuggestedRetryPolicy;
    let decision = provider
        .evaluate(&RetryPolicyContext::new(&error, 1, 2, false, Some(&advice)))
        .await;
    assert!(decision.should_retry());
    assert_eq!(decision.delay(), Some(Duration::from_millis(25)));
    // Following the provider is an ordinary retry, never an approval of unsafe replay.
    assert!(!decision.approves_unsafe_replay());
    assert_eq!(decision.reason(), Some("endpoint_should_retry_header"));
}

/// As the SDK's `provider_suggested`, a provider veto declines and keeps its reason.
#[tokio::test]
async fn the_provider_suggested_policy_keeps_the_reason_for_a_provider_veto() {
    let error =
        NormalizedProviderError::new(ProviderErrorKind::Network, "connection reset").into_error();
    let advice = RetryAdvice::new()
        .with_suggested(false)
        .with_replay_safety(ReplaySafety::Unsafe)
        .with_reason("provider veto");
    let decision = ProviderSuggestedRetryPolicy
        .evaluate(&RetryPolicyContext::new(&error, 1, 1, false, Some(&advice)))
        .await;
    assert!(!decision.should_retry());
    assert_eq!(decision.reason(), Some("provider veto"));

    let silent = RetryAdvice::new().with_reason("no verdict");
    let decision = ProviderSuggestedRetryPolicy
        .evaluate(&RetryPolicyContext::new(&error, 1, 1, false, Some(&silent)))
        .await;
    assert!(!decision.should_retry());
    assert_eq!(decision.reason(), None);
}

/// Unsafe replay approval is explicit and separate from retrying, as the SDK's
/// `approve_unsafe_replay`.
#[test]
fn unsafe_replay_approval_is_separate_from_an_ordinary_retry() {
    assert!(!RetryDecision::retry().approves_unsafe_replay());
    let approved = RetryDecision::retry().with_unsafe_replay_approval();
    assert!(approved.should_retry() && approved.approves_unsafe_replay());
}

/// A policy scopes its decisions with the replay verdict the runtime applies, whether the provider
/// had begun responding, and whether the request continued provider-held state.
#[test]
fn the_policy_context_reports_the_replay_verdict_response_start_and_continuation() {
    use ra_core::model::{ConversationContinuation, stamp_replay_safety, stamp_response_started};

    let stamped = stamp_response_started(stamp_replay_safety(
        NormalizedProviderError::new(ProviderErrorKind::Network, "connection reset")
            .with_status_code(502)
            .into_error(),
        ReplaySafety::Unsafe,
    ));
    let normalized = NormalizedProviderError::from_error(&stamped).unwrap();
    assert!(normalized.response_started());
    assert_eq!(normalized.replay_safety(), ReplaySafety::Unsafe);
    assert_eq!(normalized.status_code(), Some(502), "other facts survive");

    // Without advice the runtime applies the verdict stamped on the error, and nothing says the
    // provider had begun responding.
    let context = RetryPolicyContext::new(&stamped, 1, 1, false, None);
    assert_eq!(context.replay_safety(), ReplaySafety::Unsafe);
    assert!(!context.response_started());
    assert!(!context.is_stateful_request());
    assert_eq!(context.continuation(), &ConversationContinuation::None);

    // Advice outranks the stamp.
    let advice = RetryAdvice::new()
        .with_replay_safety(ReplaySafety::Safe)
        .with_response_started(true);
    let continuation = ConversationContinuation::PreviousResponseId("resp_1".to_owned());
    let context = RetryPolicyContext::new(&stamped, 1, 1, false, Some(&advice))
        .with_continuation(&continuation);
    assert_eq!(context.replay_safety(), ReplaySafety::Safe);
    assert!(context.response_started());
    assert!(context.is_stateful_request());
    assert_eq!(context.continuation(), &continuation);

    let unknown = NormalizedProviderError::new(ProviderErrorKind::Network, "reset").into_error();
    assert_eq!(
        RetryPolicyContext::new(&unknown, 1, 1, false, None).replay_safety(),
        ReplaySafety::Unknown
    );
}
