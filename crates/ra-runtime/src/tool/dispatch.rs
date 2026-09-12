//! One tool invocation, in the only permitted stage order (R3-4).
//!
//! The chain is guardrail resolution -> typed argument decoding -> repeat admission -> permission
//! -> approval -> input guardrail -> invoke -> output guardrail. Every stage that can refuse does
//! so by **returning a value**, never by throwing prose a caller has to read. That is what keeps
//! the turn's control flow out of error strings.
//!
//! Resolution is first and separate from the two stages that use it, because it answers a
//! configuration question rather than one about this call: whether the checks the tool names exist
//! at all. A declaration nobody installed stops the call before a host has been interrupted about
//! it, and before a decoding failure can reach a handler whose text the output checks are owed.
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
//! The [lifecycle](ra_core::lifecycle) narration sits one step further in still, immediately around
//! the invocation and below the admission gate, and it is not a stage: it decides nothing, so it
//! carries no number in the order above and cannot refuse anything. What it brackets is
//! execution — which is why every call it announced is announced again when it settles, whichever
//! way that went.
//!
//! Concurrency ceilings and batching are deliberately absent: R3-4b owns the batch shape and R3-4c
//! owns what happens when several of these run at once.

use std::{sync::Arc, time::Instant};

use ra_core::hook::{HookDecision, HookEvent, ToolHookData};
use ra_core::{
    cancel::CancelScope,
    context::RunContext,
    error::{Error, Result, ToolErrorKind},
    guardrail::{
        ToolGuardrailVerdict, ToolInputGuardrail, ToolInputGuardrailData, ToolInputGuardrailResult,
        ToolOutputGuardrail, ToolOutputGuardrailData, ToolOutputGuardrailResult,
    },
    item::{CallId, ToolApproval, ToolCallOutput},
    lifecycle::{ToolEndInput, ToolStartInput},
    permission::PermissionDecision,
    tool::{
        Tool, ToolApprovalPolicy, ToolCaller, ToolConcurrency, ToolContext, ToolFailureHandling,
        ToolOptions, ToolOutput, ToolServices, ToolTimeoutBehavior,
    },
};
use serde_json::{Value, json};

use crate::circuit;
use crate::hook::UserHooks;
use crate::lifecycle::{LifecycleHooks, dispatch as lifecycle_dispatch};
use crate::permission::PermissionEngine;
use crate::tool::guardrail::{ToolGuardrails, check_tool_input, check_tool_output};

/// Stable code the model-visible answer to a refused call is recorded under.
///
/// The two match [`Error::code`] for the same guardrail stage. A refusal is not carried as an
/// `Error` — the run continues — but a report that groups by code has to see one vocabulary,
/// not two spellings of the same boundary.
const TOOL_INPUT_GUARDRAIL_CODE: &str = "guardrail.tool_input";
const TOOL_OUTPUT_GUARDRAIL_CODE: &str = "guardrail.tool_output";

/// The same, for the two boundaries a host hook can refuse a call at.
///
/// Separate from the guardrail codes and from the permission chain's `tool.permission_denied`,
/// because those are the three things a reader of a refused call has to be able to tell apart:
/// a check the tool declared, an optional host callback, and the run's policy.
const PRE_TOOL_USE_HOOK_CODE: &str = "hook.pre_tool_use";
const PERMISSION_REQUEST_HOOK_CODE: &str = "hook.permission_request";

/// What one call's dispatch produced: the decision, and what the checks around it concluded.
///
/// The two travel together because the second is not derivable from the first. A check that
/// allowed leaves no trace in the decision at all, and "this check ran and found nothing" is
/// exactly what separates a checked call from an unchecked one afterwards.
#[must_use]
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub struct ToolDispatchOutcome {
    dispatch: ToolDispatch,
    guardrails: ToolGuardrailRecords,
}

impl ToolDispatchOutcome {
    /// What the chain decided.
    #[must_use]
    pub const fn dispatch(&self) -> &ToolDispatch {
        &self.dispatch
    }

    /// What the checks declared on this tool concluded about this call.
    pub const fn guardrails(&self) -> &ToolGuardrailRecords {
        &self.guardrails
    }

