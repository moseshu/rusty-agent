//! The two host checks that bracket one tool call: its arguments before it runs, and its result
//! before the model is shown it.
//!
//! # The same family as the run's own guardrails, at a different boundary
//!
//! [`InputGuardrail`](super::InputGuardrail) brackets a run; these bracket a call. Both are checks
//! the *host* wrote and attached, and this framework installs none of its own at either boundary.
//! What makes the tool boundary worth its own contract is where it sits: a run's input guardrail races
//! the first model call and may not return until that turn's tools have already run, so a check
//! that must be decided *before* a side effect happens has to be asked here or not at all.
//!
//! # Three behaviors, and no fourth
//!
//! [`ToolGuardrailBehavior`] is the reference implementation's `tool_guardrails.py` set, matched
//! state for state: let it through, refuse this one call and answer the model in its place, or end
//! the run. A host that wants to say something to the model about a call it is *allowing* has a
//! better place to say it — the tool's own result.
//!
//! # Why a tripwire boolean is not enough here, and evidence still is
//!
//! A run-level guardrail has one control decision to make: stop, or do not. A tool guardrail has
//! two ways of not letting a call stand, and they differ in who recovers — after
//! [`RejectContent`](ToolGuardrailBehavior::RejectContent) the model chooses again with the message
//! in front of it, after [`RaiseException`](ToolGuardrailBehavior::RaiseException) nobody does. So
//! the behavior is named rather than derived from a flag.
//!
//! `output_info` is carried for the reason
//! [`GuardrailFunctionOutput`](super::GuardrailFunctionOutput) carries it: a check that allowed and
//! recorded nothing is indistinguishable afterwards from a check that was never installed.
//!
//! # Identity, not the implementation, is what a tool declares
//!
//! The reference implementation hangs the guardrail objects off the tool itself. Here
//! [`ToolOptions`](crate::tool::ToolOptions) is a persisted configuration value, and a callback is
//! a runtime resource that cannot enter one — so a tool declares
//! [`ToolGuardrailId`]s and the runtime resolves them against what the host installed. The deviation costs
//! one failure mode the original does not have, a declared identity nobody installed, and that is
//! resolved the only way it safely can be: a declaration that does not resolve fails, rather than
//! letting the call run unchecked under a name that promises it was checked.

use core::fmt;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    context::RunContext,
    error::{Error, GuardrailStage, Result},
    item::CallId,
    tool::{ToolGuardrailId, ToolOrigin, ToolOutput, ToolServices},
};

/// What a tool guardrail decided about the call it examined.
///
/// The enum is closed to the three states the reference implementation defines. A fourth would
/// have to say what the runtime does with it at both boundaries, and the two places that answer
/// that question — the dispatch chain and this type — would then be able to disagree.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolGuardrailBehavior {
    /// Nothing to object to. The call proceeds, or its result stands.
    Allow,
    /// This call does not stand, and this message takes its place. The run continues.
    ///
    /// At the input boundary the tool never runs. At the output boundary it already has, and what
    /// is replaced is only what the model is shown.
    RejectContent {
        /// What the model is shown instead. Host-authored text, written for the model.
        message: String,
    },
    /// The run ends.
    ///
    /// It carries no message on purpose: the reason belongs in `output_info`, and the error text is
    /// built where the boundary is known — see [`reduce_tool_input_guardrails`].
    RaiseException,
}

impl ToolGuardrailBehavior {
    /// Stable code word, for trace fields and reports.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::RejectContent { .. } => "reject_content",
            Self::RaiseException => "raise_exception",
        }
    }
}

impl fmt::Display for ToolGuardrailBehavior {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.code())
    }
}

/// What one tool guardrail concluded: what it decided, plus what it saw on the way there.
///
/// The two fields answer different questions, exactly as they do for a run-level guardrail.
/// [`Self::behavior`] is control flow. [`Self::output_info`] is evidence, and a guardrail that
/// allowed should still fill it in — "this check ran and found nothing" and "this check was never
/// installed" are the same absence without it.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolGuardrailFunctionOutput {
    #[serde(default, skip_serializing_if = "Value::is_null")]
    output_info: Value,
    behavior: ToolGuardrailBehavior,
}

impl ToolGuardrailFunctionOutput {
    /// The call proceeds, or its result stands.
    #[must_use]
    pub const fn allow() -> Self {
        Self {
            output_info: Value::Null,
            behavior: ToolGuardrailBehavior::Allow,
        }
    }

    /// The call does not stand; this message is answered to the model in its place.
    #[must_use]
    pub fn reject_content(message: impl Into<String>) -> Self {
        Self {
            output_info: Value::Null,
            behavior: ToolGuardrailBehavior::RejectContent {
                message: message.into(),
            },
        }
    }

