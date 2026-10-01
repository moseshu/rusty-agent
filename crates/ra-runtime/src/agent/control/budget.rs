//! The agent tree's shared token budget, ported from Codex's rollout budget
//! (`core/src/rollout_budget.rs`, `core/src/agent/control/budget.rs`,
//! `core/src/session/rollout_budget.rs` and `core/src/context/rollout_budget.rs`).
//!
//! One budget covers every run in the tree: the host's root runs, every spawned agent's runs, and
//! the agent-tool runs any of them start. Each model call is charged in weighted tokens — output
//! tokens at the sampling weight plus input tokens not served from cache at the prefill weight —
//! and the tree is out of budget once the charges reach the limit.
//!
//! A turn whose response leaves the budget spent ends its run after it settles, whatever it
//! decided, and a turn that stopped for approval ends it once the answers are settled.
//!
//! An agent is told what is left when one of its runs starts: first the whole budget, and from
//! then on whenever the remainder has fallen past another of the configured thresholds since the
//! agent was last told. The reminder is written into the agent's history, so it is said once and
//! stays, as Codex records it into the conversation.
//!
//! # Deviations from Codex
//!
//! - **How a run on a spent budget ends.** As in Codex, the response whose usage spends the budget —
//!   and every response after that — ends its run once it is recorded and the tool calls it issued
//!   have run: an answer stays in history but is not delivered, no further model call is made, and
//!   a run that starts on a spent budget still makes its one request. Codex reports the turn as
//!   failed with `SessionBudgetExceeded`; here the run ends with
//!   [`FinishReason::BudgetExhausted`](ra_core::finish::FinishReason), as every budget stop does,
//!   which a host continues from with a larger allowance rather than treating as an error.
//! - **Provider-reported budget units are not read.** Codex prefers a `codex_rollout_budget_units`
//!   field of the `OpenAI` backend's usage when one is present; that is a provider wire field, and
//!   every call is charged by the weights instead.
//! - **The reminder is a user message,** where Codex writes a developer message: history here has
//!   no developer role, and a system message inside input history is not portable (see
//!   `crate::budget::budget_reminder`).
//! - **A compaction does not restate the remainder.** Codex keys delivered reminders by context
//!   window and repeats the current one after compacting; the loop here has no window identity,
//!   so an agent is reminded again only when a new threshold has been crossed.

use std::sync::{Mutex, PoisonError};

use ra_core::{
    error::{Error, Result},
    usage::Usage,
};

/// Configuration of an agent tree's shared token budget, Codex's `RolloutBudgetConfig`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub struct RolloutBudgetConfig {
    limit_tokens: u64,
    reminder_at_remaining_tokens: Vec<u64>,
    sampling_token_weight: f64,
    prefill_token_weight: f64,
}

impl RolloutBudgetConfig {
    /// A budget of `limit_tokens` weighted tokens, with a reminder whenever the remainder falls to
    /// or below one of `reminder_at_remaining_tokens`. Both weights default to 1, as in Codex.
    #[must_use]
    pub const fn new(limit_tokens: u64, reminder_at_remaining_tokens: Vec<u64>) -> Self {
        Self {
            limit_tokens,
            reminder_at_remaining_tokens,
            sampling_token_weight: 1.0,
            prefill_token_weight: 1.0,
        }
    }

    /// Sets the weight of an output token.
    #[must_use]
    pub const fn with_sampling_token_weight(mut self, weight: f64) -> Self {
        self.sampling_token_weight = weight;
        self
    }

    /// Sets the weight of an input token not served from the provider's cache.
    #[must_use]
    pub const fn with_prefill_token_weight(mut self, weight: f64) -> Self {
        self.prefill_token_weight = weight;
        self
    }

    /// The budget, in weighted tokens.
    #[must_use]
    pub const fn limit_tokens(&self) -> u64 {
        self.limit_tokens
    }

    /// The remainders at which an agent is reminded of what is left.
    #[must_use]
    pub fn reminder_at_remaining_tokens(&self) -> &[u64] {
        &self.reminder_at_remaining_tokens
    }

