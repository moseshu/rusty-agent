//! Runtime helpers for budget enforcement and model-facing budget guidance.

use ra_core::{budget::BudgetLimit, item::Message, state::RunState};

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
pub(crate) fn budget_reminder(state: &RunState, limit: &BudgetLimit) -> Option<Message> {
    let remaining = state.remaining_tokens(limit)?;
    Some(Message::user(format!(
        "Task token budget: {remaining} tokens remain. Pace the remaining work accordingly."
    )))
}
