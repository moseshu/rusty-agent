//! Fixed preparation pipeline executed before one model call.
//!
//! The order in [`prepare_turn`] is part of the loop contract, and two of the stages consume the
//! output of an earlier one rather than merely following it: model settings are reconciled against
//! the tool surface that dynamic availability produced, and the model-input filters run last so
//! they observe that same final surface. Handoffs, output schemas, and input filters have explicit
//! stage functions even while their owning milestones are pending, so their future implementations
//! have one insertion point rather than several call sites to reorder.
//!
//! Every stage that can block runs inside the caller's [`CancelScope`]: dynamic availability calls
//! third-party `async` code, which the cancellation contract does not allow to be awaited bare.

use std::{fmt, sync::Arc};

use futures::future::try_join_all;
use ra_core::{
    agent::AgentSpec,
    cancel::CancelScope,
    error::{Error, Result},
    item::ModelInputItem,
    model::{
        Model, ModelHandoffDefinition, ModelOutputSchema, ModelRequest, ModelResolver,
        ModelSelector, ModelSettings, ModelToolDefinition, ModelTracing, ResolvedModelSettings,
    },
    tool::{Tool, ToolAvailability, ToolRuntimeContext},
};

/// Inputs needed to prepare one model call.
///
/// The selected model and settings can be overridden for one run without mutating the agent.
/// Provider registration defaults and model defaults remain owned by [`ModelResolver`].
#[must_use]
#[non_exhaustive]
pub struct TurnPreparationRequest<'a> {
    agent: &'a AgentSpec,
    model_resolver: &'a dyn ModelResolver,
    tool_context: &'a dyn ToolRuntimeContext,
    cancel: &'a CancelScope,
    input: Vec<ModelInputItem>,
    model_override: Option<String>,
    model_settings: ModelSettings,
    tracing: ModelTracing,
}

impl<'a> TurnPreparationRequest<'a> {
    /// Creates a request using the agent's model selection and no run-level setting overrides.
    ///
    /// `cancel` is required rather than optional: preparation awaits third-party `is_enabled`
    /// implementations, so a caller that could omit the scope would have a run that cannot be
    /// interrupted while a tool decides whether it is available.
    pub fn new(
        agent: &'a AgentSpec,
        model_resolver: &'a dyn ModelResolver,
        tool_context: &'a dyn ToolRuntimeContext,
        cancel: &'a CancelScope,
        input: Vec<ModelInputItem>,
    ) -> Self {
        Self {
            agent,
            model_resolver,
            tool_context,
            cancel,
            input,
            model_override: None,
            model_settings: ModelSettings::new(),
            tracing: ModelTracing::Disabled,
        }
    }

    /// Overrides the agent's unresolved provider/model selector for this run.
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model_override = Some(model.into());
        self
    }

    /// Sets the run-override layer of the four-layer model-settings merge.
    pub fn with_model_settings(mut self, model_settings: ModelSettings) -> Self {
        self.model_settings = model_settings;
        self
    }

    /// Sets provider-side tracing visibility for this call.
    pub const fn with_tracing(mut self, tracing: ModelTracing) -> Self {
        self.tracing = tracing;
        self
    }
}

/// Fully prepared model call plus the executable tool bindings for turn settlement.
///
/// [`ModelRequest`] contains model-facing tool projections. `tools` retains the corresponding
/// executable objects; the two are produced from the same enabled-tool snapshot.
#[non_exhaustive]
pub struct PreparedTurn {
    selector: ModelSelector,
    model: Arc<dyn Model>,
    request: ModelRequest,
    tools: Vec<Arc<dyn Tool>>,
}

impl PreparedTurn {
    /// Canonical provider/model selection used by this turn.
    #[must_use]
    pub const fn selector(&self) -> &ModelSelector {
        &self.selector
    }

    /// Resolved model implementation.
    #[must_use]
    pub const fn model(&self) -> &Arc<dyn Model> {
        &self.model
    }

    /// Complete provider-neutral request ready for the model adapter.
    #[must_use]
    pub const fn request(&self) -> &ModelRequest {
        &self.request
    }

    /// Enabled executable tools matching the request's tool definitions.
    #[must_use]
    pub fn tools(&self) -> &[Arc<dyn Tool>] {
        &self.tools
    }

    /// Takes ownership of the request for the model call.
    ///
    /// [`Model::get_response`] takes the request by value, and it is the one part of a preparation
    /// that is expensive to copy — it holds the turn's whole input history. Read [`Self::model`],
    /// [`Self::selector`], and [`Self::tools`] first if settlement needs them; those are all
    /// reference-counted or short.
    #[must_use]
    pub fn into_request(self) -> ModelRequest {
        self.request
    }
}

impl fmt::Debug for PreparedTurn {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let tools = self
            .tools
            .iter()
            .map(|tool| tool.origin().qualified_name())
            .collect::<Vec<_>>();
        formatter
            .debug_struct("PreparedTurn")
            .field("selector", &self.selector)
            .field("input_items", &self.request.input().len())
            .field(
                "has_system_instructions",
                &self.request.system_instructions().is_some(),
            )
            .field("tools", &tools)
            .finish_non_exhaustive()
    }
}

