//! The compaction capability: asking the provider to compact the conversation server side.
//!
//! A port of the reference's `sandbox/capabilities/compaction.py`. It contributes no tools and no
//! prompt text. It does two things:
//!
//! - **Sampling settings.** Every request carries `context_management: [{"type": "compaction",
//!   "compact_threshold": N}]`, which tells a Responses endpoint to compact once the conversation
//!   passes `N` tokens. `N` comes from the configured policy; with none configured, it is 90% of the
//!   model's context window when the model is one the reference knows, and 240,000 otherwise.
//! - **Context processing.** When the conversation holds a compaction the provider returned, every
//!   input item before the last one is dropped: the compaction stands for them.
//!
//! # Where the request field goes
//!
//! The reference returns the field as an extra keyword argument for the model call. Here a field
//! only one provider understands goes in that provider's own `extra_body` bucket, so the field is
//! written under the provider the request is resolved to — which is why this capability answers
//! [`Capability::sampling_params_for`] and leaves the settings alone when it is not told the
//! provider. The field is written whichever protocol that provider speaks, as the reference writes
//! it for every model: an endpoint that does not know it rejects the request, and one that never
//! compacts never returns an item to replay.
//!
//! # Not the context chapter's compaction
//!
//! `ra-context` compacts locally: it summarizes history with a model call of its own and keeps the
//! summary as a portable [`ra_core::item::Compaction`]. This capability runs no model call and
//! writes no summary — the provider does the work and hands back an opaque
//! [`ProviderCompaction`](ra_core::item::ProviderCompaction). The two can be installed together; the
//! local one then treats the provider's item as one more record of history.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use async_trait::async_trait;
use ra_core::{
    capability::{
        Capability, CapabilityFamily, ContextProcessor, ContextProcessorRequest,
        ContextProcessorResult, ContextSummarizer, SamplingContext,
    },
    error::{Error, Result},
    item::ModelInputItem,
    model::ModelSettings,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// The threshold the static policy uses unless told otherwise, in tokens.
pub const DEFAULT_COMPACT_THRESHOLD: u64 = 240_000;

/// The share of the context window the dynamic policy compacts at unless told otherwise.
pub const DEFAULT_DYNAMIC_THRESHOLD: f64 = 0.9;

/// The name the reference's model-to-window table is looked up by: trimmed, lower-cased, without
/// an `openai/` prefix, and without dots or hyphens, so `gpt-5.4`, `gpt-5-4` and `openai/GPT-5.4`
/// all find the same row.
fn model_lookup_key(model: &str) -> String {
    let lowered = model.trim().to_lowercase();
    let unprefixed = lowered.strip_prefix("openai/").unwrap_or(&lowered);
    unprefixed
        .chars()
        .filter(|c| !matches!(c, '.' | '-'))
        .collect()
}

/// The reference's table of context windows, in its order; a later group wins a shared key.
static MODEL_CONTEXT_WINDOWS: LazyLock<BTreeMap<String, u64>> = LazyLock::new(|| {
    let groups: [(&[&str], u64); 4] = [
        (
            &[
                "gpt-5.4",
                "gpt-5.4-2026-03-05",
                "gpt-5.4-pro",
                "gpt-5.4-pro-2026-03-05",
                "gpt-5.5",
                "gpt-5.5-2026-04-23",
                "gpt-5.5-pro",
                "gpt-5.5-pro-2026-04-23",
                "gpt-5.6",
                "gpt-5.6-sol",
                "gpt-5.6-terra",
                "gpt-5.6-luna",
                "gpt-4.1",
                "gpt-4.1-2025-04-14",
                "gpt-4.1-mini",
                "gpt-4.1-mini-2025-04-14",
                "gpt-4.1-nano",
                "gpt-4.1-nano-2025-04-14",
            ],
            1_047_576,
        ),
        (
            &[
                "gpt-5",
                "gpt-5-2025-08-07",
                "gpt-5-codex",
                "gpt-5-mini",
                "gpt-5-mini-2025-08-07",
                "gpt-5-nano",
                "gpt-5-nano-2025-08-07",
                "gpt-5-pro",
                "gpt-5-pro-2025-10-06",
                "gpt-5.1",
                "gpt-5.1-2025-11-13",
                "gpt-5.1-codex",
                "gpt-5.1-codex-max",
                "gpt-5.1-codex-mini",
                "gpt-5.2",
                "gpt-5.2-2025-12-11",
                "gpt-5.2-codex",
                "gpt-5.2-pro",
                "gpt-5.2-pro-2025-12-11",
                "gpt-5.3-codex",
                "gpt-5.4-mini",
                "gpt-5.4-mini-2026-03-17",
                "gpt-5.4-nano",
                "gpt-5.4-nano-2026-03-17",
            ],
            400_000,
        ),
        (
            &[
                "codex-mini-latest",
                "o1",
                "o1-2024-12-17",
                "o1-pro",
                "o1-pro-2025-03-19",
                "o3",
                "o3-2025-04-16",
                "o3-deep-research",
                "o3-deep-research-2025-06-26",
                "o3-mini",
                "o3-mini-2025-01-31",
                "o3-pro",
                "o3-pro-2025-06-10",
                "o4-mini",
                "o4-mini-2025-04-16",
                "o4-mini-deep-research",
                "o4-mini-deep-research-2025-06-26",
            ],
            200_000,
        ),
        (
            &[
                "gpt-4o",
                "gpt-4o-2024-05-13",
                "gpt-4o-2024-08-06",
                "gpt-4o-2024-11-20",
                "gpt-4o-mini",
                "gpt-4o-mini-2024-07-18",
                "gpt-5-chat-latest",
                "gpt-5.1-chat-latest",
                "gpt-5.2-chat-latest",
                "gpt-5.3-chat-latest",
            ],
            128_000,
        ),
    ];
    let mut table = BTreeMap::new();
    for (models, window) in groups {
        for model in models {
            table.insert(model_lookup_key(model), window);
        }
    }
    table
});

/// Quotes a string as Python's `repr` does, for messages the reference words that way.
fn python_repr(text: &str) -> String {
    let quote = if text.contains('\'') && !text.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(text.len() + 2);
    out.push(quote);
    for character in text.chars() {
        match character {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// What the dynamic policy knows about a model: its context window, in tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactionModelInfo {
    context_window: u64,
}

impl CompactionModelInfo {
    /// A model whose context window is `context_window` tokens.
    #[must_use]
    pub const fn new(context_window: u64) -> Self {
        Self { context_window }
    }

    /// The context window, in tokens.
    #[must_use]
    pub const fn context_window(&self) -> u64 {
        self.context_window
    }

    /// The reference's entry for `model`, or `None` for a model it does not list.
    #[must_use]
    pub fn maybe_for_model(model: &str) -> Option<Self> {
        MODEL_CONTEXT_WINDOWS
            .get(&model_lookup_key(model))
            .copied()
            .map(Self::new)
    }

    /// The reference's entry for `model`.
    ///
    /// # Errors
    ///
    /// Returns a configuration error, worded as the reference's `ValueError`, for a model it does
    /// not list.
    pub fn for_model(model: &str) -> Result<Self> {
        Self::maybe_for_model(model).ok_or_else(|| {
            Error::config(format!(
                "Unknown context window for model: {}",
                python_repr(model)
            ))
        })
    }
}

/// When the provider is asked to compact.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CompactionPolicy {
    /// At a fixed number of tokens.
    Static {
        /// The threshold, in tokens.
        threshold: u64,
    },
    /// At a share of the model's context window.
    Dynamic {
        /// The model the share is taken of.
        model_info: CompactionModelInfo,
        /// The share, between 0 and 1.
        threshold: f64,
    },
}

impl<'de> Deserialize<'de> for CompactionPolicy {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        Self::from_value(&value).map_err(serde::de::Error::custom)
    }
}

impl Default for CompactionPolicy {
    fn default() -> Self {
        Self::static_threshold(DEFAULT_COMPACT_THRESHOLD)
    }
}

impl CompactionPolicy {
    /// Compacts at `threshold` tokens.
    #[must_use]
    pub const fn static_threshold(threshold: u64) -> Self {
        Self::Static { threshold }
    }

    /// Compacts at `threshold` of `model_info`'s context window.
    ///
    /// # Errors
    ///
    /// Returns a configuration error when `threshold` is not between 0 and 1, the bounds the
    /// reference validates.
    pub fn dynamic(model_info: CompactionModelInfo, threshold: f64) -> Result<Self> {
        if !(0.0..=1.0).contains(&threshold) {
            return Err(Error::config(format!(
                "a dynamic compaction threshold must be between 0 and 1, not {threshold}"
            )));
        }
        Ok(Self::Dynamic {
            model_info,
            threshold,
        })
    }

    /// Compacts at the reference's default share, 90%, of `model_info`'s context window.
    #[must_use]
    pub const fn dynamic_default(model_info: CompactionModelInfo) -> Self {
        Self::Dynamic {
            model_info,
            threshold: DEFAULT_DYNAMIC_THRESHOLD,
        }
    }

    /// Reads a policy from its serialized form, defaulting each field as the reference does.
    ///
    /// # Errors
    ///
    /// Returns a configuration error for a type other than `static` or `dynamic`, worded as the
    /// reference's, and for a field of the wrong shape.
    pub fn from_value(value: &Value) -> Result<Self> {
        let policy_type = value.get("type");
        match policy_type.and_then(Value::as_str) {
            Some("static") => {
                let threshold = match value.get("threshold") {
                    None => DEFAULT_COMPACT_THRESHOLD,
                    Some(threshold) => threshold.as_u64().ok_or_else(|| {
                        Error::config("a static compaction threshold must be a whole number")
                    })?,
                };
                Ok(Self::static_threshold(threshold))
            }
            Some("dynamic") => {
                let model_info: CompactionModelInfo =
                    serde_json::from_value(value.get("model_info").cloned().unwrap_or(Value::Null))
                        .map_err(|error| {
                            Error::config(format!("invalid dynamic compaction model_info: {error}"))
                        })?;
                let threshold = match value.get("threshold") {
                    None => DEFAULT_DYNAMIC_THRESHOLD,
                    Some(threshold) => threshold.as_f64().ok_or_else(|| {
                        Error::config("a dynamic compaction threshold must be a number")
                    })?,
                };
                Self::dynamic(model_info, threshold)
            }
            other => {
                let shown = match (other, policy_type) {
                    (Some(text), _) => python_repr(text),
                    (None, None | Some(Value::Null)) => "None".to_owned(),
                    (None, Some(value)) => value.to_string(),
                };
                Err(Error::config(format!(
                    "Unsupported compaction policy type: {shown}"
                )))
            }
        }
    }

    /// The threshold, in tokens, this policy compacts at.
    #[must_use]
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    pub fn compaction_threshold(&self) -> u64 {
        match self {
            Self::Static { threshold } => *threshold,
            // Truncated as the reference's `int()` truncates; the share is never negative.
            Self::Dynamic {
                model_info,
                threshold,
            } => (model_info.context_window() as f64 * threshold) as u64,
        }
    }
}

/// Asks the provider to compact the conversation, and keeps only what the last compaction left.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Compaction {
    policy: Option<CompactionPolicy>,
}

