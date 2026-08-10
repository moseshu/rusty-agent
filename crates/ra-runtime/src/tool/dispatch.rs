//! One tool invocation, in the only permitted stage order (R3-4).
//!
//! The chain is repeat admission -> approval -> input guardrail -> invoke -> output guardrail, and
//! every stage that can refuse does so by **returning a value**, never by throwing prose a caller
//! has to read. That is what keeps the turn's control flow out of error strings.
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

use std::sync::Arc;

use ra_core::{
    cancel::CancelScope,
    error::{Error, Result, ToolErrorKind},
    item::{CallId, ToolApproval, ToolCallOutput},
    tool::{
        Tool, ToolApprovalPolicy, ToolCaller, ToolFailureHandling, ToolInvocation, ToolOptions,
        ToolOutput, ToolRuntimeContext, ToolTimeoutBehavior,
    },
};
use serde_json::{Value, json};

/// What the chain decided about one call.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub enum ToolDispatch {
    /// The call reached a model-visible result. A failure the model is meant to react to is also
    /// this variant, carrying `is_error`; only failures that must stop the turn leave as [`Err`].
    Observed(ToolCallOutput),
    /// A host has to decide before the tool may run.
    AwaitingApproval(ToolApproval),
}

/// Inputs for one invocation.
#[must_use]
#[non_exhaustive]
pub struct ToolDispatchRequest<'a> {
    tool: &'a Arc<dyn Tool>,
    call_id: &'a CallId,
    arguments: &'a Value,
    context: &'a dyn ToolRuntimeContext,
    cancel: &'a CancelScope,
    repeat_streak: u32,
    caller: ToolCaller,
}

impl<'a> ToolDispatchRequest<'a> {
    /// Creates a request for a call the model made directly.
    ///
    /// `cancel` is required rather than optional for the same reason it is in turn preparation:
    /// [`Tool::call`] is third-party `async` code, and the cancellation contract does not allow it
    /// to be awaited bare.
    pub fn new(
        tool: &'a Arc<dyn Tool>,
        call_id: &'a CallId,
        arguments: &'a Value,
        context: &'a dyn ToolRuntimeContext,
        cancel: &'a CancelScope,
        repeat_streak: u32,
    ) -> Self {
        Self {
            tool,
            call_id,
            arguments,
            context,
            cancel,
            repeat_streak,
            caller: ToolCaller::Direct,
        }
    }

    /// Sets the caller class this invocation is admitted under.
    pub const fn with_caller(mut self, caller: ToolCaller) -> Self {
        self.caller = caller;
        self
    }

    fn invocation(&self) -> ToolInvocation<'a> {
        ToolInvocation::new(self.call_id, self.arguments)
            .with_caller(self.caller)
            .with_context(self.context)
    }
}

/// Runs one call through the fixed chain.
pub async fn dispatch_tool(request: ToolDispatchRequest<'_>) -> Result<ToolDispatch> {
    let tool = request.tool;
    let options = tool.options();
    let name = tool.origin().qualified_name().to_owned();

    // 1. Caller admission. A tool that does not admit this caller class is, from that caller's
    // side, indistinguishable from one that is not there — so it reports as `not_found` rather
    // than inventing a second way to say "you may not call this".
    if !options.allows_caller(request.caller) {
        return Ok(observed_failure(
            request.call_id,
            &name,
            &Error::tool(
                ToolErrorKind::NotFound,
                &name,
                "the tool does not admit this caller class",
            ),
        ));
    }

    // 2. Repeat admission. R3-6 puts the semantic loop breaker here — one insertion point rather
    // than a check bolted onto whichever call site notices the repetition first.
    admit_repeat(&options, request.repeat_streak)?;

    // 3. Approval, before anything runs. Static policies are answered from the declaration and
    // never enter third-party code, exactly as dynamic availability is handled in preparation.
    if needs_approval(tool, &options, &request).await? {
        let mut approval = ToolApproval::new(
            request.call_id.clone(),
            tool.origin().name(),
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

    let outcome = invoke(tool, request.invocation(), &options, request.cancel, &name).await;

    let output = match outcome {
        Ok(output) => output,
        Err(error) => return shape_failure(tool, &request, &options, &name, error).await,
    };

    // 5. Output guardrail, on the result the tool actually produced.
    check_output_guardrails(&options)?;

    observed_success(request.call_id, &output)
}

/// Invokes the tool, applying its per-call time limit.
async fn invoke(
    tool: &Arc<dyn Tool>,
    invocation: ToolInvocation<'_>,
    options: &ToolOptions,
    cancel: &CancelScope,
    name: &str,
) -> Result<ToolOutput> {
    let call = tool.call(invocation);
    match options.timeout() {
        None => cancel.run(call).await?,
        Some(limit) => match cancel.run(tokio::time::timeout(limit, call)).await? {
            Ok(result) => result,
            Err(_elapsed) => Err(Error::tool(
                ToolErrorKind::Timeout,
                name,
                format!("the tool exceeded its {limit:?} limit"),
            )),
        },
    }
}

/// Turns an invocation failure into either a model-visible observation or a stopped turn.
async fn shape_failure(
    tool: &Arc<dyn Tool>,
    request: &ToolDispatchRequest<'_>,
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
                Ok(observed_failure(request.call_id, name, &error))
            }
            ToolTimeoutBehavior::Propagate => Err(error),
            behavior => Err(Error::caller(format!(
                "tool `{name}` uses an unsupported timeout behavior `{behavior:?}`"
            ))),
        };
    }

    match options.failure_handling() {
        ToolFailureHandling::ModelVisible => Ok(observed_failure(request.call_id, name, &error)),
        ToolFailureHandling::Propagate => Err(error),
        // The one path where a tool writes its own model-facing failure text. Returning `None`
        // means the tool declined to handle it, and an unhandled failure propagates.
        ToolFailureHandling::Custom => {
            let invocation = request.invocation();
            match request
                .cancel
                .run(tool.handle_failure(&invocation, &error))
                .await??
            {
                Some(output) => observed_success(request.call_id, &output),
                None => Err(error),
            }
        }
        handling => Err(Error::caller(format!(
            "tool `{name}` uses an unsupported failure handling policy `{handling:?}`"
        ))),
    }
}