    /// Takes the decision and the records apart.
    pub fn into_parts(self) -> (ToolDispatch, ToolGuardrailRecords) {
        (self.dispatch, self.guardrails)
    }
}

/// Every tool guardrail decision reached about one call, by boundary.
///
/// Held apart rather than in one list with a stage tag, for the reason
/// [`ToolInputGuardrailResult`] and [`ToolOutputGuardrailResult`] are separate types: they are
/// produced at opposite ends of a call and recorded in different places, and one list would make
/// filing an output decision where the input ones go a runtime mistake rather than a compile error.
#[must_use]
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ToolGuardrailRecords {
    input: Vec<ToolInputGuardrailResult>,
    output: Vec<ToolOutputGuardrailResult>,
}

impl ToolGuardrailRecords {
    /// What the checks on this call's arguments decided, in declaration order.
    #[must_use]
    pub fn input(&self) -> &[ToolInputGuardrailResult] {
        &self.input
    }

    /// What the checks on this call's result decided, in declaration order.
    #[must_use]
    pub fn output(&self) -> &[ToolOutputGuardrailResult] {
        &self.output
    }

    /// Whether no declared check reached a decision about this call.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.input.is_empty() && self.output.is_empty()
    }

    /// Takes both lists out.
    #[must_use]
    pub fn into_parts(
        self,
    ) -> (
        Vec<ToolInputGuardrailResult>,
        Vec<ToolOutputGuardrailResult>,
    ) {
        (self.input, self.output)
    }
}

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
    permission: PermissionEngine,
    guardrails: ToolGuardrails,
    user_hooks: UserHooks,
    lifecycle: LifecycleHooks,
    approval_granted: bool,
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
        permission: PermissionEngine,
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
            permission,
            guardrails: ToolGuardrails::default(),
            user_hooks: UserHooks::default(),
            lifecycle: LifecycleHooks::new(),
            approval_granted: false,
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

    /// Installs the host hooks consulted by this execution path.
    ///
    /// A request that is not given them runs no callback at all; hooks are optional and no event
    /// is mandatory, which is the difference from the guardrail set below.
    pub fn with_user_hooks(mut self, hooks: UserHooks) -> Self {
        self.user_hooks = hooks;
        self
    }

    /// Sets the tool guardrails this run installed, which the tool's declarations resolve against.
    ///
    /// A request that is not given them can still dispatch a tool that declares none. A tool that
    /// declares one is refused, which is the same answer an installed set that is missing it gives:
    /// running the call would execute it unchecked under a name that says it was checked.
    pub fn with_tool_guardrails(mut self, guardrails: ToolGuardrails) -> Self {
        self.guardrails = guardrails;
        self
    }

    /// Sets the lifecycle narration this run and its running agent installed.
    ///
    /// Default-empty for the same reason the hooks above are: narration is installed by the host on
    /// the run or on the agent, and a tool declares nothing that would claim otherwise.
    pub fn with_lifecycle_hooks(mut self, lifecycle: LifecycleHooks) -> Self {
        self.lifecycle = lifecycle;
        self
    }

    /// Marks a call as already approved by the host after an interruption.
    #[doc(hidden)]
    pub const fn with_approval_granted(mut self) -> Self {
        self.approval_granted = true;
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

    /// The view of this call its input guardrails are given.
    ///
    /// Built from the same fields [`Self::context`] is: a check deciding over a different view of
    /// the call than the tool receives would be deciding about something adjacent to what happens.
    fn guardrail_input_data(&self) -> ToolInputGuardrailData<'_> {
        ToolInputGuardrailData::new(
            &self.run,
            self.tool.origin(),
            &self.call_id,
            &self.arguments,
        )
        .with_services(&self.services)
    }
}

/// Runs one call through the fixed chain.
pub async fn dispatch_tool(request: ToolDispatchRequest) -> Result<ToolDispatchOutcome> {
    dispatch_tool_with_admission(request, None, Instant::now()).await
}

