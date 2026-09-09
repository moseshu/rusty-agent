//! Running a run's input and output guardrails, and turning a tripwire into the run's error.
//!
//! The contract itself is [`ra_core::guardrail`]; what is here is only the execution: when each
//! stage runs, what it is handed, and what a firing leaves behind in the trace.
//!
//! # Blocking checks first, then the raced ones
//!
//! [`InputGuardrail::run_in_parallel`] decides which of the two an input check is, and the default
//! is to race. The blocking ones are awaited before the first request is built, so a tripwire in
//! one of them stops the run before anything is sent; the racing ones are started alongside that
//! request and stop the run the moment they trip. Upstream splits the same list the same way.
//!
//! # Concurrent, but not spawned
//!
//! Every guardrail of one stage is polled on the same task, so a stage costs the slowest check
//! rather than the sum of them. Spawning instead would buy real parallelism for what is almost
//! always IO-bound work, at the cost of a `'static` bound on the run context every guardrail reads.
//!
//! # A tripwire keeps the verdicts, including its own
//!
//! A stage returns [`StageOutcome`]: every verdict that completed, **and** the refusal if one of
//! them tripped. Both halves, always — a caller records the verdicts and then propagates the stop,
//! so a refused run still carries the evidence for why, and the checks that had already passed are
//! not lost with it. Upstream does the same thing through a results sink its dispatcher appends to
//! before raising.
//!
//! Returning `Result` here instead would discard exactly the evidence a refusal is argued from: the
//! error carries a rendered message, and `output_info` — the classifier score, the rule that
//! matched — would exist only inside that string.
//!
//! Checks still pending when one trips are dropped rather than awaited, which cancels them. A run
//! that has already been refused should not go on paying for the checks that would have agreed.

use std::{sync::Arc, time::Instant};

use futures::{StreamExt, stream::FuturesUnordered};
use ra_core::{
    cancel::CancelScope,
    context::RunContext,
    error::{Error, GuardrailStage, Result},
    guardrail::{
        GuardrailEvidence, GuardrailFinalOutput, GuardrailFunctionOutput, InputGuardrail,
        InputGuardrailResult, OutputGuardrail, OutputGuardrailResult,
    },
    item::ModelInputItem,
    trace::{self, SpanKind},
};
use tracing::{Instrument, debug_span};

use crate::tool::dispatch::duration_ms;

/// What one guardrail stage produced: its verdicts, and the refusal if one of them tripped.
///
/// The two are returned together rather than as a `Result` because they are not alternatives. A
/// stage that tripped still reached verdicts — the tripping one included — and those verdicts are
/// what a checkpoint, a report, and the refusal's own justification are read from.
#[must_use]
pub(crate) struct StageOutcome<R> {
    results: Vec<R>,
    stop: Option<Error>,
}

impl<R> StageOutcome<R> {
    /// An empty stage: nothing declared, nothing to record, nothing to stop for.
    ///
    /// Also what a caller reports for a stage it abandoned — a raced check dropped because the
    /// turn it raced already failed reached no verdict, and an empty outcome says exactly that.
    pub(crate) fn empty_stage() -> Self {
        Self::empty()
    }

    fn empty() -> Self {
        Self {
            results: Vec::new(),
            stop: None,
        }
    }

    /// Whether one of these verdicts refuses the run.
    ///
    /// For a caller that has to act on the refusal before it can consume the outcome — the input
    /// race cancels the turn it was racing at this point, and cancelling only on an `Err` would
    /// miss every tripwire, since a tripped stage still returns its verdicts.
    pub(crate) const fn refuses(&self) -> bool {
        self.stop.is_some()
    }

    /// The verdicts and the refusal, in the order a caller has to handle them.
    ///
    /// Taking both at once is deliberate: a caller that could reach the error without the verdicts
    /// would be able to report a refusal it has no evidence for.
    pub(crate) fn into_parts(self) -> (Vec<R>, Option<Error>) {
        (self.results, self.stop)
    }
}

/// The input stage, prepared but not started.
///
/// It owns its context and its copy of the input because it is raced against the first turn, and
/// that turn holds the run's state mutably for as long as it lasts. A borrow here would make the
/// two mutually exclusive, which is the one thing this stage exists not to be.
pub(crate) struct InputGuardrailCheck {
    guardrails: Vec<Arc<dyn InputGuardrail>>,
    context: RunContext,
    input: Vec<ModelInputItem>,
}

impl InputGuardrailCheck {
    /// Prepares the stage, or `None` when this segment has nothing to check.
    ///
    /// `None` rather than an empty check so that such a segment does not enter the race at all:
    /// the ordinary path stays the one with nothing in it.
    pub(crate) fn prepare(
        guardrails: Vec<Arc<dyn InputGuardrail>>,
        context: RunContext,
        input: Vec<ModelInputItem>,
    ) -> Option<Self> {
        (!guardrails.is_empty()).then_some(Self {
            guardrails,
            context,
            input,
        })
    }

    /// Runs the checks that must finish before the first request, leaving the raced ones behind.
    ///
    /// Drains rather than filtering twice: what stays in `self` is exactly what [`Self::run`] will
    /// race against the first model call, so the two halves cannot both claim a guardrail or both
    /// skip one.
    pub(crate) async fn run_blocking(
        &mut self,
        cancel: &CancelScope,
    ) -> Result<StageOutcome<InputGuardrailResult>> {
        let (parallel, blocking): (Vec<_>, Vec<_>) = std::mem::take(&mut self.guardrails)
            .into_iter()
            .partition(|guardrail| guardrail.run_in_parallel());
        self.guardrails = parallel;
        if blocking.is_empty() {
            return Ok(StageOutcome::empty());
        }
        run_input_checks(&blocking, &self.context, &self.input, cancel).await
    }

