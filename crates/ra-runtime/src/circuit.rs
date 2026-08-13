//! Loop breakers: the same call again, and the same answer again.
//!
//! Two questions, deliberately not one. [`admit_repeat`] reads the request and asks whether the
//! model is making a call it just made; [`admit_progress`] reads the record of what came back and
//! asks whether the run has learned anything from a tool that keeps failing. A model that fixes a
//! path and fails on the next one passes the first and is caught by the second; a model that runs
//! the same passing command twice passes the second and is caught by the first. Folding them into
//! one counter would answer neither question.
//!
//! # A refusal is an observation, not a failed run
//!
//! Both breakers exist to make the model change its approach, which it can only do if it hears
//! about the refusal. So this module *returns* refusals and dispatch reports them as model-visible
//! tool results; nothing here propagates. Ending the run instead would lose every item the run had
//! already produced, cancel the unrelated calls sharing the batch, and tell the model nothing — the
//! one participant whose behaviour has to change.
//!
//! The refusal carries a machine-readable code and the tool's name, and no prose. Which code it is
//! *is* the constraint on what to do next: `tool.repeated_call` says the arguments have to change,
//! `tool.no_progress` says changing them is not enough.

use ra_core::{
    error::{Error, ToolErrorKind},
    tool::ToolOptions,
};

use crate::tool::dispatch::CallHistory;

/// Decides whether one call may proceed, given what the run's records already say about it.
///
/// Returns the refusal to report, or `None` when the call may run.
pub(crate) fn admit(options: &ToolOptions, history: CallHistory, tool_name: &str) -> Option<Error> {
    admit_repeat(options, history.repeat_streak(), tool_name)
        .or_else(|| admit_progress(options, history.no_progress_streak(), tool_name))
}

/// Refuses a call that repeats the arguments of the calls before it.
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
/// arguments. That is why this threshold is opt-in per tool rather than a framework default.
fn admit_repeat(options: &ToolOptions, repeat_streak: u32, tool_name: &str) -> Option<Error> {
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

/// Refuses a tool that keeps failing without telling the run anything new.
///
/// # It acts between responses, never inside one
///
/// The streak this reads is the one the run had when the response arrived, because an outcome does
/// not exist until its call has run and outcomes are filed once the whole turn is settled. Several
/// calls in one response therefore all pass admission on the same number: a response containing
/// four hopeless calls runs all four, files four failures, and the *next* response's first call is
/// the one refused. Enforcement is a response late, deliberately.
///
/// The alternative — ordering same-identity calls so each sees the previous one's outcome — was
/// tried and reverted. It silently withdraws [`ToolConcurrency::Parallel`](ra_core::tool::ToolConcurrency::Parallel)
/// from every tool that does not exempt itself, and it pays that cost on the wrong calls: chains
/// form per identity while this breaker fires only after repeated failures produce the same
/// *evidence*. Serializing preemptively slows independent parallel calls before that evidence
/// exists. A batch-strict policy is coherent, but it has to be something a tool asks for, not
/// something a default implies.
///
/// This is where the two breakers differ. [`admit_repeat`] reads a trail that is complete before
/// any of the turn runs, so it sees the calls beside it; this one cannot.
///
/// # Why this one is on by default
///
/// The streak it reads counts *failures whose result the run had already been given*, which a run
/// never produces for a good reason. Unlike a repeat streak it is not advanced by a call that
/// merely looks like an earlier one, and unlike a repeat streak it does not latch: firing it
/// clears the streak, so the next call is judged on its own and a tool is never lost for the rest
/// of the run. A false positive therefore costs one refused call, and the model is told which
/// constraint it hit. See
/// [`ToolOutcome::refused`](ra_core::state::ToolOutcome::refused) for why clearing is the only
/// coherent choice here.
fn admit_progress(
    options: &ToolOptions,
    no_progress_streak: u32,
    tool_name: &str,
) -> Option<Error> {
    let limit = options.max_no_progress_streak()?;
    (no_progress_streak >= limit.get()).then(|| {
        Error::tool(
            ToolErrorKind::NoProgress,
            tool_name,
            format!(
                "the tool failed {no_progress_streak} times in a row without producing new \
                 evidence, at or above the tool's limit of {limit}"
            ),
        )
    })
}
