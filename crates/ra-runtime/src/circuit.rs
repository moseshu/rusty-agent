//! Semantic loop breaker (tool plus normalized-argument fingerprint).
//!
//! # A refusal is an observation, not a failed run
//!
//! The breaker exists to make the model change its hypothesis, which it can only do if it hears
//! about the refusal. So this module *returns* the refusal and dispatch reports it as a
//! model-visible tool result; nothing here propagates. Ending the run instead would lose every
//! item the run had already produced, cancel the unrelated calls sharing the batch, and tell the
//! model nothing — the one participant whose behaviour has to change.

use ra_core::{
    error::{Error, ToolErrorKind},
    tool::ToolOptions,
};

/// Decides whether one call may proceed, given how long its repeat streak already is.
///
/// Returns the refusal to report, or `None` when the call may run.
///
/// # The streak already counts this call
///
/// Settlement records a whole turn before executing any of it, so the streak this reads includes
/// the call being admitted — and every identical call beside it in the same response. Refusing at
/// `streak >= limit` therefore refuses all `N` identical calls of a single response once `N`
/// reaches the limit, the first one included. That is the intended reading: `N` identical parallel
/// calls in one response is itself the pathology, and letting one through would make the breaker's
/// behaviour depend on which task the scheduler happened to start first.
///
/// The consequence to keep in mind is that a refused call is recorded too, so the streak keeps
/// growing while the tool is being refused. Nothing lowers it except a call with different
/// arguments. That is why the threshold is opt-in per tool rather than a framework default.
pub(crate) fn admit_repeat(
    options: &ToolOptions,
    repeat_streak: u32,
    tool_name: &str,
) -> Option<Error> {
    let limit = options.max_repeat_streak()?;
    (repeat_streak >= limit.get()).then(|| {
        Error::tool(
            ToolErrorKind::RepeatedCall,
            tool_name,
            format!(
                "the call repeated identical arguments {repeat_streak} times, at or above the \
                 tool's limit of {limit}"
            ),
        )
    })
}