impl Compaction {
    /// Compaction with no policy: the threshold follows the model, as described in the module
    /// documentation.
    #[must_use]
    pub const fn new() -> Self {
        Self { policy: None }
    }

    /// Compaction at `policy`.
    #[must_use]
    pub const fn with_policy(policy: CompactionPolicy) -> Self {
        Self {
            policy: Some(policy),
        }
    }

    /// The configured policy, if any.
    #[must_use]
    pub const fn policy(&self) -> Option<&CompactionPolicy> {
        self.policy.as_ref()
    }

    /// The policy a request for `model` is compacted by: the configured one, or the one the model
    /// implies.
    #[must_use]
    pub fn effective_policy(&self, model: Option<&str>) -> CompactionPolicy {
        if let Some(policy) = &self.policy {
            return policy.clone();
        }
        match model.filter(|model| !model.is_empty()) {
            Some(model) => CompactionModelInfo::maybe_for_model(model)
                .map_or_else(CompactionPolicy::default, CompactionPolicy::dynamic_default),
            None => CompactionPolicy::default(),
        }
    }

    /// The request field the reference returns from `sampling_params`, for `model`.
    #[must_use]
    pub fn context_management(&self, model: Option<&str>) -> Value {
        json!([{
            "type": "compaction",
            "compact_threshold": self.effective_policy(model).compaction_threshold(),
        }])
    }

