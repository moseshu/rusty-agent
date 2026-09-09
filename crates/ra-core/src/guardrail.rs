//! The two host-declared checks that bracket a run: the input before an agent acts on it, and the
//! output before it is delivered.
//!
//! Mirrors `openai-agents-python`'s `guardrail.py`: a check returns
//! [`GuardrailFunctionOutput`], a tripwire in it stops the run, and everything else it recorded is
//! kept as evidence.
//!
//! # Whose checks these are
//!
//! They are the **host's**, written by whoever configures the agent and attached to an agent or to
//! a run. This framework installs none of its own and has no way to. There is no budget on them
//! either: a ceiling on what a host may verify about its own inputs would be a ceiling on somebody
//! else's safety review.
//!
//! # Why the verdict is a value and the refusal is an error
//!
//! [`GuardrailFunctionOutput`] is what a guardrail returns on **every** check, tripped or not: the
//! `output_info` of a check that passed is the evidence that it ran and what it saw, and a host
//! auditing "did the classifier actually look at this" has nothing to read if a pass produces
//! nothing. It is plain JSON, so the same value goes into the checkpoint, the trace, and the UI.
//!
//! A tripwire, by contrast, ends the run through [`Error::Guardrail`], because everything about
//! this framework's control flow reads [`Recoverability`](crate::error::Recoverability): a refusal
//! that came back as an ordinary value would have to be noticed by each caller in turn, and the
//! first one that forgot would deliver exactly the output the guardrail refused.
//!
//! # Identity is a name, and nothing is stopping two checks from sharing one
//!
//! [`InputGuardrail::name`] mirrors upstream's `get_name()`: a display string for tracing and
//! attribution, with no charset rule and no uniqueness requirement. Two guardrails under one name
//! both run and both file a verdict, exactly as upstream's list concatenation does.
//!
//! **The deviation is in the result, not the contract.** Upstream's `InputGuardrailResult` holds
//! the live guardrail object; a result here is persisted into a checkpoint and read back after the
//! process that produced it is gone, so it stores the name instead. That makes the name the only
//! thing a restored verdict can be attributed by — which is a property of the persistence, and does
//! **not** give the name object identity. Two checks may still share one.
//!
//! An earlier version of this module refused the second one, arguing that two verdicts under one
//! name cannot be told apart by a checkpoint or a report. They cannot — but that is a reason to
//! record more, not to reject a configuration upstream accepts.
//!
//! **Position does not answer it either.** Verdicts are collected as each check completes, and the
//! blocking half of the input stage finishes before the raced half starts, so the order results
//! arrive in is not the order guardrails were registered in. Anything that has to know *which
//! registration* produced a verdict — a resumed run skipping the checks that already ran, most of
//! all — needs an explicit record of execution progress. Neither the name nor the count of results
//! is one.

use core::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    context::RunContext,
    error::{Error, GuardrailStage, Result},
    finish::FinishReason,
    item::{Message, ModelInputItem, ToolCallOutput},
    tool::ToolOutput,
};

pub mod tool;

pub use tool::{
    ToolGuardrailBehavior, ToolGuardrailFunctionOutput, ToolGuardrailVerdict, ToolInputGuardrail,
    ToolInputGuardrailData, ToolInputGuardrailResult, ToolOutputGuardrail, ToolOutputGuardrailData,
    ToolOutputGuardrailResult, reduce_tool_input_guardrails, reduce_tool_output_guardrails,
};

/// What one guardrail concluded: a verdict, plus whatever it wants recorded about how it got there.
///
/// The two fields answer different questions and neither implies the other. `tripwire_triggered`
/// is control flow — it decides whether the run continues. `output_info` is evidence: a
/// classifier's score, the rule that matched, the model call the check made. A guardrail that
/// passes should still fill it in, because "this check ran and found nothing" and "this check was
/// never installed" are indistinguishable afterwards without it.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GuardrailFunctionOutput {
    #[serde(default, skip_serializing_if = "Value::is_null")]
    output_info: Value,
    tripwire_triggered: bool,
}

impl GuardrailFunctionOutput {
    /// The run may proceed.
    #[must_use]
    pub const fn pass() -> Self {
        Self {
            output_info: Value::Null,
            tripwire_triggered: false,
        }
    }

    /// The run must stop.
    #[must_use]
    pub const fn tripwire() -> Self {
        Self {
            output_info: Value::Null,
            tripwire_triggered: true,
        }
    }

    /// Attaches the evidence behind the verdict.
    #[must_use]
    pub fn with_output_info(mut self, output_info: Value) -> Self {
        self.output_info = output_info;
        self
    }