async fn needs_approval(
    tool: &Arc<dyn Tool>,
    options: &ToolOptions,
    request: &ToolDispatchRequest<'_>,
) -> Result<bool> {
    match options.approval() {
        ToolApprovalPolicy::Never => Ok(false),
        ToolApprovalPolicy::Always => Ok(true),
        ToolApprovalPolicy::Dynamic => {
            let invocation = request.invocation();
            request.cancel.run(tool.needs_approval(&invocation)).await?
        }
        policy => Err(Error::caller(format!(
            "tool `{}` uses an unsupported approval policy `{policy:?}`",
            tool.origin().qualified_name()
        ))),
    }
}

// The three stages below always succeed today and will not once their owning milestones land.
// Narrowing the return type now would take the `?` off the call sites, and a stage that reads like
// a no-op is a stage someone tidies away — which is exactly what having one insertion point per
// milestone is meant to prevent.

/// R3-6's insertion point for the semantic loop breaker.
///
/// The evidence it needs already exists: settlement records this turn's attempts into
/// [`ToolUseTracker`](ra_core::state::ToolUseTracker) *before* execution reaches here, then the
/// batch projects the current per-agent, per-tool `repeat_streak` into this request. What is
/// missing is the threshold and the refusal, both of which are R3-6's to define.
///
/// One property to decide against rather than discover: because the turn is recorded before any of
/// it runs, `repeat_streak` already includes every call in this response. `N` identical parallel
/// calls all arrive here reading `N`, so a threshold of `N` refuses the first one too. See
/// [`ToolUseTracker::repeat_streak`](ra_core::state::ToolUseTracker::repeat_streak).
#[allow(clippy::unnecessary_wraps)]
const fn admit_repeat(_options: &ToolOptions, _repeat_streak: u32) -> Result<()> {
    Ok(())
}

/// R7-3's insertion point for tool input guardrails.
#[allow(clippy::unnecessary_wraps)]
const fn check_input_guardrails(_options: &ToolOptions) -> Result<()> {
    Ok(())
}

/// R7-3's insertion point for tool output guardrails.
#[allow(clippy::unnecessary_wraps)]
const fn check_output_guardrails(_options: &ToolOptions) -> Result<()> {
    Ok(())
}

fn observed_success(call_id: &CallId, output: &ToolOutput) -> Result<ToolDispatch> {
    let payload = serde_json::to_value(output).map_err(|error| {
        Error::caller("failed to render a tool result as provider-neutral JSON").with_source(error)
    })?;
    Ok(ToolDispatch::Observed(ToolCallOutput::new(
        call_id.clone(),
        payload,
    )))
}

/// Builds the model-visible form of a failure: a stable code and the tool it came from.
///
/// No prose. See the module documentation — framework error text is written for logs and for the
/// user, not for the model, and a downstream reader must branch on the code rather than on wording.
fn observed_failure(call_id: &CallId, name: &str, error: &Error) -> ToolDispatch {
    ToolDispatch::Observed(
        ToolCallOutput::new(
            call_id.clone(),
            json!({ "error": { "code": error.code(), "tool": name } }),
        )
        .with_error(true),
    )
}