    /// The reference's `process_context`: everything from the last provider compaction on, or the
    /// input unchanged when there is none.
    #[must_use]
    pub fn process_input(input: &[ModelInputItem]) -> Vec<ModelInputItem> {
        match input
            .iter()
            .rposition(|item| matches!(item, ModelInputItem::ProviderCompaction(_)))
        {
            Some(last) => input[last..].to_vec(),
            None => input.to_vec(),
        }
    }
}

#[async_trait]
impl Capability for Compaction {
    fn kind(&self) -> CapabilityFamily {
        CapabilityFamily::COMPACTION
    }

    fn sampling_params_for(
        &self,
        settings: ModelSettings,
        context: &SamplingContext,
    ) -> ModelSettings {
        match context.provider() {
            Some(provider) => settings.with_extra_body_value(
                provider.clone(),
                "context_management",
                self.context_management(context.model()),
            ),
            // Nowhere to put a provider-specific field without a provider.
            None => settings,
        }
    }

    fn context_processor(&self) -> Option<&dyn ContextProcessor> {
        Some(self)
    }
}

#[async_trait]
impl ContextProcessor for Compaction {
    async fn process_context(
        &self,
        request: ContextProcessorRequest,
        summarizer: &dyn ContextSummarizer,
    ) -> Result<ContextProcessorResult> {
        let _ = summarizer;
        Ok(ContextProcessorResult::new(Self::process_input(
            request.input(),
        )))
    }
}
