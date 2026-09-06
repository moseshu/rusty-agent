//! The plan board: the steps an agent means to take, and which one it is on.
//!
//! # The board is the call, not the tool
//!
//! [`UpdatePlanTool`] holds no plan. Each call carries the whole plan, and the authoritative copy is
//! the tool call itself, sitting in the run's history where a resume, a replay, and a host UI all
//! read the same bytes.
//!
//! Keeping a copy in the tool would create a second one. It would be per-process rather than
//! per-run, so two runs sharing an installed capability would share a plan; it would be empty after
//! a resume, so a run that came back from a checkpoint would report no plan while its own history
//! showed six steps; and it would have to be reconciled with history at some point anyway, because
//! history is what a host reads. Whole-plan replacement is what makes that possible — a call that
//! carried a *delta* would only be interpretable against the state it was applied to, which is
//! exactly the state this design does not keep.
//!
//! # What it does not do
//!
//! It does not re-show the plan on later turns. Codex records the plan and says nothing more; Claude
//! Code projects the list back into every request as a reminder. That difference is a product's
//! policy rather than a property of the entry — one product's agents work from a plan they were just
//! shown, another's from a plan in the transcript — so the reminder belongs to whichever product
//! wants it, built on the prompt crate's reminder channel. What is here is the half both need: a
//! structured plan, recorded once, in a form neither has to parse out of prose.
//!
//! It also emits no host event. A tool call *is* the record, and a host that renders a plan board
//! reads it from the same history everything else does rather than from a second stream that could
//! disagree with it.

use std::fmt::{self, Write as _};

use async_trait::async_trait;
use ra_core::{
    error::{Error, Result, ToolErrorKind},
    permission::PermissionScope,
    tool::{
        DecodedToolInput, FuncSchema, ObservationMetadata, Tool, ToolArgumentDecodeError,
        ToolConcurrency, ToolContext, ToolFailureHandling, ToolOptions, ToolOrigin, ToolOutput,
        ToolSchema,
    },
};
use ra_macros::ToolInput;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The advertised name. Identity and schema must agree on it or [`Tool::validate`] refuses.
const TOOL_NAME: &str = "update_plan";

/// Where one step stands.
///
/// Three states rather than a boolean, because the middle one is the whole point: a reader of the
/// transcript — a host UI, a later turn, an evaluation — can tell "this was attempted and is
/// unfinished" from "this has not been started", and those lead somewhere different.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PlanStepStatus {
    /// Not started.
    Pending,
    /// Being worked on now.
    InProgress,
    /// Finished.
    Completed,
}

impl PlanStepStatus {
    /// Stable machine-readable label, identical to the serde wire value.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
        }
    }
}

impl fmt::Display for PlanStepStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.label())
    }
}

// **The doc comment below is model-facing**, because this type is part of the advertised schema:
// `schemars` renders it as the description of the `PlanStep` definition, which every turn carrying
// this entry pays for. A paragraph explaining the type to a Rust reader would be sent to the model
// verbatim — 450 bytes of rationale it cannot act on — so the explanation lives here instead.
//
// The type is public because the plan travels as a tool call's arguments, and a host that renders a
// plan board deserializes exactly this; a private shape would leave every such host re-declaring
// it, and the declarations would drift the first time a field was added. The *fields* are private
// for the mirror-image reason: a host constructing one by literal would make every later field a
// breaking change, while the wire shape it deserializes is unaffected by whether the fields can
// also be read directly.
/// One step of a plan.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PlanStep {
    /// What is to be done, in one line.
    step: String,
    /// Where it stands.
    status: PlanStepStatus,
}

impl PlanStep {
    /// Creates one step.
    #[must_use]
    pub fn new(step: impl Into<String>, status: PlanStepStatus) -> Self {
        Self {
            step: step.into(),
            status,
        }
    }

    /// What is to be done.
    #[must_use]
    pub fn step(&self) -> &str {
        &self.step
    }

    /// Where it stands.
    #[must_use]
    pub const fn status(&self) -> PlanStepStatus {
        self.status
    }
}

/// Ceilings this entry applies to a plan.
///
/// A plan is re-sent whole on every update and read by everything downstream, so an unbounded one is
/// paid for repeatedly. The numbers are deliberately generous: they exist to stop a plan from
/// becoming a document, not to make the model ration steps.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlanLimits {
    steps: usize,
    step_chars: usize,
}

impl Default for PlanLimits {
    fn default() -> Self {
        Self::new()
    }
}

impl PlanLimits {
    /// Creates the defaults.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            steps: 24,
            step_chars: 200,
        }
    }

    /// Sets the ceiling on how many steps one plan may hold.
    #[must_use]
    pub const fn with_max_steps(mut self, steps: usize) -> Self {
        self.steps = if steps == 0 { 1 } else { steps };
        self
    }

    /// Sets the ceiling on one step's length, in characters.
    #[must_use]
    pub const fn with_max_step_chars(mut self, chars: usize) -> Self {
        self.step_chars = if chars == 0 { 1 } else { chars };
        self
    }

    /// Ceiling on how many steps one plan may hold.
    #[must_use]
    pub const fn max_steps(&self) -> usize {
        self.steps
    }

    /// Ceiling on one step's length, in characters.
    #[must_use]
    pub const fn max_step_chars(&self) -> usize {
        self.step_chars
    }
}

