//! An agent exposed as a tool: `as_tool`, ported from the reference `Agent.as_tool()`.
//!
//! This is the sub-agent shape the parent keeps control through, and it differs from a handoff on
//! both axes the reference names:
//!
//! | | Agent tool | Handoff |
//! | --- | --- | --- |
//! | What the other agent receives | input the model generated for the call | the conversation, as projected |
//! | Who continues afterwards | the calling agent, with the result as a tool output | the other agent |
//!
//! So the nested run starts from nothing but the call's arguments — no parent history, which is
//! also what Codex's spawn path does by default — runs to its own conclusion, and one string comes
//! back as the call's output.
//!
//! # What the nested run inherits
//!
//! As in the reference, a tool built without its own [`RunConfig`] runs the nested agent under the
//! parent's, and with the same model resolver. The application context and the framework ports
//! installed on the parent reach the nested run too, and the nested run is recorded as a child of
//! the parent's run. Its turn cap is the tool's own `max_turns`, falling back to
//! [`DEFAULT_MAX_TURNS`] rather than to whatever the parent allowed — again the reference's rule:
//! the budget of a delegated task is a property of the delegation.
//!
//! How the parent's environment reaches this tool is a Rust adaptation. The reference reads it from
//! the tool context; here the tool context lives in `ra-core` and cannot name runtime types, so the
//! runner makes it available for the duration of the dispatched call (see `parent`). A tool called
//! directly, outside a runner, has no parent to inherit from and fails as a caller error.
//!
//! # Approvals inside the nested run
//!
//! A nested run that stops to ask the host something does not fail the call and is never shown to
//! the calling model. As in the reference, the call is left without an output, the nested run's
//! approvals are asked as the parent's, and the paused nested run is kept with the parent — in its
//! [`RunState`](ra_core::state::RunState), keyed by the call — until the host has answered them.
//! The answers are applied to the nested run's own state, so an `always` answer is a rule of that
//! nested run and nothing else. When the parent resumes it dispatches the call again, and this tool
//! continues the paused nested run from its checkpoint instead of starting a new one; the parent's
//! own checks and tool-start narration ran when the call first started and are not repeated.
//!
//! How the paused run leaves the tool is a Rust adaptation. The reference records it in a registry
//! the tool and the runner share; here [`Tool::call`] can only return an output or an error, so the
//! tool returns an error carrying the nested run's checkpoint, and dispatch turns that one error
//! into a paused call rather than a failure.
//!
//! # Streaming the nested run
//!
//! A tool given a stream handler runs the nested agent streamed and hands the handler every event
//! the nested run emits, through a bounded backlog that keeps a slow handler from holding the run
//! back; see `stream`. Which events there are is the nested run's own configuration: provider
//! deltas, for one, are only among them when that configuration asks for partial messages.
//!
//! # Not yet here
//!
//! - **Accruing nested usage on the parent.** The nested run's usage stays on its own result.
//! - **`previous_response_id`, `conversation_id` and `session`.** The first two are conversation
//!   state a provider keeps server-side, which this runner has no counterpart for; a nested session
//!   belongs with the transcript-storage work.

mod input;
pub(crate) mod parent;
mod stream;

use std::{fmt, sync::Arc};

use async_trait::async_trait;
use ra_core::{
    agent::AgentSpec,
    cancel::ScopeKind,
    context::{RunAgent, RunContext},
    error::{Error, Result, ToolErrorKind},
    finish::FinishReason,
    item::{MessageRole, RunItemKind},
    state::{RunId, RunState},
    tool::{
        FuncSchema, Tool, ToolApprovalPolicy, ToolAvailability, ToolContext, ToolFailureHandling,
        ToolInput, ToolOptions, ToolOrigin, ToolOutput, ToolSchema,
    },
};
use serde::Serialize;
use serde_json::Value;

