//! One tool invocation, in the only permitted stage order (R3-4).
//!
//! The chain is typed argument decoding -> repeat admission -> approval -> input guardrail ->
//! invoke -> output guardrail. Every stage that can refuse does so by **returning a value**, never
//! by throwing prose a caller has to read. That is what keeps the turn's control flow out of error
//! strings.
//!
//! # Framework error text must never reach the model
//!
//! A model-visible failure observation carries a machine-readable code and the tool's qualified
//! name — and nothing else. [`Error`]'s `Display` and `user_message` are written for logs and for
//! the person at the keyboard, in this project's own language; splicing either into a tool result
//! would put framework prose into the model's context and make the next turn depend on it. A tool
//! that wants to explain a failure to the model has a dedicated path:
//! [`ToolFailureHandling::Custom`] plus [`Tool::handle_failure`], which lets the tool write its own
//! model-facing text.
//!
//! Concurrency ceilings and batching are deliberately absent: R3-4b owns the batch shape and R3-4c
//! owns what happens when several of these run at once.

use std::{sync::Arc, time::Instant};

use ra_core::{
    cancel::CancelScope,
    context::RunContext,
    error::{Error, Result, ToolErrorKind},
    item::{CallId, ToolApproval, ToolCallOutput},
    tool::{
        Tool, ToolApprovalPolicy, ToolCaller, ToolConcurrency, ToolContext, ToolFailureHandling,
        ToolOptions, ToolOutput, ToolServices, ToolTimeoutBehavior,
    },
};
use serde_json::{Value, json};

use crate::circuit;

/// What the chain decided about one call.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub enum ToolDispatch {
    /// The tool ran and reached a model-visible result. A failure the model is meant to react to
    /// is also this variant; only failures that must stop the turn leave as [`Err`].
    Observed(ToolObservation),
    /// The chain answered without running the tool.
    ///
    /// A separate state from a failed [`Self::Observed`], because "the tool ran and failed" and
    /// "the tool never ran" are different facts and the records built from them differ. Deriving
    /// this from `is_error` instead would put the two on the same footing: the no-progress counter
    /// would count its own refusals as evidence about the tool.
    Refused(ToolRefusal),
    /// A host has to decide before the tool may run.
    AwaitingApproval(ToolApproval),
}

/// What the model is told when the chain declines to run a call.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub struct ToolRefusal {
    output: ToolCallOutput,
    code: &'static str,
}

impl ToolRefusal {
    /// Records a refusal under the stable code the model was shown.
    #[must_use]
    pub const fn new(output: ToolCallOutput, code: &'static str) -> Self {
        Self { output, code }
    }

    /// The result the model is shown.
    #[must_use]
    pub const fn output(&self) -> &ToolCallOutput {
        &self.output
    }

    /// Stable code of the refusal.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        self.code
    }

    /// Takes the model-visible result out.
    #[must_use]
    pub fn into_output(self) -> ToolCallOutput {
        self.output
    }
}

/// A model-visible result, plus what the chain knows about it that the result does not show.
///
/// The failure code is carried beside the output rather than read back out of it, because the two
/// can disagree by design. A tool using [`ToolFailureHandling::Custom`] answers a failure with its
/// own sentence and no error flag — that is the whole point of the seam — and a record that
/// classified outcomes by looking at the rendered result would be blind to exactly the tools that
/// explain themselves best.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub struct ToolObservation {
    output: ToolCallOutput,
    failure_code: Option<&'static str>,
}

impl ToolObservation {
    /// Records a call that produced a usable result.
    #[must_use]
    pub const fn succeeded(output: ToolCallOutput) -> Self {
        Self {
            output,
            failure_code: None,
        }
    }

    /// Records a call that failed, under the stable code of the failure it produced.
    #[must_use]
    pub const fn failed(output: ToolCallOutput, code: &'static str) -> Self {
        Self {
            output,
            failure_code: Some(code),
        }
    }

    /// The result the model is shown.
    #[must_use]
    pub const fn output(&self) -> &ToolCallOutput {
        &self.output
    }

    /// Stable code of the failure, or `None` when the call produced a usable result.
    #[must_use]
    pub const fn failure_code(&self) -> Option<&'static str> {
        self.failure_code
    }

    /// Takes the model-visible result out.
    #[must_use]
    pub fn into_output(self) -> ToolCallOutput {
        self.output
    }
}

/// What the run's records already say about a call, projected by the batch.
///
/// The two counters are separate because they answer separate questions — see
/// [`circuit`](crate::circuit). Passing them as one value keeps a call site from transposing two
/// bare integers whose meanings are unrelated.
#[must_use]
#[non_exhaustive]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CallHistory {
    repeat_streak: u32,
    no_progress_streak: u32,
}