/// Runs one call through the fixed chain, optionally coordinating with a batch resource admission gate.
///
/// The guardrail records are collected as the chain runs and returned beside its decision. A chain
/// that ends in [`Err`] loses them, with one exception that matters: a
/// [`RaiseException`](ra_core::guardrail::ToolGuardrailBehavior::RaiseException) carries every
/// decision it reached on the error itself, because the run it ends produces no result to read them
/// from.
pub(crate) async fn dispatch_tool_with_admission(
    request: ToolDispatchRequest,
    admission: Option<&crate::turn::batch::ResourceAdmissionGate>,
    admission_started: Instant,
) -> Result<ToolDispatchOutcome> {
    let mut guardrails = ToolGuardrailRecords::default();
    let dispatch = run_chain(&request, admission, admission_started, &mut guardrails).await?;
    Ok(ToolDispatchOutcome {
        dispatch,
        guardrails,
    })
}

/// The nine stages, in the only permitted order.
///
/// Long on purpose. The order is the contract this module documents, and every stage that can
/// refuse reads the two above it — splitting the chain into three functions would put that order
/// somewhere no single function states it, which is the drift having one insertion point per
/// milestone exists to prevent.
#[allow(clippy::too_many_lines)]
async fn run_chain(
    request: &ToolDispatchRequest,
    admission: Option<&crate::turn::batch::ResourceAdmissionGate>,
    admission_started: Instant,
    records: &mut ToolGuardrailRecords,
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

    // Both boundaries are resolved before anything else looks at the call, and ahead of the
    // permission stage in particular: a declaration that names nothing installed stops this call
    // before a host is interrupted about it, and before a decoding failure can reach a handler
    // whose text the output checks are owed a look at. A call whose result cannot be checked should
    // not have produced the result either, which is why the second list is resolved here and not
    // after the tool has run.
    let input_guardrails = request.guardrails.resolve_input(&options, tool.origin())?;
    let output_guardrails = request.guardrails.resolve_output(&options, tool.origin())?;

    // Typed function tools are decoded exactly once at the common invocation boundary. A tool
    // implementation receives the checked value from its context instead of each implementation
    // independently reinterpreting provider JSON. Untyped and remote tools retain the parsed
    // JSON-only contract.
    let decoded_input = match tool.decode_input(&request.arguments) {
        Ok(decoded) => decoded,
        Err(error) => {
            return shape_failure(
                tool,
                request,
                &options,
                &name,
                error,
                &output_guardrails,
                records,
            )
            .await;
        }
    };

    // 2. Loop-breaker admission, before approval so a host is not asked about a call that will not
    // run. Both breakers live behind one insertion point rather than being bolted onto whichever
    // call site notices the repetition first, and they refuse the way this stage chain refuses:
    // with an observation the model can react to.
    if let Some(reason) = circuit::admit(&options, request.history, &name) {
        return Ok(refused(&request.call_id, &name, &reason));
    }

    // 3. Permission policy and approval, before anything runs.
    if let Some(answer) =
        admit_permission(request, &options, &name, &input_guardrails, records).await?
    {
        return Ok(answer);
    }

    // 4. Input guardrail, on the arguments the tool is about to be handed. A refusal here means the
    // tool runs zero times, which is the whole reason this boundary exists rather than only the one
    // after the call.
    if let Some(refusal) = check_input_guardrails(request, &input_guardrails, records).await? {
        return Ok(refusal);
    }

    // 5. Dynamic resource claims evaluation.
    // Exclusive tools run alone under a global write gate and skip fine-grained claims.
    let claims = if matches!(options.concurrency(), ToolConcurrency::Parallel) {
        let claims_result = request
            .cancel
            .run(tool.resource_claims(&request.context()))
            .await;
        match claims_result {
            Ok(Ok(claims)) => claims,
            Ok(Err(error)) => {
                return shape_failure(
                    tool,
                    request,
                    &options,
                    &name,
                    error,
                    &output_guardrails,
                    records,
                )
                .await;
            }
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

    // Lifecycle narration, immediately around the invocation and nowhere wider. It is not a stage:
    // it decides nothing, and it is unnumbered so the numbering keeps meaning "a point where this
    // call can still be refused". Every stage above decides whether the call happens at all, so
    // announcing a tool start ahead of them would announce tools the chain then refuses to run, and
    // a host counting invocations would be counting decisions. It sits below the admission gate
    // too, so the pair brackets execution rather than execution plus queueing — the waiting already
    // has its own field.
    lifecycle_dispatch::tool_start(
        &request.lifecycle,
        &lifecycle_call(request),
        &request.cancel,
    )
    .await?;

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

    announce_invocation_end(request, &outcome).await?;

    let output = match outcome {
        Ok(output) => output,
        Err(error) => {
            return shape_failure(
                tool,
                request,
                &options,
                &name,
                error,
                &output_guardrails,
                records,
            )
            .await;
        }
    };

    run_user_hook(
        request,
        HookEvent::PostToolUse {
            call: tool_hook_data(request),
            output: &output,
        },
    )
    .await?;

    // 8. Output guardrail, on the complete result the tool actually produced.
    //
    // The tool has run and whatever it did stands; what a refusal here replaces is the answer the
    // model reads. That answer *is* the record in this framework — history is what the next request
    // is built from — so the refused content does not travel on beside the message that replaced it.
    if let Some(replacement) =
        check_output_guardrails(request, &output_guardrails, &output, records).await?
    {
        return Ok(replacement);
    }

    // 9. Context projection happens after the guardrail has inspected the complete observation
    // and immediately before the record is made.  The stored ToolOutput retains those complete
    // blocks; only its model-facing excerpt changes.
    let output = project_output(request, output)?;
    observed_success(&request.call_id, &output)
}

fn tool_hook_data(request: &ToolDispatchRequest) -> ToolHookData<'_> {
    ToolHookData::new(request.tool.origin(), &request.call_id, &request.arguments)
}

/// The view of one call the lifecycle narration is built from.
///
/// One constructor, for the reason [`tool_hook_data`] is one: what a callback is told before the
/// invocation and what it is told after it must describe the same call.
///
/// It is a separate type from [`ToolHookData`] even though the fields coincide today, because the
/// two families are told different things about a call as they diverge: a deciding hook is handed
/// everything it might refuse the call over, while narration is handed what happened. A shared view
/// would make the next field added for one of them appear in the other.
fn lifecycle_call(request: &ToolDispatchRequest) -> ToolStartInput<'_> {
    ToolStartInput::new(
        &request.run,
        request.tool.origin(),
        &request.call_id,
        &request.arguments,
    )
    .with_services(&request.services)
}

