//! Runtime helpers for budget enforcement and model-facing budget guidance.

use std::sync::{Arc, Mutex, PoisonError};

use ra_core::{
    budget::BudgetLimit, error::BudgetKind, item::Message, state::RunState, usage::Usage,
};

use crate::agent::control::RolloutBudget;

/// Returns the token-budget reminder appended to one model call's input.
///
/// It rides at the **tail of the input**, never in the system instructions. Instructions are the
/// stable cache prefix, and a number that changes every turn would invalidate that prefix on every
/// single call — the largest cost driver there is. A tail message says the same thing while leaving
/// everything before it byte-identical.
///
/// It carries the **user** role, matching the cross-protocol lowering of every other tail item. A
/// system message inside input history is not portable: Anthropic accepts system text only in its
/// top-level instruction field and rejects it in the message list, so a system-role reminder would
/// fail every single request of a run that configured a token budget.
///
/// Only the task-token budget becomes prompt text. Turn, cost, and deadline ceilings are host
/// control-plane limits; exposing them would invite the model to reason about implementation
/// details instead of pacing the work it was asked to complete.
///
/// The remainder is measured against the same spend the ceiling is: see [`RunSpend`].
pub(crate) fn budget_reminder(spend: &RunSpend, limit: &BudgetLimit) -> Option<Message> {
    let remaining = limit.max_tokens()?.saturating_sub(spend.budget_tokens());
    Some(Message::user(format!(
        "Task token budget: {remaining} tokens remain. Pace the remaining work accordingly."
    )))
}

/// The first exhausted budget dimension, with token spend read the way [`RunSpend`] shares it.
///
/// The run's own state answers the turn cap and the deadline, and its ledger is a lower bound on
/// the shared spend, so its token verdict can only agree. What it cannot see is spend the ceiling
/// is shared with — the parent's, and that of agent-tool runs started beside this one — which is
/// read here. The priority order is the state's.
///
/// The agent tree's rollout budget is deliberately not among these. Codex checks it after a
/// response rather than before a request — a turn that starts on a spent budget still makes its
/// request and fails after it — so the loop asks [`RunSpend::rollout_budget_exhausted`] once a turn
/// has settled instead.
pub(crate) fn exhausted_budget_kind(
    state: &RunState,
    limit: &BudgetLimit,
    spend: &RunSpend,
) -> Option<BudgetKind> {
    let own = state.exhausted_budget_kind(limit);
    if own == Some(BudgetKind::MaxTurns) {
        return own;
    }
    let tokens = limit
        .max_tokens()
        .is_some_and(|maximum| spend.budget_tokens() >= maximum);
    if tokens {
        return Some(BudgetKind::Tokens);
    }
    own
}

/// What a run has spent so far, as the runs it started see it while it is still running.
///
/// # Why this exists beside the ledger
///
/// The reference hands a nested agent-tool run the parent's own `Usage` object, so every model call
/// the nested run makes is added to the parent's total the moment it is paid for — whether the
/// nested run then finishes, fails, is cancelled, or pauses and continues later. Here the parent's
/// ledger belongs to [`RunState`], which the parent's loop owns and a nested run cannot reach. So
/// each run keeps one of these next to its state: a nested run forwards every call it records to
/// its parent's, up the whole chain, and the parent moves what arrived into its own ledger when its
/// loop ends ([`Self::take_nested`]). The ledger stays the only record that is persisted; this is
/// the live view of it plus what has not been moved yet, and it is what the run's budget checks and
/// live context read while the loop runs.
///
/// # Which spend a token ceiling is measured against
///
/// A nested run started under the parent's configuration inherits the parent's token ceiling, and
/// with a shared ledger that ceiling is the parent's: read through the reference's shared `Usage`,
/// the nested run would see the parent's spend and every sibling's along with its own. Such a run
/// therefore measures its ceiling against the run whose configuration it inherited
/// ([`Self::budget_tokens`]). A nested run given its own configuration was given its own ceiling,
/// and measures it against its own spend; its calls still reach the parent's ledger.
///
/// # What the code running in a run reads
///
/// The reference's shared object is also what the nested run's own hooks, guardrails and tools
/// read as the run's usage: the outermost run's total with every nested call in it, and the same
/// object again once a paused nested run resumes. A run's
/// [`RunContext`](ra_core::context::RunContext) therefore reports [`Self::shared_usage`], read from
/// the outermost run of its agent-tool chain whichever configuration each level ran under, while
/// each run's own ledger, result and checkpoint keep only what it spent itself.
///
/// # The agent tree's rollout budget
///
/// A run bound to an agent tree with a rollout budget records every call that reaches its ledger
/// there — its own and its nested runs' — exactly once, since a nested run is never bound itself.
pub(crate) struct RunSpend {
    inner: Mutex<SpendInner>,
    parent: Option<Arc<RunSpend>>,
    /// The run whose token ceiling this run inherited, when it is not its own.
    budget_owner: Option<Arc<RunSpend>>,
    rollout_budget: Option<Arc<RolloutBudget>>,
}