    /// Evidence the guardrail recorded, [`Value::Null`] when it offered none.
    #[must_use]
    pub const fn output_info(&self) -> &Value {
        &self.output_info
    }

    /// Whether this verdict stops the run.
    #[must_use]
    pub const fn tripwire_triggered(&self) -> bool {
        self.tripwire_triggered
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

impl Default for GuardrailFunctionOutput {
    /// Passing. The default has to be the verdict that is safe to reach by accident, and a value
    /// that stopped runs because a field was left unset would be the other one.
    fn default() -> Self {
        Self::pass()
    }
}

/// Every verdict a refused stage reached, carried by the refusal itself.
///
/// A tripwire ends the run through [`Error::Guardrail`], and a run that ended that way has no
/// result for a host to read its verdicts from. So they ride on the error — the tripping one and
/// every sibling that finished before it, with the `output_info` each recorded intact.
///
/// This is upstream's shape: its tripwire exception carries the `guardrail_result` that triggered
/// it, and its base exception carries the run's accumulated result lists. Without it the evidence
/// a refusal is argued from survives only as prose inside the error message, which is the one form
/// nothing can read programmatically.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub enum GuardrailEvidence {
    /// Verdicts from the stage that examines the run's input.
    Input(Vec<InputGuardrailResult>),
    /// Verdicts from the stage that examines what the run is about to deliver.
    Output(Vec<OutputGuardrailResult>),
}

impl GuardrailEvidence {
    /// Which stage these came from.
    #[must_use]
    pub const fn stage(&self) -> GuardrailStage {
        match self {
            Self::Input(_) => GuardrailStage::Input,
            Self::Output(_) => GuardrailStage::Output,
        }
    }

    /// The input verdicts, or `None` when this evidence is from the other stage.
    #[must_use]
    pub fn input(&self) -> Option<&[InputGuardrailResult]> {
        match self {
            Self::Input(results) => Some(results),
            Self::Output(_) => None,
        }
    }

    /// The output verdicts, or `None` when this evidence is from the other stage.
    #[must_use]
    pub fn output(&self) -> Option<&[OutputGuardrailResult]> {
        match self {
            Self::Output(results) => Some(results),
            Self::Input(_) => None,
        }
    }
}

/// One input guardrail's verdict, under the name of the guardrail that reached it.
///
/// Held apart from [`OutputGuardrailResult`] rather than carrying a stage field, because the two
/// are produced at opposite ends of a run and stored in different places. One type with a stage
/// tag would make putting an output verdict where the input verdicts are recorded a runtime
/// mistake instead of a compile error.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InputGuardrailResult {
    guardrail: String,
    output: GuardrailFunctionOutput,
}

impl InputGuardrailResult {
    /// Files a verdict under the guardrail that produced it.
    #[must_use]
    pub fn new(guardrail: impl Into<String>, output: GuardrailFunctionOutput) -> Self {
        Self {
            guardrail: guardrail.into(),
            output,
        }
    }

    /// Which guardrail reached this verdict.
    #[must_use]
    pub fn guardrail(&self) -> &str {
        &self.guardrail
    }

    /// The verdict and its evidence.
    #[must_use]
    pub const fn output(&self) -> &GuardrailFunctionOutput {
        &self.output
    }

    /// Whether this verdict stops the run.
    #[must_use]
    pub const fn tripwire_triggered(&self) -> bool {
        self.output.tripwire_triggered()
    }

    /// The typed refusal this verdict stands for, and `None` when the guardrail let the run pass.
    ///
    /// Built here rather than at the call site so that the stage and the name cannot be supplied
    /// by whoever is reporting the block. A caller that named the wrong stage would produce an
    /// error whose `code()` sends every report and every metric to the other end of the run.
    #[must_use]
    pub fn tripwire_error(&self) -> Option<Error> {
        self.tripwire_triggered().then(|| {
            Error::guardrail(
                GuardrailStage::Input,
                &self.guardrail,
                format!(
                    "the run's input was refused before an agent acted on it ({})",
                    self.output.detail()
                ),
            )
        })
    }
}

impl fmt::Display for InputGuardrailResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} ({})",
            self.guardrail,
            if self.tripwire_triggered() {
                "tripwire"
            } else {
                "pass"
            }
        )
    }
}

/// One output guardrail's verdict, under the name of the guardrail that reached it.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutputGuardrailResult {
    guardrail: String,
    output: GuardrailFunctionOutput,
}

impl OutputGuardrailResult {
    /// Files a verdict under the guardrail that produced it.
    #[must_use]
    pub fn new(guardrail: impl Into<String>, output: GuardrailFunctionOutput) -> Self {
        Self {
            guardrail: guardrail.into(),
            output,
        }
    }