// The doc comment below is the model-facing description. It states the two rules a caller can
// violate — send the whole plan, keep one step in progress — because both are refusals it would
// otherwise discover by being refused.
//
// One field, where Codex's has two: its `explanation` says why the plan changed, and this framework
// already has somewhere for that. Commentary is a channel of its own here, delivered as the model
// works rather than as an argument to the entry that happens to be next, so an `explanation` would
// be a second place to say the same thing and schema bytes spent on every turn to offer it.
#[derive(Debug, Deserialize, JsonSchema, ToolInput)]
#[serde(deny_unknown_fields)]
/// Records the plan for the current task. Send the whole plan every time; it replaces the previous
/// one. At most one step may be `in_progress`.
struct UpdatePlanInput {
    /// The steps, in the order they will be done.
    plan: Vec<PlanStep>,
}

/// The `update_plan` tool.
#[derive(Debug)]
pub struct UpdatePlanTool {
    origin: ToolOrigin,
    func_schema: FuncSchema,
    options: ToolOptions,
    limits: PlanLimits,
}

impl UpdatePlanTool {
    /// Creates the plan entry.
    ///
    /// It takes no workspace, no store, and no manager: recording a plan reaches nothing outside the
    /// conversation, which is also why its permission scope is [`PermissionScope::Read`] rather than
    /// something a read-only role would have to withhold.
    ///
    /// # Errors
    ///
    /// Returns a configuration error when the tool's identity or schema cannot be built.
    pub fn new() -> Result<Self> {
        Ok(Self {
            origin: ToolOrigin::new(TOOL_NAME)?,
            func_schema: FuncSchema::for_input::<UpdatePlanInput>(TOOL_NAME)?,
            options: ToolOptions::new()
                .with_failure_handling(ToolFailureHandling::Custom)
                .with_permission_scope(PermissionScope::Read)
                .with_concurrency(ToolConcurrency::Parallel),
            limits: PlanLimits::new(),
        })
    }

    /// Replaces the plan ceilings.
    #[must_use]
    pub const fn with_limits(mut self, limits: PlanLimits) -> Self {
        self.limits = limits;
        self
    }

    /// The ceilings in force.
    #[must_use]
    pub const fn limits(&self) -> &PlanLimits {
        &self.limits
    }

    /// Checks the plan and renders what was recorded.
    ///
    /// The checks are structural — a plan nobody can act on, or one claiming two simultaneous
    /// steps — and each of them is something the next call can fix. What is deliberately not checked
    /// is whether the steps are *good*: that is the model's judgement, and a tool that graded it
    /// would be enforcing a planning style through a refusal.
    fn record(&self, input: &UpdatePlanInput) -> PlanResult<ToolOutput> {
        if input.plan.is_empty() {
            return Err(PlanFailure::Empty);
        }
        if input.plan.len() > self.limits.steps {
            return Err(PlanFailure::TooManySteps {
                steps: input.plan.len(),
                ceiling: self.limits.steps,
            });
        }
        for (index, step) in input.plan.iter().enumerate() {
            if step.step().trim().is_empty() {
                return Err(PlanFailure::EmptyStep {
                    position: index + 1,
                });
            }
            let chars = step.step().chars().count();
            if chars > self.limits.step_chars {
                return Err(PlanFailure::StepTooLong {
                    position: index + 1,
                    chars,
                    ceiling: self.limits.step_chars,
                });
            }
        }

        let in_progress: Vec<&str> = input
            .plan
            .iter()
            .filter(|step| step.status() == PlanStepStatus::InProgress)
            .map(PlanStep::step)
            .collect();
        if in_progress.len() > 1 {
            return Err(PlanFailure::SeveralInProgress {
                steps: in_progress.len(),
            });
        }

        let completed = input
            .plan
            .iter()
            .filter(|step| step.status() == PlanStepStatus::Completed)
            .count();
        let total = input.plan.len();
        // A summary rather than the plan echoed back. The plan is already in this call's arguments,
        // which the model can see and a host reads authoritatively; repeating it in the result would
        // put two copies of the same list in the same turn, and pay for the longer one on every
        // request that carries the history afterwards.
        let mut body = format!("Plan recorded: {total} steps, {completed} completed.");
        let mut metadata = ObservationMetadata::new();
        match in_progress.first() {
            Some(step) => {
                let _ = write!(body, " In progress: {step}");
            }
            None if completed < total => {
                metadata = metadata
                    .with_guidance("Mark the step you are working on as `in_progress` next time.");
            }
            None => {}
        }
        Ok(ToolOutput::text(body).with_metadata(metadata))
    }
}

type PlanResult<T> = std::result::Result<T, PlanFailure>;

