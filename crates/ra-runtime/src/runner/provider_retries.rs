//! Which of an adapter's own retries the runner switches off for one attempt.
//!
//! Ported from the Agents SDK's `_should_disable_provider_managed_retries` and
//! `_should_disable_websocket_pre_event_retry` (`src/agents/run_internal/model_retry.py`) at
//! f53c62451c7f34c15f0c87fa745e05404025420f; see NOTICE for the retained MIT license.
//!
//! The SDK sets two context variables around each attempt. Here the runner writes the same two
//! answers onto the attempt's request, which an adapter reads with
//! [`ModelRequest::should_disable_provider_managed_retries`] and
//! [`ModelRequest::should_disable_websocket_pre_event_retries`].
//!
//! The SDK's `replay_unsafe_request` input, which disables both, has no local counterpart and is
//! not ported. Its stateful branch also covers the `conversation_locked` compatibility retries,
//! which do not exist locally either; the rule for a later stateful attempt applies unchanged to
//! every retry the runner schedules.

use ra_core::model::{ModelRequest, ModelRetrySettings};

/// Marks `request` with the adapter retries the runner disables for this attempt.
///
/// `replay` says whether an earlier attempt of the same call already failed, which is the SDK's
/// `attempt > 1`.
pub(super) fn coordinate(
    request: ModelRequest,
    settings: Option<&ModelRetrySettings>,
    replay: bool,
) -> ModelRequest {
    let stateful = request.continuation().is_server_managed();
    request
        .with_provider_managed_retries_disabled(disable_provider_managed_retries(
            settings, replay, stateful,
        ))
        .with_websocket_pre_event_retries_disabled(disable_websocket_pre_event_retries(settings))
}

/// The SDK's `_should_disable_provider_managed_retries`, without its `replay_unsafe_request`.
fn disable_provider_managed_retries(
    settings: Option<&ModelRetrySettings>,
    replay: bool,
    stateful: bool,
) -> bool {
    let max_retries = settings.and_then(ModelRetrySettings::max_retries);
    let has_policy = settings.is_some_and(|settings| settings.policy().is_some());
    if max_retries == Some(0) {
        // An explicit no-retry budget also disables hidden provider retries, so callers can opt
        // out of retries entirely.
        return true;
    }
    if replay {
        // A stateful replay already passed the runner's replay checks. Without a policy the SDK
        // runner never schedules a stateless retry, so the provider's own remain the only
        // recovery. Locally a host's `ModelRetryRecovery` can schedule a budget-exempt retry
        // without a policy; such a stateless retry keeps the provider's retries too, nested
        // inside it, rather than leaving that recovery without them.
        return stateful || (has_policy && max_retries.unwrap_or(0) > 0);
    }
    // The first attempt keeps provider retries, except on a request continuing provider-held
    // state that the runner could retry: its replay decisions must go through the runner, which a
    // hidden retry would bypass.
    stateful && max_retries.is_some_and(|max| max > 0) && has_policy
}

/// The SDK's `_should_disable_websocket_pre_event_retry`, without its `replay_unsafe_request`.
fn disable_websocket_pre_event_retries(settings: Option<&ModelRetrySettings>) -> bool {
    let Some(settings) = settings else {
        return false;
    };
    match (settings.max_retries(), settings.policy()) {
        (Some(0), _) => true,
        (Some(_), Some(policy)) => policy.retries_safe_transport_errors(),
        _ => false,
    }
}