pub use input::{
    AgentAsToolInput, STRUCTURED_INPUT_PREAMBLE, StructuredInputSchemaInfo,
    StructuredToolInputBuilder, StructuredToolInputBuilderOptions, StructuredToolInputResult,
    default_tool_input_builder, is_agent_tool_input, resolve_agent_tool_input,
};
pub(crate) use parent::ParentRun;
use stream::StreamForwarding;
pub use stream::{
    AgentToolStreamEvent, AgentToolStreamHandler, DEFAULT_ON_STREAM_MAX_PENDING_EVENTS,
};

use super::AgentBinding;
use crate::runner::{
    AgentToolInvocation, DEFAULT_MAX_TURNS, RunConfig, RunRequest, RunResult, Runner,
};

/// Extracts the tool output from a finished nested run.
///
/// Replaces the default choice of the nested run's final answer. The result carries
/// [`RunResult::agent_tool_invocation`], so the extractor knows which call it is answering.
#[async_trait]
pub trait AgentToolOutputExtractor: Send + Sync + 'static {
    /// Returns the text handed back to the calling agent.
    async fn extract(&self, result: &RunResult) -> Result<String>;
}

#[async_trait]
impl<F> AgentToolOutputExtractor for F
where
    F: Fn(&RunResult) -> Result<String> + Send + Sync + 'static,
{
    async fn extract(&self, result: &RunResult) -> Result<String> {
        self(result)
    }
}

/// Decides per run whether an agent tool is offered to the model.
#[async_trait]
pub trait AgentToolEnablement: Send + Sync + 'static {
    /// Whether the tool is available to the agent running under `context`.
    ///
    /// `agent` is the public agent **using** the tool — the caller, not the agent the tool wraps —
    /// as in the reference, which hands the callback the agent resolving its tool list. One tool
    /// shared by several agents can therefore be offered to some and withheld from others.
    ///
    /// The reference passes the agent object itself; here it is the credential-free
    /// [`RunAgent`] view, because a tool is never handed the calling agent's full declaration (see
    /// [`RunContext`]). It is the same value as `context.agent()`, passed separately to keep the
    /// reference callback's shape.
    async fn is_enabled(&self, context: &RunContext, agent: &RunAgent) -> Result<bool>;
}

#[async_trait]
impl<F> AgentToolEnablement for F
where
    F: Fn(&RunContext, &RunAgent) -> Result<bool> + Send + Sync + 'static,
{
    async fn is_enabled(&self, context: &RunContext, agent: &RunAgent) -> Result<bool> {
        self(context, agent)
    }
}

/// Decides per call whether an agent tool call waits for host approval.
#[async_trait]
pub trait AgentToolApproval: Send + Sync + 'static {
    /// Whether this call needs approval; the context carries its arguments and call ID.
    async fn needs_approval(&self, context: &ToolContext<'_>) -> Result<bool>;
}

#[async_trait]
impl<F> AgentToolApproval for F
where
    F: for<'a, 'b> Fn(&'a ToolContext<'b>) -> Result<bool> + Send + Sync + 'static,
{
    async fn needs_approval(&self, context: &ToolContext<'_>) -> Result<bool> {
        self(context)
    }
}

/// Turns a failed agent tool call into the text the calling model reads.
///
/// The reference's `failure_error_function`. Installing one selects
/// [`ToolFailureHandling::Custom`].
#[async_trait]
pub trait AgentToolErrorFunction: Send + Sync + 'static {
    /// Renders the failure for the model.
    async fn error_message(&self, context: &ToolContext<'_>, error: &Error) -> Result<String>;
}