    /// Which guardrail reached this verdict.
    #[must_use]
    pub fn guardrail(&self) -> &str {
        &self.guardrail
    }

    /// The verdict and its evidence.
    #[must_use]
    pub const fn output(&self) -> &GuardrailFunctionOutput {
        &self.output
    }

    /// Whether this verdict stops the run.
    #[must_use]
    pub const fn tripwire_triggered(&self) -> bool {
        self.output.tripwire_triggered()
    }

    /// The typed refusal this verdict stands for, and `None` when the guardrail let the run pass.
    #[must_use]
    pub fn tripwire_error(&self) -> Option<Error> {
        self.tripwire_triggered().then(|| {
            Error::guardrail(
                GuardrailStage::Output,
                &self.guardrail,
                format!(
                    "the run's final output was refused before it was delivered ({})",
                    self.output.detail()
                ),
            )
        })
    }
}

impl fmt::Display for OutputGuardrailResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} ({})",
            self.guardrail,
            if self.tripwire_triggered() {
                "tripwire"
            } else {
                "pass"
            }
        )
    }
}

/// What the run is about to deliver, as an output guardrail sees it.
///
/// A borrowed view rather than the run's own result: a guardrail decides whether an answer may be
/// delivered, and handing it the object that also carries the checkpoint and the accounting would
/// invite it to answer a different question.
///
/// # An answer is not always an assistant message
///
/// A run that ends on [`FinishReason::ToolStop`] promoted a tool result to the answer and produced
/// no assistant message to carry it. Handing such a run's guardrail an empty string would be worse
/// than not asking at all: the check runs, reports a pass, and has examined nothing.
///
/// So the view carries both. [`Self::message`] is the assistant's own final message when there is
/// one, [`Self::tool_outputs`] is what the concluding turn's calls produced, and [`Self::text`]
/// reads whichever of the two the run actually delivered.
///
/// **Which tool results a policy promoted is not re-derived here.** `stop_on_first_tool`, a name
/// list, and a host's own handler each pick a different subset, and a second reading of that
/// decision would sooner or later disagree with the first. The turn's outputs are handed over
/// whole, with [`Self::finish_reason`] saying whether any of them is the answer.
#[non_exhaustive]
#[derive(Debug, Clone, Copy)]
pub struct GuardrailFinalOutput<'a> {
    message: Option<&'a Message>,
    tool_outputs: &'a [ToolCallOutput],
    finish_reason: FinishReason,
}

impl<'a> GuardrailFinalOutput<'a> {
    /// Describes a delivery, before either half of what it carries is attached.
    #[must_use]
    pub const fn new(finish_reason: FinishReason) -> Self {
        Self {
            message: None,
            tool_outputs: &[],
            finish_reason,
        }
    }

    /// Attaches the assistant message the run delivers.
    #[must_use]
    pub const fn with_message(mut self, message: &'a Message) -> Self {
        self.message = Some(message);
        self
    }

    /// Attaches what the concluding turn's tool calls produced.
    #[must_use]
    pub const fn with_tool_outputs(mut self, tool_outputs: &'a [ToolCallOutput]) -> Self {
        self.tool_outputs = tool_outputs;
        self
    }

