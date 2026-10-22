// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Codex's plan tool: the original schema, argument errors and `Plan updated` response.
//!
//! The call's arguments retain the complete plan and optional explanation in run history. A host
//! reads that record instead of a second process-local plan. Collaboration modes are not yet
//! available in the run context, so this tool operates in the runtime's Default mode.
//!
//! Size limits are a separately selected host extension. The default accepts the same plans as
//! Codex, including an empty plan, blank step text and multiple steps marked in progress.

use std::fmt;

use async_trait::async_trait;
use ra_core::{
    error::{Error, Result, ToolErrorKind},
    tool::{
        Tool, ToolContext, ToolFailureHandling, ToolOptions, ToolOrigin, ToolOutput, ToolSchema,
    },
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;

const TOOL_NAME: &str = "update_plan";
const DESCRIPTION: &str = "Updates the task plan.\nProvide an optional explanation and a list of plan items, each with a step and status.\nAt most one step can be in_progress at a time.\n";

/// Codex's `StepStatus`.
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

/// Codex's `PlanItemArg`, retained in the tool call's arguments.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PlanStep {
    /// Task step text.
    step: String,
    /// Step status.
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

/// Optional host-selected size limits. Defaults impose no limit beyond Rust's collection sizes.
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
    /// Creates unlimited defaults, preserving Codex's default acceptance behavior.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            steps: usize::MAX,
            step_chars: usize::MAX,
        }
    }

    /// Selects a ceiling on how many steps one plan may hold.
    #[must_use]
    pub const fn with_max_steps(mut self, steps: usize) -> Self {
        self.steps = if steps == 0 { 1 } else { steps };
        self
    }

    /// Selects a ceiling on one step's length, in characters.
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdatePlanInput {
    // The original call already persists the explanation alongside the plan.
    #[serde(default, rename = "explanation")]
    _explanation: Option<String>,
    plan: Vec<PlanStep>,
}

/// The `update_plan` tool, with Codex's native non-strict schema.
#[derive(Debug)]
pub struct UpdatePlanTool {
    origin: ToolOrigin,
    schema: ToolSchema,
    options: ToolOptions,
    limits: PlanLimits,
}

impl UpdatePlanTool {
    /// Creates the plan tool with no additional acceptance rules.
    pub fn new() -> Result<Self> {
        let parameters = json!({
            "type": "object",
            "properties": {
                "explanation": {"type":"string", "description":"Optional explanation for this plan update."},
                "plan": {"type":"array", "description":"The list of steps", "items": {
                    "type":"object", "properties": {
                        "step": {"type":"string", "description":"Task step text."},
                        "status": {"type":"string", "enum":["pending","in_progress","completed"], "description":"Step status."}
                    }, "required":["step","status"], "additionalProperties":false
                }}
            },
            "required":["plan"], "additionalProperties":false
        });
        Ok(Self {
            origin: ToolOrigin::new(TOOL_NAME)?,
            schema: ToolSchema::loose(TOOL_NAME, parameters)?.with_description(DESCRIPTION),
            options: ToolOptions::new()
                .with_failure_handling(ToolFailureHandling::Custom)
                .with_action_classes([crate::action_classes::READ]),
            limits: PlanLimits::new(),
        })
    }

    /// Explicitly selects host size limits; these are not part of Codex's default contract.
    #[must_use]
    pub const fn with_limits(mut self, limits: PlanLimits) -> Self {
        self.limits = limits;
        self
    }

    /// The size limits in force.
    #[must_use]
    pub const fn limits(&self) -> &PlanLimits {
        &self.limits
    }

    fn record(&self, input: &UpdatePlanInput) -> std::result::Result<ToolOutput, String> {
        if input.plan.len() > self.limits.steps {
            return Err(format!(
                "The plan has {} steps, over the limit of {}.",
                input.plan.len(),
                self.limits.steps
            ));
        }
        for (index, step) in input.plan.iter().enumerate() {
            let chars = step.step().chars().count();
            if chars > self.limits.step_chars {
                return Err(format!(
                    "Step {} is {chars} characters, over the limit of {}.",
                    index + 1,
                    self.limits.step_chars
                ));
            }
        }
        Ok(ToolOutput::text("Plan updated"))
    }
}

#[async_trait]
impl Tool for UpdatePlanTool {
    fn origin(&self) -> &ToolOrigin {
        &self.origin
    }
    fn schema(&self) -> &ToolSchema {
        &self.schema
    }
    fn options(&self) -> ToolOptions {
        self.options.clone()
    }

    async fn call(&self, context: ToolContext<'_>) -> Result<ToolOutput> {
        let input: UpdatePlanInput =
            serde_json::from_value(context.arguments().clone()).map_err(|error| {
                Error::tool(
                    ToolErrorKind::InvalidInput,
                    TOOL_NAME,
                    format!("failed to parse function arguments: {error}"),
                )
            })?;
        self.record(&input)
            .map_err(|message| Error::tool(ToolErrorKind::InvalidInput, TOOL_NAME, message))
    }

    async fn handle_failure(
        &self,
        _: &ToolContext<'_>,
        error: &Error,
    ) -> Result<Option<ToolOutput>> {
        if error.is_control_signal() {
            return Ok(None);
        }
        Ok(match error {
            Error::Tool { tool, message, .. } if tool == TOOL_NAME => {
                Some(ToolOutput::text(message.clone()))
            }
            _ => None,
        })
    }
}
