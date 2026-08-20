//! Whether a stored reasoning item should be replayed as `reasoning_content`.
//!
//! Chat Completions has no reasoning surface. Some gateways add one anyway, under the name
//! `reasoning_content`, and a model that emits it also expects to receive it back on the assistant
//! messages of later turns. Replaying it to a model that did not produce it is worse than dropping
//! it: the next turn then reasons from another family's private notes.
//!
//! Two things therefore have to agree before anything is replayed. The endpoint has to speak the
//! field at all, which is a provider fact and lives on the provider registration; and the specific
//! reasoning item has to belong to the model about to receive it, which is a per-item question and
//! is what this module answers.

use std::{fmt, sync::Arc};

use serde_json::Value;

/// Everything a replay decision may look at.
///
/// A borrowed view rather than an owned record: the decision is made while the request is being
/// lowered and never outlives it, and copying the provider payload per reasoning item would be
/// paid on every turn of every run.
#[non_exhaustive]
#[derive(Debug, Clone, Copy)]
pub struct ReasoningReplayContext<'a> {
    model: &'a str,
    base_url: &'a str,
    origin_model: Option<&'a str>,
    provider_data: Option<&'a Value>,
}

impl<'a> ReasoningReplayContext<'a> {
    /// Creates a replay context for one stored reasoning item.
    #[must_use]
    pub const fn new(model: &'a str, base_url: &'a str) -> Self {
        Self {
            model,
            base_url,
            origin_model: None,
            provider_data: None,
        }
    }

    /// Records which model produced the reasoning item, when that is known.
    #[must_use]
    pub const fn with_origin_model(mut self, origin_model: Option<&'a str>) -> Self {
        self.origin_model = origin_model;
        self
    }

    /// Attaches the item's provider replay payload.
    #[must_use]
    pub const fn with_provider_data(mut self, provider_data: Option<&'a Value>) -> Self {
        self.provider_data = provider_data;
        self
    }

    /// Model that will receive the next request.
    #[must_use]
    pub const fn model(self) -> &'a str {
        self.model
    }

    /// Endpoint the request is addressed to.
    ///
    /// The same model name reached directly and reached through a gateway are not the same
    /// endpoint, and only one of them may want the field back. A policy that needs to tell them
    /// apart has the base URL here rather than having to infer it from the model name.
    #[must_use]
    pub const fn base_url(self) -> &'a str {
        self.base_url
    }

    /// Model that produced the reasoning item, when the record says.
    #[must_use]
    pub const fn origin_model(self) -> Option<&'a str> {
        self.origin_model
    }

    /// Complete provider replay payload stored on the reasoning item.
    #[must_use]
    pub const fn provider_data(self) -> Option<&'a Value> {
        self.provider_data
    }

    /// Whether the item carries provider metadata other than its thinking blocks.
    ///
    /// Thinking blocks alone do not establish an origin — they are replay material, not a
    /// provenance record — so an item carrying nothing else is treated as origin-unknown.
    #[must_use]
    pub fn has_foreign_provider_data(self) -> bool {
        self.provider_data
            .and_then(Value::as_object)
            .is_some_and(|data| data.keys().any(|key| key != "thinking_blocks"))
    }
}

/// A caller-supplied replay policy.
type ReplayFn = dyn Fn(&ReasoningReplayContext<'_>) -> bool + Send + Sync;

/// Decides, per reasoning item, whether to replay it as `reasoning_content`.
///
/// Wrapped rather than exposed as a bare `Arc<dyn Fn>` so the type can carry a `Debug`
/// implementation: a model holding one has to stay printable, and a closure is not.
#[derive(Clone)]
pub struct ReasoningReplayPolicy(Arc<ReplayFn>);

impl ReasoningReplayPolicy {
    /// Wraps a caller-supplied decision function.
    #[must_use]
    pub fn new(
        policy: impl Fn(&ReasoningReplayContext<'_>) -> bool + Send + Sync + 'static,
    ) -> Self {
        Self(Arc::new(policy))
    }

    /// Never replays reasoning, whatever the endpoint declares.
    #[must_use]
    pub fn never() -> Self {
        Self::new(|_| false)
    }

    /// Applies the policy to one candidate item.
    #[must_use]
    pub fn allows(&self, context: &ReasoningReplayContext<'_>) -> bool {
        (self.0)(context)
    }
}

impl Default for ReasoningReplayPolicy {
    fn default() -> Self {
        Self::new(default_should_replay_reasoning)
    }
}

impl fmt::Debug for ReasoningReplayPolicy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ReasoningReplayPolicy")
    }
}

/// Replays a reasoning item only when it belongs to the model about to receive it.
///
/// An item is eligible when its recorded origin is this same model, or when it records no origin
/// at all — histories written before provenance was tracked, and items whose only provider payload
/// is replay material. Anything that names a different producer is refused.
///
/// # Deviation from the reference implementation
///
/// The reference gates this on the model name containing `deepseek`, and replays for any item
/// whose origin also matches that substring. This implementation has no vendor substring in it,
/// for the same reason the provider registry has no vendor branches: a name is a routing label
/// chosen by whoever wrote the configuration, and it stops being a reliable signal the moment a
/// gateway renames a model or a fine-tune keeps the family name. The endpoint capability is
/// declared once on the provider registration, which is where that fact is actually known, and
/// this function is only reached after that declaration says the field exists.
#[must_use]
pub fn default_should_replay_reasoning(context: &ReasoningReplayContext<'_>) -> bool {
    match context.origin_model() {
        Some(origin) => origin == context.model(),
        None => !context.has_foreign_provider_data(),
    }
}