    /// The assistant message being delivered, when the run produced one.
    #[must_use]
    pub const fn message(&self) -> Option<&'a Message> {
        self.message
    }

    /// What the concluding turn's tool calls produced, in settlement order.
    ///
    /// Empty on a run that concluded without calling anything. On [`FinishReason::ToolStop`] the
    /// answer is among these.
    #[must_use]
    pub const fn tool_outputs(&self) -> &'a [ToolCallOutput] {
        self.tool_outputs
    }

    /// The delivered text.
    ///
    /// The assistant's final message when the run produced one; otherwise the concluding turn's
    /// tool outputs, one per line. Empty only when the run delivered neither — a run stopped from
    /// outside, where there is no answer for a guardrail to weigh.
    ///
    /// A stored tool output is recovered through [`ToolOutput::from_stored`] and read for its text
    /// rather than having its JSON envelope rendered. The envelope is a persistence detail; a
    /// guardrail asked to judge an answer would otherwise be matching against `schema_version` and
    /// block tags instead of the words the tool produced.
    #[must_use]
    pub fn text(&self) -> String {
        if let Some(message) = self.message {
            return message.text_content();
        }
        self.tool_outputs
            .iter()
            .map(|output| {
                let stored = ToolOutput::from_stored(output.output()).ok().flatten();
                match stored.as_ref().and_then(ToolOutput::as_text) {
                    Some(text) => text.to_owned(),
                    // Not a stored tool output, or one carrying no text at all: an image, say.
                    // Its rendered value is the honest answer — better than dropping it and
                    // handing the check a shorter answer than the one being delivered.
                    None => match output.output() {
                        Value::String(text) => text.clone(),
                        other => other.to_string(),
                    },
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Why the loop stopped, which is what says how the answer was reached.
    #[must_use]
    pub const fn finish_reason(&self) -> FinishReason {
        self.finish_reason
    }
}

/// A host check applied to a run's input before an agent acts on it.
///
/// # Racing the first model call, or blocking it
///
/// A guardrail is usually a model call of its own, and serializing two of them doubles the latency
/// of every run to protect against the small fraction that are refused. So the default — like
/// upstream's `run_in_parallel` — is to start the check together with the first model call and
/// stop the run the moment it trips.
///
/// The trade that makes is explicit: a check slower than the model call it races may not return
/// until after that turn's tools have run, and the refusal then stops the run rather than the
/// turn. A check that has to be decided before anything executes returns `false` from
/// [`Self::run_in_parallel`] and is awaited before the first request goes out, paying its latency
/// on every run to get that guarantee.
///
/// # Cancellation
///
/// This is third-party `async` code, so the runtime awaits it inside the run's cancellation scope
/// rather than bare. On cancellation the returned future is **dropped**, which is safe for a pure
/// future: an implementation that spawns a task or a child process owns draining it, exactly as a
/// [`Tool`](crate::tool::Tool) does.
#[async_trait]
pub trait InputGuardrail: Send + Sync + 'static {
    /// Display name for this check, carried into its verdict, its trace field, and its refusal.
    ///
    /// Not an identity: nothing requires it to be unique, and two checks sharing a name both run
    /// and both file a verdict. See the module documentation.
    fn name(&self) -> &str;

    /// Whether this check races the first model call rather than blocking it.
    ///
    /// Defaults to racing, which is upstream's default and the one that costs nothing on the runs
    /// that pass.
    fn run_in_parallel(&self) -> bool {
        true
    }

    /// Examines the run's input.
    ///
    /// `input` is what the caller supplied when the run started, in the order the first model
    /// request carried it. A segment that resumes the run is handed that same input rather than
    /// the history it continues from: the check is about what was asked for, and a transcript is
    /// a different question that a check written for the first one would answer wrongly.
    ///
    /// Returning `Err` fails the run with that error rather than with a tripwire: a guardrail that
    /// could not reach a verdict has not decided anything, and reporting it as a refusal would
    /// blame the input for the checker being down.
    async fn check(
        &self,
        context: &RunContext,
        input: &[ModelInputItem],
    ) -> Result<GuardrailFunctionOutput>;
}

/// A host check applied to a run's final output before it is delivered.
///
/// It runs where the loop reaches its own conclusion, and only there: a run stopped from outside —
/// an exhausted budget, a turn cap, an interrupt — has no answer the agent chose, and a closeout
/// message written by the host's own error handler is not something the host needs protecting
/// from.
///
/// A tripwire here stops the delivery. It does not send the run back for another turn: upstream
/// raises out of the loop at this point, and an answer refused is not an answer the framework can
/// improve on the agent's behalf.
///
/// # Cancellation
///
/// As for [`InputGuardrail`]: awaited inside the run's cancellation scope, and dropped when it is
/// cancelled.
#[async_trait]
pub trait OutputGuardrail: Send + Sync + 'static {
    /// Display name for this check, carried into its verdict, its trace field, and its refusal.
    fn name(&self) -> &str;

    /// Examines what the run is about to deliver.
    ///
    /// Returning `Err` fails the run with that error rather than with a tripwire, for the reason
    /// [`InputGuardrail::check`] gives.
    async fn check(
        &self,
        context: &RunContext,
        output: &GuardrailFinalOutput<'_>,
    ) -> Result<GuardrailFunctionOutput>;
}

/// Combines the input guardrails an agent declares with the ones a run adds, in that order.
///
/// Concatenation, matching upstream's `agent.input_guardrails + run_config.input_guardrails`. A
/// name appearing twice is not refused — see the module documentation for why the earlier refusal
/// was removed.
#[must_use]
pub fn merge_input_guardrails(
    agent: &[Arc<dyn InputGuardrail>],
    run: &[Arc<dyn InputGuardrail>],
) -> Vec<Arc<dyn InputGuardrail>> {
    agent.iter().chain(run).map(Arc::clone).collect()
}

/// Combines the output guardrails an agent declares with the ones a run adds, in that order.
#[must_use]
pub fn merge_output_guardrails(
    agent: &[Arc<dyn OutputGuardrail>],
    run: &[Arc<dyn OutputGuardrail>],
) -> Vec<Arc<dyn OutputGuardrail>> {
    agent.iter().chain(run).map(Arc::clone).collect()
}