/// Prepares one turn in the only permitted stage order.
pub async fn prepare_turn(request: TurnPreparationRequest<'_>) -> Result<PreparedTurn> {
    // 1. Resolve dynamic availability first. Every later stage observes this exact snapshot.
    let tools = resolve_enabled_tools(request.agent, request.tool_context, request.cancel).await?;
    let tool_definitions: Vec<ModelToolDefinition> =
        tools.iter().map(|tool| tool.model_definition()).collect();

    // 2. Resolve enabled handoffs after tools. R17 owns the concrete handoff contract.
    let handoffs = resolve_handoffs(request.agent);

    // 3. Resolve structured output after handoffs. R1-16 owns the output parser contract.
    let output_schema = resolve_output_schema(request.agent);

    // 4. Resolve the provider and model only after the advertised action surface is stable.
    request.cancel.ensure_not_cancelled()?;
    let model_name = request.model_override.as_deref().or(request.agent.model());
    let resolved_model = request.model_resolver.resolve_model(model_name)?;

    // 5. Complete provider -> agent -> model -> run settings resolution, then reconcile the result
    // against the surface stages 1 and 2 produced. This is the stage that makes the order load
    // bearing rather than merely conventional: `tool_choice` and `parallel_tool_calls` cannot be
    // settled before the turn knows which tools it will advertise.
    let model_settings = resolved_model
        .resolve_settings(request.agent.model_settings(), &request.model_settings)
        .reconcile_tool_surface(advertised_names(&tool_definitions, &handoffs));

    let instructions = resolve_instructions(request.agent)?;

    // 6. Model-input filters are always last, so a filter sees the final tool surface and the
    // settings that go with it. R10-6b will replace this identity implementation with the
    // report-producing filter chain without changing the surrounding stage order.
    let (input, instructions) =
        apply_model_input_filters(request.input, instructions, &tools, &model_settings);

    let selector = resolved_model.selector().clone();
    let model = Arc::clone(resolved_model.model());
    let mut model_request = ModelRequest::new(input, model_settings)
        .with_tools(tool_definitions)
        .with_handoffs(handoffs)
        .with_tracing(request.tracing);
    if let Some(instructions) = instructions {
        model_request = model_request.with_system_instructions(instructions);
    }
    if let Some(output_schema) = output_schema {
        model_request = model_request.with_output_schema(output_schema);
    }

    Ok(PreparedTurn {
        selector,
        model,
        request: model_request,
        tools,
    })
}

async fn resolve_enabled_tools(
    agent: &AgentSpec,
    context: &dyn ToolRuntimeContext,
    cancel: &CancelScope,
) -> Result<Vec<Arc<dyn Tool>>> {
    let decisions = cancel
        .run(try_join_all(agent.tools().iter().map(|tool| async move {
            match tool.options().availability() {
                ToolAvailability::Enabled => Ok(true),
                ToolAvailability::Disabled => Ok(false),
                ToolAvailability::Dynamic => tool.is_enabled(context).await,
                _ => Err(Error::caller(format!(
                    "tool `{}` uses an unsupported availability policy",
                    tool.origin().qualified_name()
                ))),
            }
        })))
        .await??;

    Ok(agent
        .tools()
        .iter()
        .zip(decisions)
        .filter(|(_, enabled)| *enabled)
        .map(|(tool, _)| Arc::clone(tool))
        .collect())
}

fn resolve_handoffs(_agent: &AgentSpec) -> Vec<ModelHandoffDefinition> {
    Vec::new()
}

fn resolve_output_schema(_agent: &AgentSpec) -> Option<ModelOutputSchema> {
    None
}

/// Names the turn advertises to the model. Handoffs share the tool namespace on the wire, so a
/// selector may legitimately point at either.
fn advertised_names<'a>(
    tools: &'a [ModelToolDefinition],
    handoffs: &'a [ModelHandoffDefinition],
) -> impl Iterator<Item = &'a str> {
    tools
        .iter()
        .map(ModelToolDefinition::name)
        .chain(handoffs.iter().map(ModelHandoffDefinition::name))
}

/// Projects the agent's instruction source onto the stable system-instruction slot.
///
/// The failure branch is the point: when R4-11 adds a dynamic prompt source, a turn that cannot
/// render it must say so rather than quietly send a request with no instructions at all.
fn resolve_instructions(agent: &AgentSpec) -> Result<Option<String>> {
    match agent.instructions() {
        None => Ok(None),
        Some(instructions) => instructions.as_static().map(str::to_owned).map(Some).ok_or_else(
            || {
                Error::config(format!(
                    "agent `{}` uses an instruction source that turn preparation cannot render yet",
                    agent.id()
                ))
            },
        ),
    }
}

fn apply_model_input_filters(
    input: Vec<ModelInputItem>,
    instructions: Option<String>,
    _tools: &[Arc<dyn Tool>],
    _model_settings: &ResolvedModelSettings,
) -> (Vec<ModelInputItem>, Option<String>) {
    (input, instructions)
}