    /// The run ends.
    #[must_use]
    pub const fn raise_exception() -> Self {
        Self {
            output_info: Value::Null,
            behavior: ToolGuardrailBehavior::RaiseException,
        }
    }

    /// Attaches the evidence behind the decision.
    #[must_use]
    pub fn with_output_info(mut self, output_info: Value) -> Self {
        self.output_info = output_info;
        self
    }

    /// What the guardrail decided.
    #[must_use]
    pub const fn behavior(&self) -> &ToolGuardrailBehavior {
        &self.behavior
    }

    /// Evidence the guardrail recorded, [`Value::Null`] when it offered none.
    #[must_use]
    pub const fn output_info(&self) -> &Value {
        &self.output_info
    }

    /// Whether the guardrail let the call stand unchanged.
    #[must_use]
    pub const fn is_allow(&self) -> bool {
        matches!(self.behavior, ToolGuardrailBehavior::Allow)
    }

    /// The evidence as one clause of a refusal message.
    fn detail(&self) -> String {
        if self.output_info.is_null() {
            "the guardrail recorded no further detail".to_owned()
        } else {
            format!("output_info: {}", self.output_info)
        }
    }
}

impl Default for ToolGuardrailFunctionOutput {
    /// Allowing. The decision reached by leaving a field unset has to be the one that changes
    /// nothing.
    fn default() -> Self {
        Self::allow()
    }
}

/// A completed tool input check, with its evidence and the call it examined.
///
/// The upstream result stores a live guardrail object. A checkpoint stores its identity instead;
/// origin and call ID preserve attribution when that identity checks multiple tools or calls.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolInputGuardrailResult {
    guardrail: ToolGuardrailId,
    origin: ToolOrigin,
    call_id: CallId,
    output: ToolGuardrailFunctionOutput,
}

impl ToolInputGuardrailResult {
    /// Records a completed check against one call.
    #[must_use]
    pub const fn new(
        guardrail: ToolGuardrailId,
        origin: ToolOrigin,
        call_id: CallId,
        output: ToolGuardrailFunctionOutput,
    ) -> Self {
        Self {
            guardrail,
            origin,
            call_id,
            output,
        }
    }

    /// Identity of the check that produced this result.
    #[must_use]
    pub const fn guardrail(&self) -> &ToolGuardrailId {
        &self.guardrail
    }

    /// Tool examined by this check.
    #[must_use]
    pub const fn origin(&self) -> &ToolOrigin {
        &self.origin
    }

    /// Call examined by this check.
    #[must_use]
    pub const fn call_id(&self) -> &CallId {
        &self.call_id
    }

    /// The complete decision and its evidence.
    #[must_use]
    pub const fn output(&self) -> &ToolGuardrailFunctionOutput {
        &self.output
    }
}

/// A completed tool output check, with its evidence and the call it examined.
///
/// The upstream result stores a live guardrail object. A checkpoint stores its identity instead;
/// origin and call ID preserve attribution when that identity checks multiple tools or calls.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolOutputGuardrailResult {
    guardrail: ToolGuardrailId,
    origin: ToolOrigin,
    call_id: CallId,
    output: ToolGuardrailFunctionOutput,
}

impl ToolOutputGuardrailResult {
    /// Records a completed check against one call.
    #[must_use]
    pub const fn new(
        guardrail: ToolGuardrailId,
        origin: ToolOrigin,
        call_id: CallId,
        output: ToolGuardrailFunctionOutput,
    ) -> Self {
        Self {
            guardrail,
            origin,
            call_id,
            output,
        }
    }

    /// Identity of the check that produced this result.
    #[must_use]
    pub const fn guardrail(&self) -> &ToolGuardrailId {
        &self.guardrail
    }

    /// Tool examined by this check.
    #[must_use]
    pub const fn origin(&self) -> &ToolOrigin {
        &self.origin
    }

    /// Call examined by this check.
    #[must_use]
    pub const fn call_id(&self) -> &CallId {
        &self.call_id
    }

    /// The complete decision and its evidence.
    #[must_use]
    pub const fn output(&self) -> &ToolGuardrailFunctionOutput {
        &self.output
    }
}

/// One resolved call, as the guardrails declared on its tool see it before it runs.
///
/// The shape follows [`ToolContext`](crate::tool::ToolContext): the same origin, call ID, arguments
/// and port bag the tool itself will be handed. A guardrail deciding over a different view of the
/// call than the tool receives would be checking something adjacent to what happens.
///
/// A borrowed view rather than the call record itself: a guardrail decides about one call, and
/// handing it something that also carries the run's accounting would invite it to answer a
/// different question.
#[must_use]
#[non_exhaustive]
#[derive(Debug, Clone, Copy)]
pub struct ToolInputGuardrailData<'a> {
    run: &'a RunContext,
    origin: &'a ToolOrigin,
    call_id: &'a CallId,
    arguments: &'a Value,
    services: &'a ToolServices,
}

