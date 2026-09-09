//! Running the checks a tool declares: its arguments before the call, its result before the model.
//!
//! The contract is [`ra_core::guardrail::tool`]; what is here is the execution — how a declared
//! [`ToolGuardrailId`] finds the object the host installed, when each stage runs, and what a
//! decision leaves behind.
//!
//! # A declaration is a lookup, so the identity has to be unique
//!
//! This is the one place a guardrail identity in this framework is not merely a display name. A
//! run-level guardrail is *installed* with its checks attached, and two of them may share a name
//! because nothing ever looks one up. A tool declares an ID and the runtime resolves it, so two
//! installations under one ID leave the resolution with no answer it can defend — running both
//! silently doubles what the tool asked for, and picking one makes the behaviour depend on
//! installation order. [`ToolGuardrails::install`] refuses the configuration instead.
//!
//! The mirror-image failure is a declaration nobody installed, and it is refused for the sharper
//! reason: letting the call run would execute it unchecked under a name that promises it was
//! checked. [`ToolGuardrails::preflight`] asks that question once, before the run's first model
//! call, so a misconfigured run stops before it has been paid for.
//!
//! # Checks run in declaration order
//!
//! Each completed decision is recorded before it is applied. The first rejection or raise ends
//! the chain, matching the upstream tool guardrails; later checks are never invoked.

use std::{collections::BTreeMap, sync::Arc, time::Instant};

use ra_core::{
    cancel::CancelScope,
    error::{Error, GuardrailStage, Result},
    guardrail::{
        GuardrailEvidence, ToolGuardrailFunctionOutput, ToolGuardrailVerdict, ToolInputGuardrail,
        ToolInputGuardrailData, ToolInputGuardrailResult, ToolOutputGuardrail,
        ToolOutputGuardrailData, ToolOutputGuardrailResult, reduce_tool_input_guardrails,
        reduce_tool_output_guardrails,
    },
    tool::{Tool, ToolGuardrailId, ToolOptions, ToolOrigin},
    trace::{self, SpanKind},
};
use tracing::{Instrument, debug_span};

use crate::tool::dispatch::duration_ms;

/// The tool guardrails a run installed, and whether they are also asked before an approval.
///
/// Cheap to clone: one dispatch chain per call holds a copy, and the installed set is shared.
#[must_use]
#[derive(Clone, Default)]
pub struct ToolGuardrails {
    installed: Option<Arc<Installed>>,
}

#[derive(Default)]
struct Installed {
    input: BTreeMap<ToolGuardrailId, Arc<dyn ToolInputGuardrail>>,
    output: BTreeMap<ToolGuardrailId, Arc<dyn ToolOutputGuardrail>>,
    check_before_approval: bool,
}

impl ToolGuardrails {
    /// Indexes what a host installed, refusing two objects under one identity.
    ///
    /// `check_before_approval` switches on the optional pre-approval pass over a call's arguments.
    /// It never replaces the check that happens after the answer comes back — see
    /// [`ToolInputGuardrail`] for why that one is not optional.
    pub fn install(
        input: impl IntoIterator<Item = Arc<dyn ToolInputGuardrail>>,
        output: impl IntoIterator<Item = Arc<dyn ToolOutputGuardrail>>,
        check_before_approval: bool,
    ) -> Result<Self> {
        let mut installed = Installed {
            check_before_approval,
            ..Installed::default()
        };
        for guardrail in input {
            let id = guardrail.id().clone();
            if installed.input.insert(id.clone(), guardrail).is_some() {
                return Err(duplicate(GuardrailStage::ToolInput, &id));
            }
        }
        for guardrail in output {
            let id = guardrail.id().clone();
            if installed.output.insert(id.clone(), guardrail).is_some() {
                return Err(duplicate(GuardrailStage::ToolOutput, &id));
            }
        }
        Ok(Self {
            installed: (!installed.input.is_empty() || !installed.output.is_empty())
                .then(|| Arc::new(installed)),
        })
    }

    /// Whether the host installed nothing at either boundary.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.installed.is_none()
    }

    /// Whether a call awaiting approval has its arguments checked before the host is interrupted.
    #[must_use]
    pub fn checks_before_approval(&self) -> bool {
        self.installed
            .as_ref()
            .is_some_and(|installed| installed.check_before_approval)
    }

    /// Checks that every identity these tools declare resolves to something installed.
    ///
    /// Asked once, before the run's first model call. A declaration that resolves to nothing is a
    /// configuration mistake, and finding it at dispatch time means finding it after a model call
    /// has been paid for and a tool is waiting to run.
    pub fn preflight<'a>(&self, tools: impl IntoIterator<Item = &'a Arc<dyn Tool>>) -> Result<()> {
        for tool in tools {
            let origin = tool.origin();
            let options = tool.options();
            self.resolve_input(&options, origin)?;
            self.resolve_output(&options, origin)?;
        }
        Ok(())
    }

    /// The input checks one tool declared, in declaration order.
    pub(crate) fn resolve_input(
        &self,
        options: &ToolOptions,
        origin: &ToolOrigin,
    ) -> Result<Vec<Arc<dyn ToolInputGuardrail>>> {
        let declared = options.input_guardrails();
        if declared.is_empty() {
            return Ok(Vec::new());
        }
        declared
            .iter()
            .map(|id| {
                self.installed
                    .as_ref()
                    .and_then(|installed| installed.input.get(id))
                    .map(Arc::clone)
                    .ok_or_else(|| unresolved(GuardrailStage::ToolInput, id, origin))
            })
            .collect()
    }

    /// The output checks one tool declared, in declaration order.
    pub(crate) fn resolve_output(
        &self,
        options: &ToolOptions,
        origin: &ToolOrigin,
    ) -> Result<Vec<Arc<dyn ToolOutputGuardrail>>> {
        let declared = options.output_guardrails();
        if declared.is_empty() {
            return Ok(Vec::new());
        }
        declared
            .iter()
            .map(|id| {
                self.installed
                    .as_ref()
                    .and_then(|installed| installed.output.get(id))
                    .map(Arc::clone)
                    .ok_or_else(|| unresolved(GuardrailStage::ToolOutput, id, origin))
            })
            .collect()
    }
}