struct SpendInner {
    /// The run's ledger plus nested spend not yet moved into it.
    total: Usage,
    /// Spend nested runs recorded here that the run's ledger does not hold yet.
    nested: Usage,
}

/// How a nested run's spend is tied to the run whose tool call started it.
#[derive(Clone)]
pub(crate) struct NestedSpend {
    parent: Arc<RunSpend>,
    shares_budget: bool,
}

impl NestedSpend {
    /// Ties a nested run to `parent`. With `shares_budget` the nested run measures its token
    /// ceiling against the run whose ceiling `parent` measures against.
    pub(crate) const fn new(parent: Arc<RunSpend>, shares_budget: bool) -> Self {
        Self {
            parent,
            shares_budget,
        }
    }
}

impl RunSpend {
    /// Opens the live record of a run whose ledger already holds `ledger`.
    pub(crate) fn new(
        ledger: &Usage,
        nested: Option<NestedSpend>,
        rollout_budget: Option<Arc<RolloutBudget>>,
    ) -> Arc<Self> {
        let (parent, budget_owner) = match nested {
            Some(nested) => {
                let owner = nested.shares_budget.then(|| {
                    nested
                        .parent
                        .budget_owner
                        .clone()
                        .unwrap_or_else(|| Arc::clone(&nested.parent))
                });
                (Some(nested.parent), owner)
            }
            None => (None, None),
        };
        Arc::new(Self {
            inner: Mutex::new(SpendInner {
                total: ledger.clone(),
                nested: Usage::default(),
            }),
            parent,
            budget_owner,
            rollout_budget,
        })
    }

    /// Records a call the run itself paid for, which its ledger has just recorded too.
    pub(crate) fn record_own(&self, usage: &Usage) {
        {
            let mut inner = self.lock();
            inner.total = inner.total.accumulate(usage);
        }
        self.forward(usage);
    }

    /// Records a call a nested run paid for, which this run's ledger does not hold yet.
    fn record_nested(&self, usage: &Usage) {
        {
            let mut inner = self.lock();
            inner.total = inner.total.accumulate(usage);
            inner.nested = inner.nested.accumulate(usage);
        }
        self.forward(usage);
    }

    fn forward(&self, usage: &Usage) {
        if let Some(budget) = &self.rollout_budget {
            budget.record_usage(usage);
        }
        if let Some(parent) = &self.parent {
            parent.record_nested(usage);
        }
    }

    /// Takes the spend nested runs recorded since the last call, for the run's ledger.
    ///
    /// Taking rather than reading, for the reason the legacy checkpoint field is taken: the value
    /// has exactly one destination, and one that could be read twice could also be added twice.
    pub(crate) fn take_nested(&self) -> Usage {
        std::mem::take(&mut self.lock().nested)
    }

    /// The spend this run's token ceiling is measured against.
    pub(crate) fn budget_tokens(&self) -> u64 {
        self.budget_owner
            .as_deref()
            .unwrap_or(self)
            .lock()
            .total
            .total_tokens()
    }

    /// The usage the code running in this run reads: the live total of the outermost run of its
    /// agent-tool chain, which every nested call reaches as it is paid for.
    pub(crate) fn shared_usage(&self) -> Usage {
        let mut outermost = self;
        while let Some(parent) = &outermost.parent {
            outermost = parent;
        }
        outermost.lock().total.clone()
    }

    /// Whether the rollout budget of the agent tree this run belongs to — directly, or through
    /// the run whose tool call started it — is used up.
    pub(crate) fn rollout_budget_exhausted(&self) -> bool {
        self.rollout_budget
            .as_ref()
            .is_some_and(|budget| budget.is_exhausted())
            || self
                .parent
                .as_ref()
                .is_some_and(|parent| parent.rollout_budget_exhausted())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, SpendInner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}