    /// Whether anything is left to race against the first model call.
    pub(crate) fn is_empty(&self) -> bool {
        self.guardrails.is_empty()
    }

    /// Runs the checks that race the first model call, stopping at the first tripwire.
    pub(crate) async fn run(
        self,
        cancel: &CancelScope,
    ) -> Result<StageOutcome<InputGuardrailResult>> {
        let Self {
            guardrails,
            context,
            input,
        } = self;
        if guardrails.is_empty() {
            return Ok(StageOutcome::empty());
        }
        run_input_checks(&guardrails, &context, &input, cancel).await
    }
}

/// Runs one set of input checks concurrently, keeping every verdict that completed.
///
/// The outer `Result` is for a guardrail that could not reach a verdict at all: it has not decided
/// anything, and reporting it as a refusal would blame the input for the checker being down.
async fn run_input_checks(
    guardrails: &[Arc<dyn InputGuardrail>],
    context: &RunContext,
    input: &[ModelInputItem],
    cancel: &CancelScope,
) -> Result<StageOutcome<InputGuardrailResult>> {
    let checks: FuturesUnordered<_> = guardrails
        .iter()
        .map(|guardrail| async move {
            let output = run_one(
                guardrail.name(),
                GuardrailStage::Input,
                guardrail.check(context, input),
            )
            .await?;
            Ok(InputGuardrailResult::new(guardrail.name(), output))
        })
        .collect();
    // Third-party `async` code is never awaited bare: a guardrail that ignored a stop signal would
    // keep the run alive after the user asked it to stop, with no tool left running to blame.
    cancel
        .run(collect_until_tripwire(
            checks,
            InputGuardrailResult::tripwire_error,
            GuardrailEvidence::Input,
        ))
        .await?
}

/// Runs every declared output guardrail against what the run is about to deliver.
pub(crate) async fn run_output_guardrails(
    guardrails: &[Arc<dyn OutputGuardrail>],
    context: &RunContext,
    output: &GuardrailFinalOutput<'_>,
    cancel: &CancelScope,
) -> Result<StageOutcome<OutputGuardrailResult>> {
    if guardrails.is_empty() {
        return Ok(StageOutcome::empty());
    }
    let checks: FuturesUnordered<_> = guardrails
        .iter()
        .map(|guardrail| async move {
            let verdict = run_one(
                guardrail.name(),
                GuardrailStage::Output,
                guardrail.check(context, output),
            )
            .await?;
            Ok(OutputGuardrailResult::new(guardrail.name(), verdict))
        })
        .collect();
    cancel
        .run(collect_until_tripwire(
            checks,
            OutputGuardrailResult::tripwire_error,
            GuardrailEvidence::Output,
        ))
        .await?
}

/// Drives a set of checks, keeping each verdict as it completes and stopping at the first tripwire.
///
/// The tripping verdict is kept too, and it is kept *before* the stop is built: a refusal whose
/// evidence was dropped on the way out is a refusal nobody can audit.
async fn collect_until_tripwire<R, F, E>(
    mut checks: FuturesUnordered<impl Future<Output = Result<R>>>,
    tripwire_error: F,
    evidence: E,
) -> Result<StageOutcome<R>>
where
    R: Clone,
    F: Fn(&R) -> Option<Error>,
    E: Fn(Vec<R>) -> GuardrailEvidence,
{
    let mut results = Vec::new();
    while let Some(verdict) = checks.next().await {
        let result = verdict?;
        let stop = tripwire_error(&result);
        results.push(result);
        if let Some(error) = stop {
            // Attached before the error leaves this function. A run refused this way returns no
            // result, so the error is the only thing a host receives — evidence recorded anywhere
            // else is evidence it cannot reach.
            let error = error.with_guardrail_evidence(evidence(results.clone()));
            // The rest are dropped here rather than awaited, which cancels them.
            return Ok(StageOutcome {
                results,
                stop: Some(error),
            });
        }
    }
    Ok(StageOutcome {
        results,
        stop: None,
    })
}

/// Runs one guardrail inside its own span and records what the span is for.
///
/// The verdict is recorded whichever way it went. A span that appeared only when a guardrail
/// tripped would answer "how often does this fire" with a number whose denominator is missing.
async fn run_one(
    id: &str,
    stage: GuardrailStage,
    check: impl Future<Output = Result<GuardrailFunctionOutput>>,
) -> Result<GuardrailFunctionOutput> {
    let span = debug_span!(
        "guardrail",
        span.kind = SpanKind::Guardrail.label(),
        guardrail.id = %id,
        guardrail.stage = stage.code(),
        guardrail.triggered = tracing::field::Empty,
        outcome = tracing::field::Empty,
        error.code = tracing::field::Empty,
        duration.ms = tracing::field::Empty,
    );
    let started = Instant::now();
    let verdict = check.instrument(span.clone()).await;
    span.record(trace::field::DURATION_MS, duration_ms(started.elapsed()));
    match &verdict {
        Ok(output) => {
            span.record(
                trace::field::GUARDRAIL_TRIGGERED,
                output.tripwire_triggered(),
            );
            trace::record_outcome(&span, trace::SpanOutcome::Ok);
        }
        // The check itself failed, so there is no verdict to report — `guardrail.triggered` stays
        // empty rather than being recorded as `false`, which would claim the guardrail looked and
        // found nothing.
        Err(error) => trace::record_error(&span, error),
    }
    verdict
}
