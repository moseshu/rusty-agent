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

fn network_failure() -> ra_core::error::Error {
    NormalizedProviderError::new(ProviderErrorKind::Network, "connection reset").into_error()
}

/// A policy that retries with an unsafe-replay approval.
#[derive(Debug)]
struct ApproveUnsafeReplay;

#[async_trait::async_trait]
impl ModelRetryPolicy for ApproveUnsafeReplay {
    async fn evaluate(&self, _context: &RetryPolicyContext<'_>) -> RetryDecision {
        RetryDecision::retry().with_unsafe_replay_approval()
    }
}

fn policies(list: Vec<Arc<dyn ModelRetryPolicy>>) -> Vec<Arc<dyn ModelRetryPolicy>> {
    list
}

/// As the SDK's `test_retry_policies_any_merges_later_positive_metadata`: positive decisions
/// merge, and a later one's delay replaces an earlier one's absence of it.
#[tokio::test]
async fn any_merges_later_positive_metadata() {
    use ra_core::model::{AnyRetryPolicy, RetryAfterPolicy};

    let error = NormalizedProviderError::new(ProviderErrorKind::Network, "connection reset")
        .with_retry_after(Duration::from_millis(1_750))
        .into_error();
    let advice = RetryAdvice::new().with_retry_after(Duration::from_millis(1_750));
    let decision = AnyRetryPolicy::new(policies(vec![
        Arc::new(NetworkErrorRetryPolicy),
        Arc::new(RetryAfterPolicy),
    ]))
    .evaluate(&RetryPolicyContext::new(&error, 1, 2, false, Some(&advice)))
    .await;
    assert!(decision.should_retry());
    assert_eq!(decision.delay(), Some(Duration::from_millis(1_750)));
}

/// A provider veto that is not about an unsafe replay is a hard veto: it ends the combined
/// evaluation with its reason, whatever the other policies would say, as in the SDK's
/// `test_get_response_with_retry_honors_provider_hard_veto`.
#[tokio::test]
async fn a_provider_veto_ends_a_combined_evaluation() {
    use ra_core::model::{AllRetryPolicy, AnyRetryPolicy};

    let error = network_failure();
    let veto = RetryAdvice::new()
        .with_suggested(false)
        .with_reason("server veto");
    let context = RetryPolicyContext::new(&error, 1, 1, false, Some(&veto));
    for provider_first in [true, false] {
        let list = |approve: bool| {
            let other: Arc<dyn ModelRetryPolicy> = if approve {
                Arc::new(ApproveUnsafeReplay)
            } else {
                Arc::new(NetworkErrorRetryPolicy)
            };
            let provider: Arc<dyn ModelRetryPolicy> = Arc::new(ProviderSuggestedRetryPolicy);
            if provider_first {
                policies(vec![provider, other])
            } else {
                policies(vec![other, provider])
            }
        };
        for approve in [false, true] {
            for decision in [
                AnyRetryPolicy::new(list(approve)).evaluate(&context).await,
                AllRetryPolicy::new(list(approve)).evaluate(&context).await,
            ] {
                assert!(!decision.should_retry(), "{provider_first} {approve}");
                assert_eq!(decision.reason(), Some("server veto"));
            }
        }
    }
}

/// A provider veto over a replay the adapter marked unsafe is delegable: it stands against an
/// ordinary retry, and another combined policy's unsafe-replay approval resolves it, in either
/// order and under either combinator. The SDK checks the same in
/// `test_unsafe_replay_approval_resolves_provider_veto_in_policy_combinators`.
#[tokio::test]
async fn an_unsafe_replay_veto_is_resolved_only_by_an_approval() {
    use ra_core::model::{AllRetryPolicy, AnyRetryPolicy};

    let error = network_failure();
    let veto = RetryAdvice::new()
        .with_suggested(false)
        .with_replay_safety(ReplaySafety::Unsafe)
        .with_reason("provider veto");
    let context = RetryPolicyContext::new(&error, 1, 1, false, Some(&veto));
    for provider_first in [true, false] {
        for approve in [false, true] {
            let other: Arc<dyn ModelRetryPolicy> = if approve {
                Arc::new(ApproveUnsafeReplay)
            } else {
                Arc::new(NetworkErrorRetryPolicy)
            };
            let provider: Arc<dyn ModelRetryPolicy> = Arc::new(ProviderSuggestedRetryPolicy);
            let list = if provider_first {
                vec![provider, other]
            } else {
                vec![other, provider]
            };
            for decision in [
                AnyRetryPolicy::new(list.clone()).evaluate(&context).await,
                AllRetryPolicy::new(list).evaluate(&context).await,
            ] {
                assert_eq!(
                    decision.should_retry(),
                    approve,
                    "{provider_first} {approve}"
                );
                assert_eq!(decision.approves_unsafe_replay(), approve);
                if !approve {
                    assert_eq!(decision.reason(), Some("provider veto"));
                }
            }
        }
    }
}