#[async_trait]
impl<F> AgentToolErrorFunction for F
where
    F: for<'a, 'b> Fn(&'a ToolContext<'b>, &'a Error) -> Result<String> + Send + Sync + 'static,
{
    async fn error_message(&self, context: &ToolContext<'_>, error: &Error) -> Result<String> {
        self(context, error)
    }
}

/// Starts building an agent tool from an agent: the reference's `agent.as_tool(...)`.
pub trait AgentAsTool {
    /// Begins an [`AgentTool`] that runs this agent.
    fn as_tool(&self) -> AgentToolBuilder;
}

impl AgentAsTool for Arc<AgentSpec> {
    fn as_tool(&self) -> AgentToolBuilder {
        AgentTool::builder(Arc::clone(self))
    }
}

/// Converts an agent name into a function-style tool name.
///
/// Whitespace and every character outside `[A-Za-z0-9_]` become `_`, and the result is lowercased
/// — the reference's `transform_string_function_style`.
#[must_use]
pub fn transform_string_function_style(name: &str) -> String {
    name.chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '_' {
                character.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect()
}

type ParamsNormalizer = fn(&mut ToolContext<'_>, &FuncSchema) -> Result<Value>;

struct TypedParameters {
    func_schema: FuncSchema,
    normalize: ParamsNormalizer,
}

/// Builder for an [`AgentTool`].
#[must_use]
pub struct AgentToolBuilder {
    agent: Arc<AgentSpec>,
    tool_name: Option<String>,
    tool_description: Option<String>,
    output_extractor: Option<Arc<dyn AgentToolOutputExtractor>>,
    enabled: Option<bool>,
    enablement: Option<Arc<dyn AgentToolEnablement>>,
    run_config: Option<RunConfig>,
    max_turns: Option<u32>,
    propagate_failures: bool,
    error_function: Option<Arc<dyn AgentToolErrorFunction>>,
    needs_approval: Option<bool>,
    approval: Option<Arc<dyn AgentToolApproval>>,
    parameters: Option<fn(&str) -> Result<TypedParameters>>,
    input_builder: Option<Arc<dyn StructuredToolInputBuilder>>,
    include_input_schema: bool,
    on_stream: Option<Arc<dyn AgentToolStreamHandler>>,
    on_stream_max_pending_events: Option<usize>,
    options: ToolOptions,
}

impl AgentToolBuilder {
    /// Sets the tool name. Defaults to the agent's name in function style.
    pub fn tool_name(mut self, name: impl Into<String>) -> Self {
        self.tool_name = Some(name.into());
        self
    }

    /// Sets the description the model reads to decide when to call the tool.
    pub fn tool_description(mut self, description: impl Into<String>) -> Self {
        self.tool_description = Some(description.into());
        self
    }

    /// Replaces the default output with one extracted from the nested result.
    pub fn custom_output_extractor(mut self, extractor: Arc<dyn AgentToolOutputExtractor>) -> Self {
        self.output_extractor = Some(extractor);
        self
    }

    /// Statically enables or disables the tool. A disabled tool is hidden from the model.
    pub fn is_enabled(mut self, enabled: bool) -> Self {
        self.enabled = Some(enabled);
        self.enablement = None;
        self
    }

    /// Decides availability per run, from the agent that is using the tool.
    pub fn is_enabled_fn(mut self, enablement: Arc<dyn AgentToolEnablement>) -> Self {
        self.enablement = Some(enablement);
        self.enabled = None;
        self
    }

    /// Runs the nested agent under this configuration instead of the parent's.
    pub fn run_config(mut self, config: RunConfig) -> Self {
        self.run_config = Some(config);
        self
    }

    /// Sets the nested run's turn cap. Defaults to [`DEFAULT_MAX_TURNS`].
    pub const fn max_turns(mut self, max_turns: u32) -> Self {
        self.max_turns = Some(max_turns);
        self
    }

    /// Renders a failed call for the model with `error_function`.
    pub fn failure_error_function(
        mut self,
        error_function: Arc<dyn AgentToolErrorFunction>,
    ) -> Self {
        self.error_function = Some(error_function);
        self.propagate_failures = false;
        self
    }

    /// Propagates a failed call instead of showing it to the model — the reference's
    /// `failure_error_function=None`.
    pub fn propagate_failures(mut self) -> Self {
        self.propagate_failures = true;
        self.error_function = None;
        self
    }

    /// Statically requires or waives host approval before each call.
    pub fn needs_approval(mut self, needs_approval: bool) -> Self {
        self.needs_approval = Some(needs_approval);
        self.approval = None;
        self
    }

    /// Decides per call whether host approval is needed.
    pub fn needs_approval_fn(mut self, approval: Arc<dyn AgentToolApproval>) -> Self {
        self.approval = Some(approval);
        self.needs_approval = None;
        self
    }

    /// Declares the tool's argument type instead of the default `{"input": string}`.
    ///
    /// The arguments are validated against `T`'s schema and re-serialized from the decoded value,
    /// then rendered for the nested agent as structured data.
    pub fn parameters<T: ToolInput + Serialize>(mut self) -> Self {
        self.parameters = Some(typed_parameters::<T>);
        self
    }

    /// Builds the nested agent's input from the structured arguments.
    pub fn input_builder(mut self, builder: Arc<dyn StructuredToolInputBuilder>) -> Self {
        self.input_builder = Some(builder);
        self
    }

    /// Includes the full JSON schema in the structured input. Only takes effect with
    /// [`Self::parameters`].
    pub const fn include_input_schema(mut self, include: bool) -> Self {
        self.include_input_schema = include;
        self
    }

    /// Receives the nested run's events as it runs; the nested agent then runs streamed.
    pub fn on_stream(mut self, handler: Arc<dyn AgentToolStreamHandler>) -> Self {
        self.on_stream = Some(handler);
        self
    }

    /// Caps the events waiting for the stream handler, not counting the one being handled.
    ///
    /// Defaults to [`DEFAULT_ON_STREAM_MAX_PENDING_EVENTS`]; `None` allows any backlog. When the
    /// backlog stays full after the handler has had a chance to catch up, the nested run and the
    /// handler are stopped and the call fails through the tool's failure handling. This bounds the
    /// number of pending events, not their size. Without [`Self::on_stream`] it has no effect.
    pub const fn on_stream_max_pending_events(mut self, limit: Option<usize>) -> Self {
        self.on_stream_max_pending_events = limit;
        self
    }

    /// Sets the remaining tool policies — exposure, concurrency, timeout, guardrails, permission
    /// scope.
    ///
    /// Availability, approval, and failure handling are decided by the dedicated setters above and
    /// override whatever these options say about them.
    pub fn options(mut self, options: ToolOptions) -> Self {
        self.options = options;
        self
    }

    /// Validates the declaration and builds the tool.
    pub fn build(self) -> Result<AgentTool> {
        let name = self
            .tool_name
            .unwrap_or_else(|| transform_string_function_style(self.agent.name()));
        if self.max_turns == Some(0) {
            return Err(Error::config(format!(
                "agent tool `{name}` sets max_turns to zero"
            )));
        }
        if self.on_stream_max_pending_events == Some(0) {
            return Err(Error::config(format!(
                "agent tool `{name}`: on_stream_max_pending_events must be a positive integer or None"
            )));
        }

        let typed = self.parameters.map(|build| build(&name)).transpose()?;
        let mut schema = match &typed {
            Some(typed) => typed.func_schema.tool_schema().clone(),
            None => ToolSchema::new(name.clone(), AgentAsToolInput::json_schema())?,
        };
        if let Some(description) = self.tool_description {
            schema = schema.with_description(description);
        }

        let include_schema = self.include_input_schema && typed.is_some();
        let capture_tool_input = typed.is_some() || include_schema || self.input_builder.is_some();
        let schema_info = StructuredInputSchemaInfo::from_params_schema(
            Some(schema.input_schema()),
            include_schema,
        );

        let availability = match (self.enabled, &self.enablement) {
            (_, Some(_)) => ToolAvailability::Dynamic,
            (Some(false), None) => ToolAvailability::Disabled,
            (Some(true), None) => ToolAvailability::Enabled,
            (None, None) => self.options.availability(),
        };
        let approval = match (self.needs_approval, &self.approval) {
            (_, Some(_)) => ToolApprovalPolicy::Dynamic,
            (Some(true), None) => ToolApprovalPolicy::Always,
            (Some(false), None) => ToolApprovalPolicy::Never,
            (None, None) => self.options.approval(),
        };
        let failure_handling = if self.error_function.is_some() {
            ToolFailureHandling::Custom
        } else if self.propagate_failures {
            ToolFailureHandling::Propagate
        } else {
            self.options.failure_handling()
        };
        let options = self
            .options
            .with_availability(availability)
            .with_approval(approval)
            .with_failure_handling(failure_handling);

        let tool = AgentTool {
            origin: ToolOrigin::new(name)?,
            schema,
            agent: self.agent,
            typed,
            options,
            schema_info,
            capture_tool_input,
            input_builder: self.input_builder,
            output_extractor: self.output_extractor,
            enablement: self.enablement,
            approval: self.approval,
            error_function: self.error_function,
            run_config: self.run_config,
            max_turns: self.max_turns,
            stream: self
                .on_stream
                .map(|handler| StreamForwarding::new(handler, self.on_stream_max_pending_events)),
        };
        tool.validate()?;
        Ok(tool)
    }
}

/// An agent exposed as a tool: each call runs the agent to completion and returns its answer.
pub struct AgentTool {
    agent: Arc<AgentSpec>,
    origin: ToolOrigin,
    schema: ToolSchema,
    typed: Option<TypedParameters>,
    options: ToolOptions,
    schema_info: StructuredInputSchemaInfo,
    capture_tool_input: bool,
    input_builder: Option<Arc<dyn StructuredToolInputBuilder>>,
    output_extractor: Option<Arc<dyn AgentToolOutputExtractor>>,
    enablement: Option<Arc<dyn AgentToolEnablement>>,
    approval: Option<Arc<dyn AgentToolApproval>>,
    error_function: Option<Arc<dyn AgentToolErrorFunction>>,
    run_config: Option<RunConfig>,
    max_turns: Option<u32>,
    stream: Option<StreamForwarding>,
}

impl AgentTool {
    /// Starts building a tool that runs `agent`.
    pub fn builder(agent: Arc<AgentSpec>) -> AgentToolBuilder {
        AgentToolBuilder {
            agent,
            tool_name: None,
            tool_description: None,
            output_extractor: None,
            enabled: None,
            enablement: None,
            run_config: None,
            max_turns: None,
            propagate_failures: false,
            error_function: None,
            needs_approval: None,
            approval: None,
            parameters: None,
            input_builder: None,
            include_input_schema: false,
            on_stream: None,
            on_stream_max_pending_events: Some(DEFAULT_ON_STREAM_MAX_PENDING_EVENTS),
            options: ToolOptions::default(),
        }
    }

    /// The agent each call runs.
    #[must_use]
    pub const fn agent(&self) -> &Arc<AgentSpec> {
        &self.agent
    }

    /// Validates the model's arguments and returns the parameter data the input is built from.
    fn params_data(&self, context: &mut ToolContext<'_>) -> Result<Value> {
        if let Some(typed) = &self.typed {
            return (typed.normalize)(context, &typed.func_schema);
        }
        let name = self.origin.qualified_name();
        let input = context
            .arguments()
            .as_object()
            .and_then(|arguments| arguments.get("input"))
            .and_then(Value::as_str)
            .ok_or_else(|| {
                Error::tool(
                    ToolErrorKind::InvalidInput,
                    name,
                    format!("Invalid JSON input for tool {name}: expected an object with a string `input`"),
                )
            })?;
        serde_json::to_value(AgentAsToolInput::new(input))
            .map_err(|error| Error::tool(ToolErrorKind::InvalidInput, name, error.to_string()))
    }

    async fn run_nested(&self, context: &mut ToolContext<'_>) -> Result<RunResult> {
        let params = self.params_data(context)?;
        let schema_info = self.capture_tool_input.then_some(&self.schema_info);
        let input = resolve_agent_tool_input(&params, schema_info, self.input_builder.as_deref())
            .await?
            .into_input_items();

        let name = self.origin.qualified_name();
        let parent = parent::ParentRun::current().ok_or_else(|| {
            Error::caller(format!(
                "agent tool `{name}` has no parent run to inherit a model resolver from; it must \
                 be dispatched by a runner"
            ))
        })?;
        let call_scope = parent.call_scope().ok_or_else(|| {
            Error::caller(format!(
                "agent tool `{name}` was invoked outside a dispatched tool call"
            ))
        })?;

        let config = self
            .run_config
            .clone()
            .unwrap_or_else(|| parent.config().clone())
            .with_max_turns(self.max_turns.unwrap_or(DEFAULT_MAX_TURNS));
        // A resumed call continues the nested run it paused on, from that run's checkpoint and
        // with its history, as the reference passes the saved state where a fresh call passes
        // input. Its parent was recorded when it first started.
        let resume = parent.nested_resume().cloned();
        let (run_id, input) = match &resume {
            Some(state) => (state.run_id().clone(), Vec::new()),
            None => (RunId::generate(), input),
        };
        // The agent a resumed run continues with may be one the nested run handed off to.
        let starting_agent = resume
            .as_ref()
            .and_then(RunState::current_agent)
            .unwrap_or_else(|| self.agent.id())
            .clone();
        let scope = call_scope.child(ScopeKind::Run);
        let mut request = RunRequest::new(
            AgentBinding::direct(Arc::clone(&self.agent)),
            Arc::clone(parent.model_resolver()),
            run_id,
            scope.clone(),
            input,
        )
        .with_config(config)
        .with_services(context.services().clone());
        request = match resume {
            Some(state) => request.with_state(state),
            None => request.with_parent_run_id(context.run().run_id().clone())?,
        };
        if let Some(app_context) = parent.app_context() {
            request = request.with_app_context(Arc::clone(app_context));
        }
        // Derived again on resume from the call's arguments, which the parent dispatches unchanged,
        // rather than kept in the checkpoint — the reference sets it on every invocation the same way.
        if self.capture_tool_input {
            request = request.with_tool_input(params);
        }

        let invocation = AgentToolInvocation::new(
            name.to_owned(),
            context.call_id().clone(),
            context.arguments().clone(),
        );
        let mut result = match &self.stream {
            Some(stream) => {
                Box::pin(stream.run(
                    request,
                    &scope,
                    starting_agent,
                    Arc::new(invocation.clone()),
                ))
                .await?
            }
            None => Box::pin(Runner::run(request)).await?,
        };
        result.set_agent_tool_invocation(invocation);
        Ok(result)
    }

    async fn output_of(&self, result: &RunResult) -> Result<String> {
        let name = self.origin.qualified_name();
        if !result.outcome().interruptions().is_empty() {
            return Err(Error::caller(format!(
                "agent tool `{name}`: the nested agent stopped to ask for approval"
            ))
            .with_source(NestedInterruption {
                state: Box::new(result.state().clone()),
            }));
        }
        // The reference raises when a nested run runs out of turns; this runner ends such a run
        // softly instead, so the stop is turned back into the failure the calling model is shown.
        if let Some(reason @ (FinishReason::MaxTurns | FinishReason::BudgetExhausted)) =
            result.outcome().finish_reason()
            && result.final_message().is_none()
        {
            return Err(Error::tool(
                ToolErrorKind::ExecutionFailed,
                name,
                format!("the nested agent stopped before concluding ({reason})"),
            ));
        }

        if let Some(extractor) = &self.output_extractor {
            return extractor.extract(result).await;
        }
        Ok(default_output(result))
    }
}

/// The reference's default output rule for a nested run.
///
/// The final answer when there is a non-empty one. An empty answer that output guardrails checked
/// is kept as it is — the checks approved exactly that — and otherwise the most recent non-empty
/// message or tool output this run produced stands in for it.
fn default_output(result: &RunResult) -> String {
    let final_text = result.final_text();
    if !final_text.is_empty() || !result.output_guardrail_results().is_empty() {
        return final_text;
    }
    for item in result.new_items().iter().rev() {
        match item.kind() {
            RunItemKind::Message(message) if message.role() == MessageRole::Assistant => {
                let text = message.text_content();
                if !text.is_empty() {
                    return text;
                }
            }
            RunItemKind::ToolCallOutput(output) => {
                if let Some(text) =
                    stored_output_text(output.output()).filter(|text| !text.is_empty())
                {
                    return text;
                }
            }
            _ => {}
        }
    }
    final_text
}

fn stored_output_text(payload: &Value) -> Option<String> {
    if let Value::String(text) = payload {
        return Some(text.clone());
    }
    ToolOutput::from_stored(payload)
        .ok()
        .flatten()
        .and_then(|output| output.as_text().map(str::to_owned))
}

fn typed_parameters<T: ToolInput + Serialize>(name: &str) -> Result<TypedParameters> {
    Ok(TypedParameters {
        func_schema: FuncSchema::for_input::<T>(name)?,
        normalize: normalize_typed::<T>,
    })
}

fn normalize_typed<T: ToolInput + Serialize>(
    context: &mut ToolContext<'_>,
    func_schema: &FuncSchema,
) -> Result<Value> {
    let decoded = match context.take_decoded_input::<T>()? {
        Some(decoded) => decoded,
        None => func_schema
            .decode_value(context.arguments().clone())?
            .downcast::<T>()?,
    };
    serde_json::to_value(&decoded).map_err(|error| {
        let name = context.origin().qualified_name();
        Error::tool(
            ToolErrorKind::InvalidInput,
            name,
            format!("Failed to serialize structured tool input for {name}: {error}"),
        )
    })
}

#[async_trait]
impl Tool for AgentTool {
    fn origin(&self) -> &ToolOrigin {
        &self.origin
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    fn func_schema(&self) -> Option<&FuncSchema> {
        self.typed.as_ref().map(|typed| &typed.func_schema)
    }

    async fn call(&self, mut context: ToolContext<'_>) -> Result<ToolOutput> {
        let result = self.run_nested(&mut context).await?;
        self.output_of(&result).await.map(ToolOutput::text)
    }

    fn options(&self) -> ToolOptions {
        self.options.clone()
    }

    async fn is_enabled(&self, context: &RunContext) -> Result<bool> {
        match &self.enablement {
            Some(enablement) => enablement.is_enabled(context, context.agent()).await,
            None => Ok(!matches!(
                self.options.availability(),
                ToolAvailability::Disabled
            )),
        }
    }

    async fn needs_approval(&self, context: &ToolContext<'_>) -> Result<bool> {
        match &self.approval {
            Some(approval) => approval.needs_approval(context).await,
            None => Ok(matches!(
                self.options.approval(),
                ToolApprovalPolicy::Always
            )),
        }
    }

    async fn handle_failure(
        &self,
        context: &ToolContext<'_>,
        error: &Error,
    ) -> Result<Option<ToolOutput>> {
        match &self.error_function {
            Some(error_function) => error_function
                .error_message(context, error)
                .await
                .map(|message| Some(ToolOutput::text(message))),
            None => Ok(None),
        }
    }
}

impl fmt::Debug for AgentTool {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentTool")
            .field("tool", &self.origin.qualified_name())
            .field("agent_id", self.agent.id())
            .field("structured", &self.typed.is_some())
            .field("capture_tool_input", &self.capture_tool_input)
            .field("inherits_run_config", &self.run_config.is_none())
            .field("max_turns", &self.max_turns)
            .field("stream", &self.stream)
            .finish_non_exhaustive()
    }
}

/// Carries a nested run that stopped on an approval out of [`Tool::call`].
///
/// An error is the only way out of `call` that is not an output, and an output is exactly what the
/// call must not produce yet. Dispatch recognises this source and turns the error into a paused
/// call, so no failure handling — model-visible or propagated — ever sees it.
#[derive(Debug)]
struct NestedInterruption {
    state: Box<RunState>,
}

impl fmt::Display for NestedInterruption {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("the nested agent run is waiting for approval")
    }
}

impl std::error::Error for NestedInterruption {}

/// The paused nested run `error` carries, when it reports a nested agent that stopped for approval.
pub(crate) fn nested_interruption(error: &Error) -> Option<&RunState> {
    std::error::Error::source(error)
        .and_then(<dyn std::error::Error>::downcast_ref::<NestedInterruption>)
        .map(|interruption| interruption.state.as_ref())
}