/// Tells the lifecycle narration how the invocation it announced settled.
///
/// Runs before any output transformation, including asynchronous custom failure handling and the
/// output guardrails, so an observer measures invocation rather than settlement.
async fn announce_invocation_end(
    request: &ToolDispatchRequest,
    outcome: &Result<ToolOutput>,
) -> Result<()> {
    if request.lifecycle.is_empty() {
        return Ok(());
    }
    let settled = ToolEndInput::new(lifecycle_call(request), outcome);
    lifecycle_dispatch::tool_end(&request.lifecycle, &settled, &request.cancel).await
}

async fn run_user_hook(
    request: &ToolDispatchRequest,
    event: HookEvent<'_>,
) -> Result<HookDecision> {
    if !request.user_hooks.has_event(event.name()) {
        return Ok(HookDecision::Continue);
    }
    request
        .user_hooks
        .bind(
            Arc::clone(&request.run),
            request.cancel.clone(),
            request.services.clone(),
        )
        .dispatch(event)
        .await
}

/// Runs the permission stage, returning the answer the chain gives instead of running the tool.
///
/// `None` means the call may proceed. A fixed rule or mode decision is resolved without entering
/// third-party code; only the remaining default path asks a dynamic tool whether it needs approval.
async fn admit_permission(
    request: &ToolDispatchRequest,
    options: &ToolOptions,
    name: &str,
    input_guardrails: &[Arc<dyn ToolInputGuardrail>],
    records: &mut ToolGuardrailRecords,
) -> Result<Option<ToolDispatch>> {
    let tool = &request.tool;
    let permission = resolve_permission(tool, options, request).await?;
    // A policy denial is final; no hook grant is consulted on that path.
    if !matches!(permission, PermissionDecision::Deny)
        && let HookDecision::Deny { message } =
            run_user_hook(request, HookEvent::PreToolUse(tool_hook_data(request))).await?
    {
        return Ok(Some(ToolDispatch::Refused(ToolRefusal::new(
            guardrail_message(&request.call_id, message)?,
            PRE_TOOL_USE_HOOK_CODE,
        ))));
    }
    match permission {
        PermissionDecision::Allow => Ok(None),
        PermissionDecision::Deny => Ok(Some(refused(
            &request.call_id,
            name,
            &Error::tool(
                ToolErrorKind::PermissionDenied,
                name,
                "the permission policy denied this tool call",
            ),
        ))),
        PermissionDecision::Ask => {
            // The optional pre-approval pass. It exists so nobody is asked to approve a call that
            // is going to be refused anyway — and it does not settle the question: the check after
            // the answer comes back still runs, against whatever the host has installed by then.
            // See [`ToolInputGuardrail`] for why that one is not a formality.
            if request.guardrails.checks_before_approval()
                && let Some(refusal) =
                    check_input_guardrails(request, input_guardrails, records).await?
            {
                return Ok(Some(refusal));
            }
            match run_user_hook(
                request,
                HookEvent::PermissionRequest(tool_hook_data(request)),
            )
            .await?
            {
                HookDecision::Allow => return Ok(None),
                HookDecision::Deny { message } => {
                    return Ok(Some(ToolDispatch::Refused(ToolRefusal::new(
                        guardrail_message(&request.call_id, message)?,
                        PERMISSION_REQUEST_HOOK_CODE,
                    ))));
                }
                _ => {}
            }
            let mut approval = ToolApproval::new(
                request.call_id.clone(),
                tool.model_definition().name(),
                request.arguments.clone(),
            );
            if let Some(namespace) = tool.origin().namespace() {
                approval = approval.with_namespace(namespace.as_str());
            }
            approval = approval.with_tool_origin(tool.origin());
            Ok(Some(ToolDispatch::AwaitingApproval(approval)))
        }
        _ => Err(Error::caller(format!(
            "tool `{name}` has an unsupported permission decision `{permission:?}`"
        ))),
    }
}

