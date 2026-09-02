//! Contracts for capabilities that transform model context.
//!
//! The tool, prompt, and sampling halves of the assembly surface are not defined yet. Context
//! processing is the first part that is, so it has a narrow contract of its own instead of
//! teaching the runner about any particular capability. Implementations live in service or
//! product crates; the kernel only carries the data and callback boundary they share.

use async_trait::async_trait;

use crate::{
    error::Result,
    item::{ItemId, ModelInputItem, ModelResponse, RunItem},
    model::ModelOutputSchema,
    state::RunId,
    usage::Usage,
};

/// One context transformation installed by a host.
///
/// A processor receives authoritative history separately from the model-facing prefix and tail.
/// It may replace only the history projection while emitting new authoritative records such as a
/// compaction summary. The runner appends those records; a processor never mutates `RunState`
/// itself.
#[async_trait]
pub trait ContextProcessor: Send + Sync {
    /// Transforms the context before the ordinary model request is sent.
    async fn process_context(
        &self,
        request: ContextProcessorRequest,
        summarizer: &dyn ContextSummarizer,
    ) -> Result<ContextProcessorResult>;
}

/// Facts a context processor needs for one pending model request.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct ContextProcessorRequest {
    run_id: RunId,
    current_turn: u64,
    record_id: ItemId,
    model_name: Option<String>,
    prefix: Vec<ModelInputItem>,
    history: Vec<RunItem>,
    suffix: Vec<ModelInputItem>,
    input: Vec<ModelInputItem>,
}

impl ContextProcessorRequest {
    /// Creates the input to one context processor.
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn new(
        run_id: RunId,
        current_turn: u64,
        record_id: ItemId,
        model_name: Option<String>,
        prefix: Vec<ModelInputItem>,
        history: Vec<RunItem>,
        suffix: Vec<ModelInputItem>,
        input: Vec<ModelInputItem>,
    ) -> Self {
        Self {
            run_id,
            current_turn,
            record_id,
            model_name,
            prefix,
            history,
            suffix,
            input,
        }
    }

    /// Identity of the run whose context is being processed.
    #[must_use]
    pub const fn run_id(&self) -> &RunId {
        &self.run_id
    }

    /// Whole-run ordinal of the pending turn.
    #[must_use]
    pub const fn current_turn(&self) -> u64 {
        self.current_turn
    }

    /// Reserved ID for an authoritative record this processing pass emits.
    #[must_use]
    pub const fn record_id(&self) -> &ItemId {
        &self.record_id
    }

    /// Resolved model name, when the resolver selected one explicitly.
    #[must_use]
    pub fn model_name(&self) -> Option<&str> {
        self.model_name.as_deref()
    }

    /// Caller-owned input that precedes the authoritative generated history.
    #[must_use]
    pub fn prefix(&self) -> &[ModelInputItem] {
        &self.prefix
    }

    /// Complete authoritative records the processor may project.
    #[must_use]
    pub fn history(&self) -> &[RunItem] {
        &self.history
    }

    /// Ephemeral model-input items that follow history, such as a budget reminder.
    #[must_use]
    pub fn suffix(&self) -> &[ModelInputItem] {
        &self.suffix
    }

    /// Complete request input before this processor runs.
    #[must_use]
    pub fn input(&self) -> &[ModelInputItem] {
        &self.input
    }
}

/// Model request a context processor asks the runtime to perform.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct ContextSummaryRequest {
    input: Vec<ModelInputItem>,
    instructions: String,
    output_schema: Option<ModelOutputSchema>,
}

impl ContextSummaryRequest {
    /// Creates a summary request over the supplied model-visible input.
    #[must_use]
    pub fn new(input: Vec<ModelInputItem>, instructions: impl Into<String>) -> Self {
        Self {
            input,
            instructions: instructions.into(),
            output_schema: None,
        }
    }

    /// Requires a provider-neutral structured output schema for the summary.
    #[must_use]
    pub fn with_output_schema(mut self, output_schema: ModelOutputSchema) -> Self {
        self.output_schema = Some(output_schema);
        self
    }

    /// Input to summarize, before the runtime appends the task instruction.
    #[must_use]
    pub fn input(&self) -> &[ModelInputItem] {
        &self.input
    }

    /// Task instruction appended by the runtime as a volatile user message.
    #[must_use]
    pub fn instructions(&self) -> &str {
        &self.instructions
    }

    /// Optional structured-output contract for the summary response.
    #[must_use]
    pub const fn output_schema(&self) -> Option<&ModelOutputSchema> {
        self.output_schema.as_ref()
    }
}

/// Result of a context-summary request.
#[derive(Debug, Clone)]
pub struct ContextSummaryResponse {
    text: String,
    response: ModelResponse,
}

impl ContextSummaryResponse {
    /// Creates a summary response with the usage its request consumed.
    #[must_use]
    pub fn new(text: impl Into<String>, response: ModelResponse) -> Self {
        Self {
            text: text.into(),
            response,
        }
    }

    /// Returned summary text.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Usage charged by the summary request.
    #[must_use]
    pub const fn usage(&self) -> &Usage {
        self.response.usage()
    }

    /// Complete provider response that produced this summary.
    #[must_use]
    pub const fn response(&self) -> &ModelResponse {
        &self.response
    }
}

/// Runtime callback a processor uses when it needs a model-produced summary.
#[async_trait]
pub trait ContextSummarizer: Send + Sync {
    /// Produces one bounded summary without exposing a capability's implementation to the runner.
    async fn summarize(&self, request: ContextSummaryRequest) -> Result<ContextSummaryResponse>;
}

/// One processor's replacement model input and authoritative records.
///
/// Spend is deliberately absent as a field. It is derived from [`Self::model_responses`], because
/// a separately supplied total is a second ledger: a processor that returns two responses and the
/// usage of one would under-report the run's spend with nothing able to notice.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct ContextProcessorResult {
    input: Vec<ModelInputItem>,
    generated_items: Vec<RunItem>,
    model_responses: Vec<ModelResponse>,
}

impl ContextProcessorResult {
    /// Creates a result that only changes the model-facing input.
    #[must_use]
    pub fn new(input: Vec<ModelInputItem>) -> Self {
        Self {
            input,
            generated_items: Vec::new(),
            model_responses: Vec::new(),
        }
    }

    /// Adds authoritative records emitted by this processor.
    #[must_use]
    pub fn with_generated_items(mut self, generated_items: Vec<RunItem>) -> Self {
        self.generated_items = generated_items;
        self
    }

    /// Adds model responses produced while creating this projection.
    #[must_use]
    pub fn with_model_responses(mut self, model_responses: Vec<ModelResponse>) -> Self {
        self.model_responses = model_responses;
        self
    }

    /// Replacement input for the ordinary model call.
    #[must_use]
    pub fn input(&self) -> &[ModelInputItem] {
        &self.input
    }

    /// Records the runner must append to authoritative history.
    #[must_use]
    pub fn generated_items(&self) -> &[RunItem] {
        &self.generated_items
    }

    /// Model responses produced while creating this projection.
    #[must_use]
    pub fn model_responses(&self) -> &[ModelResponse] {
        &self.model_responses
    }

    /// Spend this projection cost, summed from the responses that paid it.
    #[must_use]
    pub fn usage(&self) -> Usage {
        self.model_responses
            .iter()
            .fold(Usage::default(), |total, response| {
                total.accumulate(response.usage())
            })
    }
}