impl<'a> ToolInputGuardrailData<'a> {
    /// Describes a call about to run, with no ports installed.
    pub fn new(
        run: &'a RunContext,
        origin: &'a ToolOrigin,
        call_id: &'a CallId,
        arguments: &'a Value,
    ) -> Self {
        Self {
            run,
            origin,
            call_id,
            arguments,
            services: ToolServices::none(),
        }
    }

    /// Installs the framework ports this call may reach.
    pub const fn with_services(mut self, services: &'a ToolServices) -> Self {
        self.services = services;
        self
    }

    /// The run this call is part of.
    #[must_use]
    pub const fn run(&self) -> &'a RunContext {
        self.run
    }

    /// Stable identity of the tool being called.
    #[must_use]
    pub const fn origin(&self) -> &'a ToolOrigin {
        self.origin
    }

    /// Provider call ID paired with the eventual result.
    #[must_use]
    pub const fn call_id(&self) -> &'a CallId {
        self.call_id
    }

    /// Parsed model arguments, exactly as the tool will see them.
    #[must_use]
    pub const fn arguments(&self) -> &'a Value {
        self.arguments
    }

    /// Framework ports available to this call.
    pub const fn services(&self) -> &'a ToolServices {
        self.services
    }
}

/// The same call, plus what it produced.
///
/// The output is required rather than optional. These guardrails are asked about a result and are
/// not asked at all when there is none — a call that failed before producing anything never reaches
/// this boundary. Making the field optional would hand every implementation an absence it has no
/// case for.
#[must_use]
#[non_exhaustive]
#[derive(Debug, Clone, Copy)]
pub struct ToolOutputGuardrailData<'a> {
    call: ToolInputGuardrailData<'a>,
    output: &'a ToolOutput,
}

impl<'a> ToolOutputGuardrailData<'a> {
    /// Describes a call that produced a result.
    pub const fn new(call: ToolInputGuardrailData<'a>, output: &'a ToolOutput) -> Self {
        Self { call, output }
    }

    /// The call this result answers, in the view its input guardrails were given.
    pub const fn call(&self) -> ToolInputGuardrailData<'a> {
        self.call
    }

    /// The run this call was part of.
    #[must_use]
    pub const fn run(&self) -> &'a RunContext {
        self.call.run()
    }

    /// Stable identity of the tool that was called.
    #[must_use]
    pub const fn origin(&self) -> &'a ToolOrigin {
        self.call.origin()
    }

    /// Provider call ID this result is paired with.
    #[must_use]
    pub const fn call_id(&self) -> &'a CallId {
        self.call.call_id()
    }

    /// The arguments the call ran with.
    #[must_use]
    pub const fn arguments(&self) -> &'a Value {
        self.call.arguments()
    }

    /// Framework ports the call reached.
    pub const fn services(&self) -> &'a ToolServices {
        self.call.services()
    }

    /// What the tool produced, complete, before any context projection narrows it.
    #[must_use]
    pub const fn output(&self) -> &'a ToolOutput {
        self.output
    }
}

/// A host check applied to one tool call's arguments before it runs.
///
/// # It is asked twice when a host has to approve the call
///
/// A call that needs approval may be checked once before the host is interrupted — so nobody is
/// asked to approve a call that is going to be refused anyway — and is checked again after the
/// approval comes back. The second check is not optional and not a formality: an approval can
/// return in a later process, against a registry the host has since changed, and a decision made
/// before the interruption is not a decision about the run that resumes. The first check is what a
/// run switches on with `RunConfig::with_pre_approval_tool_input_guardrails`; the second always
/// happens.
///
/// # Cancellation
///
/// This is third-party `async` code, so the runtime awaits it inside the call's cancellation scope
/// rather than bare. On cancellation the returned future is **dropped**, which is safe for a pure
/// future: an implementation that spawns a task or a child process owns draining it, exactly as a
/// [`Tool`](crate::tool::Tool) does.
#[async_trait]
pub trait ToolInputGuardrail: Send + Sync + 'static {
    /// Stable identity of this check, shared by the tools that declare it, its trace field, and its
    /// metric.
    fn id(&self) -> &ToolGuardrailId;

    /// Examines a call before it runs.
    ///
    /// Returning `Err` fails the turn rather than refusing the call: a guardrail that could not
    /// reach a decision has not decided anything, and reporting it as a refusal would blame the
    /// arguments for the checker being down.
    async fn check(&self, data: &ToolInputGuardrailData<'_>)
    -> Result<ToolGuardrailFunctionOutput>;
}