/// Applies fixed rule and mode decisions before consulting a tool's dynamic approval callback.
///
/// # What a host answer already covers
///
/// A call resumed after an approval carries the host's answer, and that answer settles exactly one
/// of the three decisions: `Ask` is a question the host has now been asked and has answered, so it
/// collapses to `Allow`. `Deny` does not — a deny rule, plan mode, and `DontAsk` are policy that
/// was never up for a click, and resolving the interruption is not a licence to overrule them.
/// The order matters as much as the substance: consulting the fixed decision *before* honouring
/// the answer is what keeps the denials reachable at all.
async fn resolve_permission(
    tool: &Arc<dyn Tool>,
    options: &ToolOptions,
    request: &ToolDispatchRequest,
) -> Result<PermissionDecision> {
    let origin = tool.origin();
    if let Some(decision) = request
        .permission
        .fixed_decision(options.permission_scope(), origin)
    {
        if request.approval_granted && matches!(decision, PermissionDecision::Ask) {
            return Ok(PermissionDecision::Allow);
        }
        return Ok(decision);
    }
    if request.approval_granted {
        return Ok(PermissionDecision::Allow);
    }

    let fallback = if needs_approval(tool, options, request).await? {
        PermissionDecision::Ask
    } else {
        PermissionDecision::Allow
    };
    Ok(request
        .permission
        .evaluate(options.permission_scope(), origin, fallback))
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
///
/// The output guardrails are carried in because one branch below produces tool-authored text the
/// model will read — [`ToolFailureHandling::Custom`] — and that is a tool result like any other. The
/// branches that render a stable code are not offered to them: a code and a qualified name are the
/// framework's own vocabulary, with nothing a check written about tool content could inspect.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn shape_failure(
    tool: &Arc<dyn Tool>,
    request: &ToolDispatchRequest,
    options: &ToolOptions,
    name: &str,
    error: Error,
    output_guardrails: &[Arc<dyn ToolOutputGuardrail>],
    records: &mut ToolGuardrailRecords,
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
                Some(output) => {
                    if let Some(replacement) =
                        check_output_guardrails(request, output_guardrails, &output, records)
                            .await?
                    {
                        return Ok(replacement);
                    }
                    let output = project_output(request, output)?;
                    observed(&request.call_id, &output, Some(error.code()))
                }
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

/// Applies the host's context policy without giving the runtime a dependency on its implementation.
///
/// An absent projector preserves the historical behavior: every tool result is sent in full. A
/// configured projector can add an excerpt and projection metadata, while the runtime applies it
/// to the original complete [`ToolOutput`] before serializing the authoritative session record.
fn project_output(request: &ToolDispatchRequest, output: ToolOutput) -> Result<ToolOutput> {
    match request.services.output_projector() {
        Some(projector) => {
            let projection = projector.project(request.run.run_id(), &request.call_id, &output)?;
            Ok(output.with_model_projection(projection))
        }
        None => Ok(output),
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

/// Asks the checks declared on this tool about the call's arguments.
///
/// `Ok(None)` is the ordinary answer: nothing declared, or every check allowed. `Ok(Some(_))` is a
/// refusal the model reads in place of the call it asked for, and `Err` is a raise — or a check
/// that could not decide, which is not a refusal and must not be reported as one.
async fn check_input_guardrails(
    request: &ToolDispatchRequest,
    guardrails: &[Arc<dyn ToolInputGuardrail>],
    records: &mut ToolGuardrailRecords,
) -> Result<Option<ToolDispatch>> {
    if guardrails.is_empty() {
        return Ok(None);
    }
    let data = request.guardrail_input_data();
    let verdict = check_tool_input(guardrails, &data, &request.cancel, &mut records.input).await?;
    match verdict {
        ToolGuardrailVerdict::Allow => Ok(None),
        ToolGuardrailVerdict::RejectContent { message, .. } => {
            // `Refused`, not a failed observation: the tool did not run, and a record that said it
            // had would hand the no-progress breaker evidence about a tool that never executed.
            Ok(Some(ToolDispatch::Refused(ToolRefusal::new(
                guardrail_message(&request.call_id, message)?,
                TOOL_INPUT_GUARDRAIL_CODE,
            ))))
        }
        verdict => Err(unsupported_verdict(&verdict)),
    }
}

/// Asks the checks declared on this tool about what the call produced.
///
/// `Ok(Some(_))` carries the message the model reads instead of the result.
async fn check_output_guardrails(
    request: &ToolDispatchRequest,
    guardrails: &[Arc<dyn ToolOutputGuardrail>],
    output: &ToolOutput,
    records: &mut ToolGuardrailRecords,
) -> Result<Option<ToolDispatch>> {
    if guardrails.is_empty() {
        return Ok(None);
    }
    let data = ToolOutputGuardrailData::new(request.guardrail_input_data(), output);
    let verdict =
        check_tool_output(guardrails, &data, &request.cancel, &mut records.output).await?;
    match verdict {
        ToolGuardrailVerdict::Allow => Ok(None),
        ToolGuardrailVerdict::RejectContent { message, .. } => {
            // `Observed`, because the tool did run: its side effects stand, and the records owe the
            // streak counters the truth about that. Classified as a failure so no stop policy can
            // promote a refused result to the run's answer.
            Ok(Some(ToolDispatch::Observed(ToolObservation::failed(
                guardrail_message(&request.call_id, message)?,
                TOOL_OUTPUT_GUARDRAIL_CODE,
            ))))
        }
        verdict => Err(unsupported_verdict(&verdict)),
    }
}

/// Renders a guardrail's own message as the answer the model reads.
///
/// Host-authored text written for the model, which is why it reaches the model at all — the ban in
/// this module's documentation is on *framework* prose, written for logs and for the person at the
/// keyboard. It is flagged as an error so that a refusal can never be promoted to the run's answer
/// by a tool-use stop policy, which cannot inspect what it stops on.
fn guardrail_message(call_id: &CallId, message: String) -> Result<ToolCallOutput> {
    let payload = serde_json::to_value(ToolOutput::text(message)).map_err(|error| {
        Error::caller("failed to render a tool guardrail's message as provider-neutral JSON")
            .with_source(error)
    })?;
    Ok(ToolCallOutput::new(call_id.clone(), payload).with_error(true))
}

fn unsupported_verdict(verdict: &ToolGuardrailVerdict) -> Error {
    Error::caller(format!(
        "a tool guardrail reduced to the unsupported verdict `{verdict:?}`"
    ))
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