impl CallHistory {
    /// Creates the projection for one call.
    pub const fn new(repeat_streak: u32, no_progress_streak: u32) -> Self {
        Self {
            repeat_streak,
            no_progress_streak,
        }
    }

    /// Consecutive calls of this identity that carried the same arguments, including this one.
    #[must_use]
    pub const fn repeat_streak(&self) -> u32 {
        self.repeat_streak
    }

    /// Consecutive failures of this identity that showed the model nothing new.
    #[must_use]
    pub const fn no_progress_streak(&self) -> u32 {
        self.no_progress_streak
    }
}

/// Inputs for one invocation.
#[must_use]
#[non_exhaustive]
pub struct ToolDispatchRequest {
    tool: Arc<dyn Tool>,
    call_id: CallId,
    arguments: Value,
    run: Arc<RunContext>,
    cancel: CancelScope,
    history: CallHistory,
    caller: ToolCaller,
    services: ToolServices,
}

impl ToolDispatchRequest {
    /// Creates a request for a call the model made directly.
    ///
    /// `cancel` is required rather than optional for the same reason it is in turn preparation:
    /// [`Tool::call`] is third-party `async` code, and the cancellation contract does not allow it
    /// to be awaited bare.
    ///
    /// `run` is the live context of the run this call belongs to. It is shared rather than rebuilt
    /// per call, so every tool in one turn sees the same run identity, the same public agent, and
    /// the same host state.
    pub fn new(
        tool: Arc<dyn Tool>,
        call_id: CallId,
        arguments: Value,
        run: Arc<RunContext>,
        cancel: CancelScope,
        history: CallHistory,
    ) -> Self {
        Self {
            tool,
            call_id,
            arguments,
            run,
            cancel,
            history,
            caller: ToolCaller::Direct,
            services: ToolServices::new(),
        }
    }

    /// Sets the caller class this call is admitted under.
    pub const fn with_caller(mut self, caller: ToolCaller) -> Self {
        self.caller = caller;
        self
    }

    /// Sets the framework ports the tool is handed.
    pub fn with_services(mut self, services: ToolServices) -> Self {
        self.services = services;
        self
    }

    /// The tool being dispatched.
    #[must_use]
    pub fn tool(&self) -> &Arc<dyn Tool> {
        &self.tool
    }

    /// Builds the context for this call. Every stage that enters third-party code takes it from
    /// here, so a tool cannot be asked about a call under one context and then run under another.
    pub fn context(&self) -> ToolContext<'_> {
        ToolContext::new(
            &self.run,
            self.tool.as_ref(),
            &self.call_id,
            &self.arguments,
        )
        .with_caller(self.caller)
        .with_services(&self.services)
    }

    fn context_with_decoded(
        &self,
        decoded: Option<ra_core::tool::DecodedToolInput>,
    ) -> ToolContext<'_> {
        let context = self.context();
        match decoded {
            Some(decoded) => context.with_decoded_input(decoded),
            None => context,
        }
    }
}

/// Runs one call through the fixed chain.
pub async fn dispatch_tool(request: ToolDispatchRequest) -> Result<ToolDispatch> {
    dispatch_tool_with_admission(request, None, Instant::now()).await
}

