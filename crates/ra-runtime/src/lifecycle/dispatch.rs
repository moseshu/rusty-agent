//! Telling a run's lifecycle hooks about one moment.
//!
//! The contract is [`ra_core::lifecycle`] and the index is [`LifecycleHooks`]; what is here is only
//! the execution — who is told, as which scope, and what a firing leaves in the trace. There is no
//! reduction step, because there is nothing to reduce: this family observes, so the only thing
//! several hooks can disagree about is whether they succeeded.
//!
//! # Concurrent, and everybody is told
//!
//! One moment's hooks are polled together on the same task through
//! [`join_all`](futures::future::join_all), so the moment costs the slowest one rather than the sum
//! of them. Nobody is skipped when a neighbour fails: how often a hook was told out of how often
//! the moment happened is the number a host keeps, and a denominator that depends on where a name
//! sits in a list is not one.
//!
//! The futures are built run scope first and each scope in declaration order, and `join_all`
//! returns them in that order, so **which** failure a run reports when two hooks fail at one moment
//! is the same on every run.
//!
//! # Never awaited bare
//!
//! A lifecycle hook is third-party `async` code the host installed, so every moment runs inside the
//! caller's [`CancelScope`]. A hook that ignored a stop signal would otherwise keep the run alive
//! after the user asked it to stop, with nothing else still running to blame for it.

use std::time::Instant;

use futures::future::join_all;
use ra_core::{
    cancel::CancelScope,
    error::{Error, Result},
    lifecycle::{
        AgentEndInput, AgentStartInput, HandoffInput, LifecycleEvent, LifecycleScope, LlmEndInput,
        LlmStartInput, ToolEndInput, ToolStartInput,
    },
    trace,
};
use tracing::{Instrument, debug_span};

use crate::lifecycle::LifecycleHooks;
use crate::tool::dispatch::duration_ms;

/// Tells both scopes that an agent became the one running.
pub async fn agent_start(
    hooks: &LifecycleHooks,
    input: &AgentStartInput<'_>,
    cancel: &CancelScope,
) -> Result<()> {
    let event = LifecycleEvent::AgentStart;
    if hooks.is_empty() {
        return Ok(());
    }
    let told = hooks
        .all_scoped()
        .map(|(scope, hook)| observe(hook, event, scope, hook.on_agent_start(scope, input)));
    settle(told, cancel).await
}

/// Tells both scopes that an agent produced the answer the run delivers.
pub async fn agent_end(
    hooks: &LifecycleHooks,
    input: &AgentEndInput<'_>,
    cancel: &CancelScope,
) -> Result<()> {
    let event = LifecycleEvent::AgentEnd;
    if hooks.is_empty() {
        return Ok(());
    }
    let told = hooks
        .all_scoped()
        .map(|(scope, hook)| observe(hook, event, scope, hook.on_agent_end(scope, input)));
    settle(told, cancel).await
}

/// Tells both scopes that the model is about to be called.
pub async fn llm_start(
    hooks: &LifecycleHooks,
    input: &LlmStartInput<'_>,
    cancel: &CancelScope,
) -> Result<()> {
    let event = LifecycleEvent::LlmStart;
    if hooks.is_empty() {
        return Ok(());
    }
    let told = hooks
        .all_scoped()
        .map(|(scope, hook)| observe(hook, event, scope, hook.on_llm_start(scope, input)));
    settle(told, cancel).await
}

/// Tells both scopes what the model call produced.
pub async fn llm_end(
    hooks: &LifecycleHooks,
    input: &LlmEndInput<'_>,
    cancel: &CancelScope,
) -> Result<()> {
    let event = LifecycleEvent::LlmEnd;
    if hooks.is_empty() {
        return Ok(());
    }
    let told = hooks
        .all_scoped()
        .map(|(scope, hook)| observe(hook, event, scope, hook.on_llm_end(scope, input)));
    settle(told, cancel).await
}

/// Tells both scopes that a tool is about to be invoked.
pub async fn tool_start(
    hooks: &LifecycleHooks,
    input: &ToolStartInput<'_>,
    cancel: &CancelScope,
) -> Result<()> {
    let event = LifecycleEvent::ToolStart;
    if hooks.is_empty() {
        return Ok(());
    }
    let told = hooks
        .all_scoped()
        .map(|(scope, hook)| observe(hook, event, scope, hook.on_tool_start(scope, input)));
    settle(told, cancel).await
}

/// Tells both scopes how a tool invocation settled.
pub async fn tool_end(
    hooks: &LifecycleHooks,
    input: &ToolEndInput<'_>,
    cancel: &CancelScope,
) -> Result<()> {
    let event = LifecycleEvent::ToolEnd;
    if hooks.is_empty() {
        return Ok(());
    }
    let told = hooks
        .all_scoped()
        .map(|(scope, hook)| observe(hook, event, scope, hook.on_tool_end(scope, input)));
    settle(told, cancel).await
}

/// Tells both scopes that control is transferring, the agent scope belonging to the receiving agent.
pub async fn handoff(
    hooks: &LifecycleHooks,
    input: &HandoffInput<'_>,
    cancel: &CancelScope,
) -> Result<()> {
    let event = LifecycleEvent::Handoff;
    if hooks.is_empty() {
        return Ok(());
    }
    let told = hooks
        .all_scoped()
        .map(|(scope, hook)| observe(hook, event, scope, hook.on_handoff(scope, input)));
    settle(told, cancel).await
}

/// Runs one moment's hooks together and reports the first failure in the order they were built.
async fn settle<Told>(told: impl Iterator<Item = Told>, cancel: &CancelScope) -> Result<()>
where
    Told: Future<Output = Result<()>>,
{
    cancel.run(join_all(told)).await?.into_iter().collect()
}

/// Tells one hook about one moment, inside its own span.
async fn observe(
    hook: &dyn ra_core::lifecycle::LifecycleHook,
    event: LifecycleEvent,
    scope: LifecycleScope,
    told: impl Future<Output = Result<()>>,
) -> Result<()> {
    let span = debug_span!(
        "lifecycle_hook",
        hook.name = hook.name(),
        hook.event = event.code(),
        hook.scope = scope.code(),
        outcome = tracing::field::Empty,
        error.code = tracing::field::Empty,
        duration.ms = tracing::field::Empty,
    );
    let started = Instant::now();
    let answer = told.instrument(span.clone()).await;
    span.record(trace::field::DURATION_MS, duration_ms(started.elapsed()));
    match &answer {
        Ok(()) => trace::record_outcome(&span, trace::SpanOutcome::Ok),
        Err(error) => trace::record_error(&span, error),
    }
    answer.map_err(|error| attribute(error, hook.name(), event, scope))
}

/// Names the hook, the moment and the scope on the way out, leaving the host's own error kind
/// intact.
///
/// The error is the host's, not the framework's: this is code the host installed, and rewrapping
/// its failure as a framework error would change what a caller reading
/// [`Recoverability`](ra_core::error::Recoverability) does about it. The scope is part of the
/// attribution because one object may be installed at both, and "which of my two registrations
/// broke" is otherwise unanswerable.
fn attribute(error: Error, name: &str, event: LifecycleEvent, scope: LifecycleScope) -> Error {
    error.with_context(format!(
        "lifecycle hook `{name}` at `{}` ({} scope)",
        event.code(),
        scope.code()
    ))
}