/// `all` returns the first decision not to retry and keeps the latest delay of the rest; `any`
/// returns the last decision not to retry when none retries. With no policies, neither retries.
#[tokio::test]
async fn combinators_follow_the_sdk_short_circuits() {
    use ra_core::model::{AllRetryPolicy, AnyRetryPolicy, NeverRetryPolicy, RetryAfterPolicy};

    let error = NormalizedProviderError::new(ProviderErrorKind::Network, "connection reset")
        .with_retry_after(Duration::from_millis(40))
        .into_error();
    let context = RetryPolicyContext::new(&error, 1, 1, false, None);

    let all = AllRetryPolicy::new(policies(vec![
        Arc::new(NetworkErrorRetryPolicy),
        Arc::new(RetryAfterPolicy),
    ]))
    .evaluate(&context)
    .await;
    assert!(all.should_retry());
    assert_eq!(all.delay(), Some(Duration::from_millis(40)));

    let declined = AllRetryPolicy::new(policies(vec![
        Arc::new(NetworkErrorRetryPolicy),
        Arc::new(NeverRetryPolicy),
        Arc::new(RetryAfterPolicy),
    ]))
    .evaluate(&context)
    .await;
    assert!(!declined.should_retry());

    let none = AnyRetryPolicy::new(policies(vec![
        Arc::new(NeverRetryPolicy),
        Arc::new(NeverRetryPolicy),
    ]))
    .evaluate(&context)
    .await;
    assert!(!none.should_retry());

    assert!(
        !AllRetryPolicy::new(Vec::new())
            .evaluate(&context)
            .await
            .should_retry()
    );
    assert!(
        !AnyRetryPolicy::new(Vec::new())
            .evaluate(&context)
            .await
            .should_retry()
    );
}

/// As the SDK's `retry_policies.http_status`, only the listed statuses retry, and a failure with
/// no HTTP status does not.
#[tokio::test]
async fn the_http_status_policy_retries_only_the_listed_statuses() {
    use ra_core::model::HttpStatusRetryPolicy;

    let policy = HttpStatusRetryPolicy::new([429, 503]);
    for (status, retried) in [
        (Some(429), true),
        (Some(503), true),
        (Some(500), false),
        (None, false),
    ] {
        let mut normalized = NormalizedProviderError::new(ProviderErrorKind::ServerError, "failed");
        if let Some(status) = status {
            normalized = normalized.with_status_code(status);
        }
        let error = normalized.into_error();
        let decision = policy
            .evaluate(&RetryPolicyContext::new(&error, 1, 1, false, None))
            .await;
        assert_eq!(decision.should_retry(), retried, "{status:?}");
    }
}

/// The markers combinators read are not part of a decision's public meaning: a provider veto
/// equals an ordinary refusal with the same reason, and `Debug` does not show them, as the SDK's
/// private fields are declared `compare=False` and `repr=False`.
#[tokio::test]
async fn combinator_markers_stay_out_of_equality_and_debug() {
    let error = network_failure();
    for safety in [ReplaySafety::Unknown, ReplaySafety::Unsafe] {
        let veto = RetryAdvice::new()
            .with_suggested(false)
            .with_replay_safety(safety)
            .with_reason("provider veto");
        let decision = ProviderSuggestedRetryPolicy
            .evaluate(&RetryPolicyContext::new(&error, 1, 1, false, Some(&veto)))
            .await;
        let ordinary = RetryDecision::no_retry().with_reason("provider veto");
        assert_eq!(decision, ordinary, "{safety:?}");
        assert_eq!(format!("{decision:?}"), format!("{ordinary:?}"));
    }

    let safe = RetryAdvice::new()
        .with_suggested(true)
        .with_replay_safety(ReplaySafety::Safe);
    let decision = ProviderSuggestedRetryPolicy
        .evaluate(&RetryPolicyContext::new(&error, 1, 1, false, Some(&safe)))
        .await;
    assert_eq!(decision, RetryDecision::retry());
    assert_eq!(
        format!("{decision:?}"),
        "RetryDecision { retry: true, delay: None, reason: None, unsafe_replay_approved: false }"
    );
}

/// A policy that approves unsafe replay with an empty reason.
#[derive(Debug)]
struct ApproveWithEmptyReason;

#[async_trait::async_trait]
impl ModelRetryPolicy for ApproveWithEmptyReason {
    async fn evaluate(&self, _context: &RetryPolicyContext<'_>) -> RetryDecision {
        RetryDecision::retry()
            .with_unsafe_replay_approval()
            .with_reason("")
    }
}

/// Resolving a provider veto keeps the approving reason unless it is empty, as the SDK's
/// `approving.reason or veto.reason` falls back on an empty string.
#[tokio::test]
async fn resolving_a_veto_falls_back_to_its_reason_for_an_empty_approval_reason() {
    use ra_core::model::{AllRetryPolicy, AnyRetryPolicy};

    let error = network_failure();
    let veto = RetryAdvice::new()
        .with_suggested(false)
        .with_replay_safety(ReplaySafety::Unsafe)
        .with_reason("provider veto");
    let context = RetryPolicyContext::new(&error, 1, 1, false, Some(&veto));
    let list = || {
        policies(vec![
            Arc::new(ProviderSuggestedRetryPolicy),
            Arc::new(ApproveWithEmptyReason),
        ])
    };
    for decision in [
        AllRetryPolicy::new(list()).evaluate(&context).await,
        AnyRetryPolicy::new(list()).evaluate(&context).await,
    ] {
        assert!(decision.should_retry() && decision.approves_unsafe_replay());
        assert_eq!(decision.reason(), Some("provider veto"));
    }
}