/// Why a plan was not recorded.
///
/// It travels in an [`Error`]'s source rather than in its message, so the model-facing sentence is
/// produced once, in [`Tool::handle_failure`], from a value rather than from prose.
#[derive(Debug)]
enum PlanFailure {
    /// The argument object does not match the schema.
    BadArguments(ToolArgumentDecodeError),
    /// The plan has no steps.
    Empty,
    /// More steps than this entry will record.
    TooManySteps { steps: usize, ceiling: usize },
    /// A step with no text, counted from one.
    EmptyStep { position: usize },
    /// A step longer than this entry will record, counted from one.
    StepTooLong {
        position: usize,
        chars: usize,
        ceiling: usize,
    },
    /// Several steps claim to be in progress at once.
    SeveralInProgress { steps: usize },
}

impl PlanFailure {
    /// Which failure class the host records.
    ///
    /// A constant, and that is the whole finding: every one of these is something the same call
    /// could have avoided, and nothing here can fail for any other reason — the entry reaches no
    /// store, no process, and no network. The day a variant arrives that is not the caller's
    /// mistake, this becomes a match like every other entry's.
    const KIND: ToolErrorKind = ToolErrorKind::InvalidInput;

    fn into_error(self) -> Error {
        Error::tool(Self::KIND, TOOL_NAME, self.to_string()).with_source(self)
    }

    fn of(error: &Error) -> Option<&Self> {
        std::error::Error::source(error).and_then(<dyn std::error::Error + 'static>::downcast_ref)
    }

    /// The next step that follows the diagnosis, kept out of [`fmt::Display`] so a log line gets the
    /// fact without an instruction addressed to a model.
    const fn next_step(&self) -> &'static str {
        match self {
            Self::BadArguments(_) => "Send `plan` as a list of `{step, status}` objects.",
            Self::Empty => "Send at least one step, or do not call this entry.",
            Self::TooManySteps { .. } => "Send fewer, larger steps.",
            Self::EmptyStep { .. } => "Give every step text, or leave it out of the plan.",
            Self::StepTooLong { .. } => "Say what the step is in one line.",
            Self::SeveralInProgress { .. } => "Leave exactly one step `in_progress`.",
        }
    }
}

impl fmt::Display for PlanFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            // The decoder's own words, because they name the offending field, which is the whole of
            // what makes this failure correctable on the next turn.
            Self::BadArguments(reason) => write!(formatter, "Invalid arguments: {reason}."),
            Self::Empty => formatter.write_str("The plan has no steps."),
            Self::TooManySteps { steps, ceiling } => write!(
                formatter,
                "The plan has {steps} steps, over the limit of {ceiling}."
            ),
            Self::EmptyStep { position } => {
                write!(formatter, "Step {position} has no text.")
            }
            Self::StepTooLong {
                position,
                chars,
                ceiling,
            } => write!(
                formatter,
                "Step {position} is {chars} characters, over the limit of {ceiling}."
            ),
            Self::SeveralInProgress { steps } => write!(
                formatter,
                "{steps} steps are marked `in_progress`; at most one may be."
            ),
        }
    }
}

impl std::error::Error for PlanFailure {}

#[async_trait]
impl Tool for UpdatePlanTool {
    fn origin(&self) -> &ToolOrigin {
        &self.origin
    }

    fn schema(&self) -> &ToolSchema {
        self.func_schema.tool_schema()
    }

    fn func_schema(&self) -> Option<&FuncSchema> {
        Some(&self.func_schema)
    }

    fn decode_input(&self, arguments: &serde_json::Value) -> Result<Option<DecodedToolInput>> {
        self.func_schema
            .decode_value_diagnostic(arguments.clone())
            .map(Some)
            .map_err(|error| PlanFailure::BadArguments(error).into_error())
    }

    async fn call(&self, mut context: ToolContext<'_>) -> Result<ToolOutput> {
        let input = match context.take_decoded_input::<UpdatePlanInput>()? {
            Some(input) => input,
            None => serde_json::from_value(context.arguments().clone()).map_err(|error| {
                PlanFailure::BadArguments(ToolArgumentDecodeError::Deserialize {
                    input_type: self.func_schema.input_type_name(),
                    message: error.to_string(),
                })
                .into_error()
            })?,
        };
        self.record(&input).map_err(PlanFailure::into_error)
    }

    fn options(&self) -> ToolOptions {
        self.options.clone()
    }

    async fn handle_failure(
        &self,
        _context: &ToolContext<'_>,
        error: &Error,
    ) -> Result<Option<ToolOutput>> {
        // Control signals must reach the runner rather than becoming an observation: they stop or
        // close out work instead of describing a plan that was not recorded.
        if error.is_cancelled() || matches!(error, Error::Budget { .. } | Error::Guardrail { .. }) {
            return Ok(None);
        }
        Ok(PlanFailure::of(error).map(|failure| {
            ToolOutput::text(failure.to_string())
                .with_metadata(ObservationMetadata::new().with_guidance(failure.next_step()))
        }))
    }
}