impl std::fmt::Debug for ToolGuardrails {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (input, output) = self.installed.as_ref().map_or((0, 0), |installed| {
            (installed.input.len(), installed.output.len())
        });
        formatter
            .debug_struct("ToolGuardrails")
            .field("input", &input)
            .field("output", &output)
            .field("check_before_approval", &self.checks_before_approval())
            .finish()
    }
}

fn duplicate(stage: GuardrailStage, id: &ToolGuardrailId) -> Error {
    Error::config(format!(
        "two {} guardrails are installed as `{id}`; a tool declares this identity to select one \
         of them, so the run has no way to say which it meant",
        stage.code()
    ))
}

fn unresolved(stage: GuardrailStage, id: &ToolGuardrailId, origin: &ToolOrigin) -> Error {
    Error::config(format!(
        "tool `{}` declares the {} guardrail `{id}`, which this run did not install; the call \
         would otherwise run unchecked under a name that says it was checked",
        origin.qualified_name(),
        stage.code()
    ))
}

/// Runs input checks in declaration order until one rejects or raises.
pub(crate) async fn check_tool_input(
    guardrails: &[Arc<dyn ToolInputGuardrail>],
    data: &ToolInputGuardrailData<'_>,
    cancel: &CancelScope,
    results: &mut Vec<ToolInputGuardrailResult>,
) -> Result<ToolGuardrailVerdict> {
    let first = results.len();
    for guardrail in guardrails {
        // Each callback is cancellation-scoped, including a check that never returns itself.
        let output = cancel
            .run(run_one(
                guardrail.id(),
                GuardrailStage::ToolInput,
                guardrail.check(data),
            ))
            .await??;
        results.push(ToolInputGuardrailResult::new(
            guardrail.id().clone(),
            data.origin().clone(),
            data.call_id().clone(),
            output.clone(),
        ));
        let verdict =
            reduce_tool_input_guardrails([(guardrail.id().clone(), output)]).map_err(|error| {
                error.with_guardrail_evidence(GuardrailEvidence::ToolInput(
                    results[first..].to_vec(),
                ))
            })?;
        if !verdict.is_allow() {
            return Ok(verdict);
        }
    }
    Ok(ToolGuardrailVerdict::Allow)
}

/// Runs output checks in declaration order until one rejects or raises.
pub(crate) async fn check_tool_output(
    guardrails: &[Arc<dyn ToolOutputGuardrail>],
    data: &ToolOutputGuardrailData<'_>,
    cancel: &CancelScope,
    results: &mut Vec<ToolOutputGuardrailResult>,
) -> Result<ToolGuardrailVerdict> {
    let first = results.len();
    for guardrail in guardrails {
        // Each callback is cancellation-scoped, including a check that never returns itself.
        let output = cancel
            .run(run_one(
                guardrail.id(),
                GuardrailStage::ToolOutput,
                guardrail.check(data),
            ))
            .await??;
        results.push(ToolOutputGuardrailResult::new(
            guardrail.id().clone(),
            data.origin().clone(),
            data.call_id().clone(),
            output.clone(),
        ));
        let verdict =
            reduce_tool_output_guardrails([(guardrail.id().clone(), output)]).map_err(|error| {
                error.with_guardrail_evidence(GuardrailEvidence::ToolOutput(
                    results[first..].to_vec(),
                ))
            })?;
        if !verdict.is_allow() {
            return Ok(verdict);
        }
    }
    Ok(ToolGuardrailVerdict::Allow)
}

/// Runs one check inside its own span and records what it decided.
///
/// The decision is recorded whichever way it went, for the reason the run-level stage records its
/// verdicts: a span that appeared only on a refusal answers "how often does this fire" with a
/// number whose denominator is missing.
async fn run_one(
    id: &ToolGuardrailId,
    stage: GuardrailStage,
    check: impl Future<Output = Result<ToolGuardrailFunctionOutput>>,
) -> Result<ToolGuardrailFunctionOutput> {
    let span = debug_span!(
        "guardrail",
        span.kind = SpanKind::Guardrail.label(),
        guardrail.id = %id,
        guardrail.stage = stage.code(),
        guardrail.triggered = tracing::field::Empty,
        guardrail.behavior = tracing::field::Empty,
        outcome = tracing::field::Empty,
        error.code = tracing::field::Empty,
        duration.ms = tracing::field::Empty,
    );
    let started = Instant::now();
    let decision = check.instrument(span.clone()).await;
    span.record(trace::field::DURATION_MS, duration_ms(started.elapsed()));
    match &decision {
        Ok(output) => {
            // Both fields, because neither answers the other's question. `triggered` is the one a
            // run-level guardrail also records, so a report can count firings across all four
            // boundaries; `behavior` is what separates a call the model gets to retry from one that
            // ended the run.
            span.record(trace::field::GUARDRAIL_TRIGGERED, !output.is_allow());
            span.record(trace::field::GUARDRAIL_BEHAVIOR, output.behavior().code());
            trace::record_outcome(&span, trace::SpanOutcome::Ok);
        }
        // No decision to report, so both fields stay empty rather than recording an allow — which
        // would claim the check looked and found nothing.
        Err(error) => trace::record_error(&span, error),
    }
    decision
}