    /// The weight of an output token.
    #[must_use]
    pub const fn sampling_token_weight(&self) -> f64 {
        self.sampling_token_weight
    }

    /// The weight of an input token not served from the provider's cache.
    #[must_use]
    pub const fn prefill_token_weight(&self) -> f64 {
        self.prefill_token_weight
    }

    /// Codex's configuration checks: a positive limit, reminders strictly between zero and the
    /// limit, and finite non-negative weights.
    pub(crate) fn validate(&self) -> Result<()> {
        if self.limit_tokens == 0 {
            return Err(Error::config(
                "rollout budget `limit_tokens` must be positive",
            ));
        }
        if self
            .reminder_at_remaining_tokens
            .iter()
            .any(|&tokens| tokens == 0 || tokens >= self.limit_tokens)
        {
            return Err(Error::config(
                "rollout budget `reminder_at_remaining_tokens` must contain only positive values \
                 below `limit_tokens`",
            ));
        }
        for (field, weight) in [
            ("sampling_token_weight", self.sampling_token_weight),
            ("prefill_token_weight", self.prefill_token_weight),
        ] {
            if !weight.is_finite() || weight < 0.0 {
                return Err(Error::config(format!(
                    "rollout budget `{field}` must be finite and non-negative"
                )));
            }
        }
        Ok(())
    }
}

/// A reminder of the remaining budget, acknowledged once it is in the agent's history.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RolloutBudgetReminder {
    pub(crate) remaining_tokens: u64,
    /// How many thresholds the remainder has fallen past.
    pub(crate) reminder_index: usize,
}

impl RolloutBudgetReminder {
    /// Codex's reminder text.
    pub(crate) fn text(self) -> String {
        format!(
            "<rollout_budget>\nYou have {} weighted tokens left in the shared session token \
             budget.\n</rollout_budget>",
            self.remaining_tokens
        )
    }
}

/// Shared accounting for one agent tree.
pub(crate) struct RolloutBudget {
    config: RolloutBudgetConfig,
    weighted_tokens_used: Mutex<f64>,
}

impl RolloutBudget {
    pub(crate) const fn new(config: RolloutBudgetConfig) -> Self {
        Self {
            config,
            weighted_tokens_used: Mutex::new(0.0),
        }
    }

    /// Charges one model call.
    #[allow(
        clippy::cast_precision_loss,
        reason = "token counts are charged as weighted floating-point units, as in Codex"
    )]
    pub(crate) fn record_usage(&self, usage: &Usage) {
        let non_cached_input = usage
            .input_tokens()
            .saturating_sub(usage.cached_input_tokens());
        let units = usage.output_tokens() as f64 * self.config.sampling_token_weight
            + non_cached_input as f64 * self.config.prefill_token_weight;
        *self.used() += units;
    }

    /// Whether the charges have reached the limit.
    #[allow(
        clippy::cast_precision_loss,
        reason = "the limit is compared in weighted floating-point units, as in Codex"
    )]
    pub(crate) fn is_exhausted(&self) -> bool {
        *self.used() >= self.config.limit_tokens as f64
    }

    /// The reminder an agent last told `delivered` is owed now, if any.
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "the remainder is floored to whole weighted tokens, as in Codex"
    )]
    pub(crate) fn pending_reminder(
        &self,
        delivered: Option<usize>,
    ) -> Option<RolloutBudgetReminder> {
        let remaining = (self.config.limit_tokens as f64 - *self.used()).max(0.0);
        let remaining_tokens = remaining.floor() as u64;
        let reminder_index = self
            .config
            .reminder_at_remaining_tokens
            .iter()
            .filter(|&&threshold| remaining_tokens <= threshold)
            .count();
        if delivered.is_some_and(|delivered| delivered >= reminder_index) {
            return None;
        }
        Some(RolloutBudgetReminder {
            remaining_tokens,
            reminder_index,
        })
    }

    fn used(&self) -> std::sync::MutexGuard<'_, f64> {
        self.weighted_tokens_used
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}