/// A host check applied to one tool call's result before the model is shown it.
///
/// Asked about tool results, including a custom failure handler's model-facing output. Framework
/// failures rendered only as stable codes have no tool-authored content to inspect.
///
/// # Cancellation
///
/// As for [`ToolInputGuardrail`]: awaited inside the call's cancellation scope, and dropped when it
/// is cancelled.
#[async_trait]
pub trait ToolOutputGuardrail: Send + Sync + 'static {
    /// Stable identity of this check, shared by the tools that declare it, its trace field, and its
    /// metric.
    fn id(&self) -> &ToolGuardrailId;

    /// Examines what a call produced.
    ///
    /// Returning `Err` fails the turn, for the reason [`ToolInputGuardrail::check`] gives.
    async fn check(
        &self,
        data: &ToolOutputGuardrailData<'_>,
    ) -> Result<ToolGuardrailFunctionOutput>;
}

/// What the guardrails declared on one tool concluded, taken together.
///
/// A raise is not here: it ends the run, so it travels as an [`Error`] instead — the same reason a
/// tripwire does. Control flow in this framework reads
/// [`Recoverability`](crate::error::Recoverability), and a refusal returned as an ordinary value
/// would have to be noticed by each caller in turn.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolGuardrailVerdict {
    /// Every guardrail let the call stand.
    Allow,
    /// One guardrail refused, and this message stands in place of what it refused.
    RejectContent {
        /// Which guardrail refused.
        guardrail: ToolGuardrailId,
        /// What the model is shown instead.
        message: String,
    },
}

impl ToolGuardrailVerdict {
    /// Whether the call stands unchanged.
    #[must_use]
    pub const fn is_allow(&self) -> bool {
        matches!(self, Self::Allow)
    }
}

/// Reduces what every input guardrail on one tool answered into the decision the chain acts on.
///
/// `firings` is `(identity, decision)` for each guardrail the tool declared, in declaration order.
///
/// # Ties
///
/// A raise beats a rejection, and among equals the first by declaration order wins while the rest
/// are dropped rather than concatenated. One call gets one answer: two refusal messages for one
/// call is two explanations the model has to reconcile, and neither is the one it needs.
///
/// # Why the stage is not a parameter
///
/// There are two functions rather than one taking a [`GuardrailStage`], so no caller is in a
/// position to name the wrong boundary. A raise at the input boundary and a raise at the output
/// boundary produce different [`Error::code`] values, and a mislabelled one sends every report and
/// every metric to the other end of the call.
pub fn reduce_tool_input_guardrails(
    firings: impl IntoIterator<Item = (ToolGuardrailId, ToolGuardrailFunctionOutput)>,
) -> Result<ToolGuardrailVerdict> {
    reduce(
        GuardrailStage::ToolInput,
        "the call's arguments were refused before the tool ran",
        firings,
    )
}

/// Reduces what every output guardrail on one tool answered, as
/// [`reduce_tool_input_guardrails`] does for the other boundary.
pub fn reduce_tool_output_guardrails(
    firings: impl IntoIterator<Item = (ToolGuardrailId, ToolGuardrailFunctionOutput)>,
) -> Result<ToolGuardrailVerdict> {
    reduce(
        GuardrailStage::ToolOutput,
        "the call's result was refused before the model was shown it",
        firings,
    )
}

fn reduce(
    stage: GuardrailStage,
    refusal: &str,
    firings: impl IntoIterator<Item = (ToolGuardrailId, ToolGuardrailFunctionOutput)>,
) -> Result<ToolGuardrailVerdict> {
    let mut verdict = ToolGuardrailVerdict::Allow;
    let mut raised: Option<(ToolGuardrailId, String)> = None;

    for (guardrail, output) in firings {
        match output.behavior() {
            ToolGuardrailBehavior::Allow => {}
            ToolGuardrailBehavior::RejectContent { message } => {
                if verdict.is_allow() {
                    verdict = ToolGuardrailVerdict::RejectContent {
                        guardrail,
                        message: message.clone(),
                    };
                }
            }
            ToolGuardrailBehavior::RaiseException => {
                if raised.is_none() {
                    raised = Some((guardrail, output.detail()));
                }
            }
        }
    }

    match raised {
        Some((guardrail, detail)) => Err(Error::guardrail(
            stage,
            guardrail.as_str(),
            format!("{refusal} ({detail})"),
        )),
        None => Ok(verdict),
    }
}