/// Runs one call through the fixed chain, optionally coordinating with a batch resource admission gate.
pub(crate) async fn dispatch_tool_with_admission(
    request: ToolDispatchRequest,
    admission: Option<&crate::turn::batch::ResourceAdmissionGate>,
    admission_started: Instant,
) -> Result<ToolDispatch> {
    let tool = &request.tool;
    let options = tool.options();
    let name = tool.origin().qualified_name().to_owned();

    // 1. Caller admission. A tool that does not admit this caller class is, from that caller's
    // side, indistinguishable from one that is not there — so it reports as `not_found` rather
    // than inventing a second way to say "you may not call this".
    if !options.allows_caller(request.caller) {
        return Ok(refused(
            &request.call_id,
            &name,
            &Error::tool(
                ToolErrorKind::NotFound,
                &name,
                "the tool does not admit this caller class",
            ),
        ));
    }

    // Typed function tools are decoded exactly once at the common invocation boundary. A tool
    // implementation receives the checked value from its context instead of each implementation
    // independently reinterpreting provider JSON. Untyped and remote tools retain the parsed
    // JSON-only contract.
    let decoded_input = match tool.decode_input(&request.arguments) {
        Ok(decoded) => decoded,
        Err(error) => return shape_failure(tool, &request, &options, &name, error).await,
    };

    // 2. Loop-breaker admission, before approval so a host is not asked about a call that will not
    // run. Both breakers live behind one insertion point rather than being bolted onto whichever
    // call site notices the repetition first, and they refuse the way this stage chain refuses:
    // with an observation the model can react to.
    if let Some(reason) = circuit::admit(&options, request.history, &name) {
        return Ok(refused(&request.call_id, &name, &reason));
    }

    // 3. Approval, before anything runs. Static policies are answered from the declaration and
    // never enter third-party code, exactly as dynamic availability is handled in preparation.
    if needs_approval(tool, &options, &request).await? {
        let mut approval = ToolApproval::new(
            request.call_id.clone(),
            tool.model_definition().name(),
            request.arguments.clone(),
        );
        if let Some(namespace) = tool.origin().namespace() {
            approval = approval.with_namespace(namespace.as_str());
        }
        return Ok(ToolDispatch::AwaitingApproval(approval));
    }

    // 4. Input guardrail. R7-3 owns the contract; the stage exists so it lands in one place, and
    // so its position relative to approval is decided here rather than per call site.
    check_input_guardrails(&options)?;

    // 5. Dynamic resource claims evaluation.
    // Exclusive tools run alone under a global write gate and skip fine-grained claims.
    let claims = if matches!(options.concurrency(), ToolConcurrency::Parallel) {
        let claims_result = request
            .cancel
            .run(tool.resource_claims(&request.context()))
            .await;
        match claims_result {
            Ok(Ok(claims)) => claims,
            Ok(Err(error)) => return shape_failure(tool, &request, &options, &name, error).await,
            Err(error) => return Err(error),
        }
    } else {
        Vec::new()
    };

    // 6. Concurrency & Resource Admission (if gate is provided).
    let permits = if let Some(gate) = admission {
        let permits = gate
            .acquire_permits(&request.cancel, options.concurrency(), &claims)
            .await?;
        tracing::Span::current().record(
            ra_core::trace::field::TOOL_ADMISSION_WAIT_MS,
            duration_ms(admission_started.elapsed()),
        );
        request.cancel.ensure_not_cancelled()?;
        Some(permits)
    } else {
        None
    };

    // 7. Invoke tool.
    let outcome = invoke(
        tool,
        request.context_with_decoded(decoded_input),
        &options,
        &request.cancel,
        &name,
    )
    .await;

    // Release fine-grained resource locks immediately upon invoke completion.
    drop(permits);

    let output = match outcome {
        Ok(output) => output,
        Err(error) => return shape_failure(tool, &request, &options, &name, error).await,
    };

    // 8. Output guardrail, on the result the tool actually produced.
    check_output_guardrails(&options)?;

    observed_success(&request.call_id, &output)
}

/// Invokes the tool, applying its per-call time limit.
async fn invoke(
    tool: &Arc<dyn Tool>,
    context: ToolContext<'_>,
    options: &ToolOptions,
    cancel: &CancelScope,
    name: &str,
) -> Result<ToolOutput> {
    let started = Instant::now();
    let call = tool.call(context);
    let result = match options.timeout() {
        None => cancel.run(call).await.and_then(|result| result),
        Some(limit) => cancel
            .run(tokio::time::timeout(limit, call))
            .await
            .and_then(|result| match result {
                Ok(result) => result,
                Err(_elapsed) => Err(Error::tool(
                    ToolErrorKind::Timeout,
                    name,
                    format!("the tool exceeded its {limit:?} limit"),
                )),
            }),
    };
    tracing::Span::current().record(
        ra_core::trace::field::TOOL_EXECUTION_MS,
        duration_ms(started.elapsed()),
    );
    result
}

/// Converts elapsed time to the trace vocabulary's millisecond unit.
pub(crate) fn duration_ms(duration: std::time::Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// Turns an invocation failure into either a model-visible observation or a stopped turn.
pub(crate) async fn shape_failure(
    tool: &Arc<dyn Tool>,
    request: &ToolDispatchRequest,
    options: &ToolOptions,
    name: &str,
    error: Error,
) -> Result<ToolDispatch> {
    // A cancellation is never an observation. Reporting it to the model as a tool failure would
    // let the loop continue past the very thing that asked it to stop, and the run would keep
    // spending after the user interrupted it.
    if error.is_cancelled() {
        return Err(error);
    }

    let timed_out = matches!(
        error,
        Error::Tool {
            kind: ToolErrorKind::Timeout,
            ..
        }
    );
    if timed_out {
        return match options.timeout_behavior() {
            ToolTimeoutBehavior::ModelVisible => {
                Ok(observed_failure(&request.call_id, name, &error))
            }
            ToolTimeoutBehavior::Propagate => Err(error),
            behavior => Err(Error::caller(format!(
                "tool `{name}` uses an unsupported timeout behavior `{behavior:?}`"
            ))),
        };
    }

    match options.failure_handling() {
        ToolFailureHandling::ModelVisible => Ok(observed_failure(&request.call_id, name, &error)),
        ToolFailureHandling::Propagate => Err(error),
        // The one path where a tool writes its own model-facing failure text. Returning `None`
        // normally means the tool declined to handle it, and an unhandled failure propagates.
        // `ToolErrorKind::InvalidInput` is the exception: it is always model-visible so a malformed
        // call can be corrected. This also covers an InvalidInput emitted from a tool body, not
        // only the common decoding boundary.
        //
        // The result reads as a plain answer to the model, and the records still have to know a
        // call failed here: `run_tests` explaining two different failures in its own words is the
        // case the no-progress breaker exists to *not* fire on, and it can only tell those two
        // apart if it is told they were failures at all.
        ToolFailureHandling::Custom => {
            let context = request.context();
            match request
                .cancel
                .run(tool.handle_failure(&context, &error))
                .await??
            {
                Some(output) => observed(&request.call_id, &output, Some(error.code())),
                None if is_invalid_input(&error) => {
                    Ok(observed_failure(&request.call_id, name, &error))
                }
                None => Err(error),
            }
        }
        handling => Err(Error::caller(format!(
            "tool `{name}` uses an unsupported failure handling policy `{handling:?}`"
        ))),
    }
}

/// Whether this refusal is an argument object the model can correct on its next turn.
fn is_invalid_input(error: &Error) -> bool {
    matches!(
        error,
        Error::Tool {
            kind: ToolErrorKind::InvalidInput,
            ..
        }
    )
}

pub(crate) async fn needs_approval(
    tool: &Arc<dyn Tool>,
    options: &ToolOptions,
    request: &ToolDispatchRequest,
) -> Result<bool> {
    match options.approval() {
        ToolApprovalPolicy::Never => Ok(false),
        ToolApprovalPolicy::Always => Ok(true),
        ToolApprovalPolicy::Dynamic => {
            let context = request.context();
            request.cancel.run(tool.needs_approval(&context)).await?
        }
        policy => Err(Error::caller(format!(
            "tool `{}` uses an unsupported approval policy `{policy:?}`",
            tool.origin().qualified_name()
        ))),
    }
}

// The two stages below always succeed today and will not once their owning milestone lands.
// Narrowing the return type now would take the `?` off the call sites, and a stage that reads like
// a no-op is a stage someone tidies away — which is exactly what having one insertion point per
// milestone is meant to prevent.

/// R7-3's insertion point for tool input guardrails.
#[allow(clippy::unnecessary_wraps)]
pub(crate) const fn check_input_guardrails(_options: &ToolOptions) -> Result<()> {
    Ok(())
}

/// R7-3's insertion point for tool output guardrails.
#[allow(clippy::unnecessary_wraps)]
pub(crate) const fn check_output_guardrails(_options: &ToolOptions) -> Result<()> {
    Ok(())
}

pub(crate) fn observed_success(call_id: &CallId, output: &ToolOutput) -> Result<ToolDispatch> {
    observed(call_id, output, None)
}

/// Renders a tool's own output as the model-visible result, classified for the records.
pub(crate) fn observed(
    call_id: &CallId,
    output: &ToolOutput,
    failure_code: Option<&'static str>,
) -> Result<ToolDispatch> {
    let payload = serde_json::to_value(output).map_err(|error| {
        Error::caller("failed to render a tool result as provider-neutral JSON").with_source(error)
    })?;
    let output = ToolCallOutput::new(call_id.clone(), payload);
    Ok(ToolDispatch::Observed(match failure_code {
        None => ToolObservation::succeeded(output),
        Some(code) => ToolObservation::failed(output, code),
    }))
}

/// Builds the model-visible form of a failure: a stable code and the tool it came from.
///
/// No prose. See the module documentation — framework error text is written for logs and for the
/// user, not for the model, and a downstream reader must branch on the code rather than on wording.
pub(crate) fn observed_failure(call_id: &CallId, name: &str, error: &Error) -> ToolDispatch {
    ToolDispatch::Observed(ToolObservation::failed(
        failure_output(call_id, name, error),
        error.code(),
    ))
}

/// Answers a call the chain declined to run, in the shape a failing tool would have used.
///
/// One error format reaches the model, not two. What differs is the record left behind: nothing
/// ran, so there is nothing to say about how the tool behaves.
pub(crate) fn refused(call_id: &CallId, name: &str, error: &Error) -> ToolDispatch {
    ToolDispatch::Refused(ToolRefusal::new(
        failure_output(call_id, name, error),
        error.code(),
    ))
}

pub(crate) fn failure_output(call_id: &CallId, name: &str, error: &Error) -> ToolCallOutput {
    ToolCallOutput::new(
        call_id.clone(),
        json!({ "error": { "code": error.code(), "tool": name } }),
    )
    .with_error(true)
}
