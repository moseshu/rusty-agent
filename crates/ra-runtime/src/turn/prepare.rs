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

use std::{collections::BTreeSet, fmt, sync::Arc};

use futures::future::try_join_all;
use ra_core::{
    agent::AgentSpec,
    cancel::CancelScope,
    error::{Error, Result},
    item::{AgentId, ModelInputItem},
    model::{
        Model, ModelHandoffDefinition, ModelOutputSchema, ModelRequest, ModelResolver,
        ModelSelector, ModelSettings, ModelToolDefinition, ModelTracing, ResolvedModelSettings,
    },
    tool::{Tool, ToolAvailability, ToolRuntimeContext},
};

use crate::agent::AgentBinding;

/// Inputs needed to prepare one model call.
///
/// The selected model and settings can be overridden for one run without mutating the agent.
/// Provider registration defaults and model defaults remain owned by [`ModelResolver`].
#[must_use]
#[non_exhaustive]
pub struct TurnPreparationRequest<'a> {
    agent: &'a AgentBinding,
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
    /// `agent` is a binding rather than a spec because preparation is the stage that decides what
    /// the model may call, and that has to come from the instance that will run — see
    /// [`AgentBinding`]. Passing the public agent when a prepared instance exists would advertise
    /// tools the executor does not have.
    ///
    /// `cancel` is required rather than optional: preparation awaits third-party `is_enabled`
    /// implementations, so a caller that could omit the scope would have a run that cannot be
    /// interrupted while a tool decides whether it is available.
    pub fn new(
        agent: &'a AgentBinding,
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

/// Fully prepared model call plus the executable bindings for turn settlement.
///
/// [`ModelRequest`] contains model-facing tool projections. [`TurnActionSurface`] retains the
/// corresponding executable objects; the two are produced from the same advertised-tool snapshot.
#[non_exhaustive]
pub struct PreparedTurn {
    selector: ModelSelector,
    model: Arc<dyn Model>,
    request: ModelRequest,
    surface: TurnActionSurface,
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
        self.surface.tools()
    }

    /// The action surface this turn advertises, for settlement to resolve names against.
    ///
    /// It is built and validated during preparation rather than here, so an ambiguous surface
    /// fails before the model call is paid for rather than at settlement afterwards.
    #[must_use]
    pub const fn action_surface(&self) -> &TurnActionSurface {
        &self.surface
    }

    /// Takes ownership of the surface and the request together.
    ///
    /// This is the exit settlement uses. The surface has to survive the model call — resolving the
    /// response's names against anything else would resolve them against the agent's *declared*
    /// tools rather than this turn's enabled snapshot, and a tool `is_enabled` turned off would
    /// become callable again.
    #[must_use]
    pub fn into_call(self) -> (TurnActionSurface, ModelRequest) {
        (self.surface, self.request)
    }

    /// Takes ownership of the request alone, for a caller that does not settle the turn.
    ///
    /// [`Model::get_response`] takes the request by value, and it is the one part of a preparation
    /// that is expensive to copy — it holds the turn's whole input history. Use [`Self::into_call`]
    /// instead whenever the response will be classified.
    #[must_use]
    pub fn into_request(self) -> ModelRequest {
        self.request
    }
}

impl fmt::Debug for PreparedTurn {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreparedTurn")
            .field("selector", &self.selector)
            .field("input_items", &self.request.input().len())
            .field(
                "has_system_instructions",
                &self.request.system_instructions().is_some(),
            )
            .field("surface", &self.surface)
            .finish_non_exhaustive()
    }
}

/// What one turn advertised, retained across the model call.
///
/// The model answers with names, and settlement has to map each name back to the exact object the
/// turn offered. Keeping tools and handoffs together in one snapshot is what makes that mapping
/// total: a name resolves to a tool, to a handoff, or to nothing — never to two things at once,
/// because construction rejects a surface where the two overlap.
#[non_exhaustive]
pub struct TurnActionSurface {
    tools: Vec<Arc<dyn Tool>>,
    handoffs: Vec<ModelHandoffDefinition>,
}

impl TurnActionSurface {
    /// Builds a snapshot, rejecting a surface that advertises one name twice.
    ///
    /// Handoffs share the tool namespace on the wire, so a duplicate is not a theoretical concern:
    /// it makes the model's call ambiguous, and any resolution order picked here would be an
    /// arbitrary one that silently favours one meaning over the other.
    pub fn new(tools: Vec<Arc<dyn Tool>>, handoffs: Vec<ModelHandoffDefinition>) -> Result<Self> {
        let mut names = BTreeSet::new();
        let advertised = tools
            .iter()
            .map(|tool| tool.origin().name())
            .chain(handoffs.iter().map(ModelHandoffDefinition::name));
        for name in advertised {
            if !names.insert(name) {
                return Err(Error::config(format!(
                    "the turn advertises the name `{name}` more than once; a model call on it \
                     would be ambiguous"
                )));
            }
        }
        Ok(Self { tools, handoffs })
    }

    /// Executable tools this turn advertised.
    #[must_use]
    pub fn tools(&self) -> &[Arc<dyn Tool>] {
        &self.tools
    }

    /// Handoffs this turn advertised.
    #[must_use]
    pub fn handoffs(&self) -> &[ModelHandoffDefinition] {
        &self.handoffs
    }

    /// Resolves a model-facing name to its executable tool.
    #[must_use]
    pub fn find_tool(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        self.tools.iter().find(|tool| tool.origin().name() == name)
    }

    /// Resolves a model-facing name to its handoff definition.
    #[must_use]
    pub fn find_handoff(&self, name: &str) -> Option<&ModelHandoffDefinition> {
        self.handoffs.iter().find(|handoff| handoff.name() == name)
    }

    /// Whether this turn offered a transfer to the given agent.
    #[must_use]
    pub fn advertises_handoff_to(&self, target: &AgentId) -> bool {
        self.handoffs
            .iter()
            .any(|handoff| handoff.target_agent() == target)
    }

    /// Every name this turn puts in front of the model.
    ///
    /// Settings reconciliation and response classification both ask this question, and they have to
    /// get the same answer: a `tool_choice` kept for a name the surface cannot resolve is a request
    /// that fails at the provider, while one dropped for a name it can resolve silently disables a
    /// forced call.
    pub fn advertised_names(&self) -> impl Iterator<Item = &str> {
        self.tools
            .iter()
            .map(|tool| tool.origin().name())
            .chain(self.handoffs.iter().map(ModelHandoffDefinition::name))
    }
}

impl fmt::Debug for TurnActionSurface {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let tools = self
            .tools
            .iter()
            .map(|tool| tool.origin().qualified_name())
            .collect::<Vec<_>>();
        let handoffs = self
            .handoffs
            .iter()
            .map(ModelHandoffDefinition::name)
            .collect::<Vec<_>>();
        formatter
            .debug_struct("TurnActionSurface")
            .field("tools", &tools)
            .field("handoffs", &handoffs)
            .finish_non_exhaustive()
    }
}

/// Prepares one turn in the only permitted stage order.
pub async fn prepare_turn(request: TurnPreparationRequest<'_>) -> Result<PreparedTurn> {
    // Every stage below reads the **execution** instance. Preparation decides what the model is
    // offered, and offering what the public agent declares would hand the model tools whichever
    // capability or sandbox step produced this instance may have removed.
    let agent = request.agent.execution();

    // 1. Resolve dynamic availability first. Every later stage observes this exact snapshot.
    let tools = resolve_enabled_tools(agent, request.tool_context, request.cancel).await?;
    let tool_definitions: Vec<ModelToolDefinition> = tools
        .advertised
        .iter()
        .map(|tool| tool.model_definition())
        .collect();

    // 2. Resolve enabled handoffs after tools. R17 owns the concrete handoff contract. Sealing the
    // two into one surface here — not lazily at settlement — is what makes an ambiguous surface
    // fail before the model call is paid for instead of after it.
    let handoffs = resolve_handoffs(agent);
    let surface = TurnActionSurface::new(tools.advertised, handoffs)?;

    // 3. Resolve structured output after handoffs. R1-16 owns the output parser contract.
    let output_schema = resolve_output_schema(agent);

    // 4. Resolve the provider and model only after the advertised action surface is stable.
    request.cancel.ensure_not_cancelled()?;
    let model_name = request.model_override.as_deref().or(agent.model());
    let resolved_model = request.model_resolver.resolve_model(model_name)?;

    // 5. Complete provider -> agent -> model -> run settings resolution, then reconcile the result
    // against the surface stages 1 and 2 produced. This is the stage that makes the order load
    // bearing rather than merely conventional: `tool_choice` and `parallel_tool_calls` cannot be
    // settled before the turn knows which tools it will advertise.
    let model_settings = resolved_model
        .resolve_settings(agent.model_settings(), &request.model_settings)
        .reconcile_tool_surface(surface.advertised_names());

    let instructions = resolve_instructions(agent, request.agent.public_id())?;

    // 6. Model-input filters are always last, so a filter sees the final tool surface and the
    // settings that go with it. R10-6b will replace this identity implementation with the
    // report-producing filter chain without changing the surrounding stage order.
    let (input, instructions) = apply_model_input_filters(
        request.input,
        instructions,
        surface.tools(),
        &model_settings,
    );

    let selector = resolved_model.selector().clone();
    let model = Arc::clone(resolved_model.model());
    let mut model_request = ModelRequest::new(input, model_settings)
        .with_tools(tool_definitions)
        .with_handoffs(surface.handoffs().to_vec())
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
        surface,
    })
}

struct EnabledTools {
    advertised: Vec<Arc<dyn Tool>>,
}

async fn resolve_enabled_tools(
    agent: &AgentSpec,
    context: &dyn ToolRuntimeContext,
    cancel: &CancelScope,
) -> Result<EnabledTools> {
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

    let mut advertised = Vec::new();
    for (tool, enabled) in agent.tools().iter().zip(decisions) {
        if !enabled {
            continue;
        }
        if tool.options().is_advertised() {
            advertised.push(Arc::clone(tool));
        } else if tool.options().is_discoverable() {
            // `ToolExposure::Deferred` is a promise that a model-visible `tool_search` call can
            // find the tool and promote it into a subsequent turn's advertised snapshot.  R2-5c
            // has not installed that state machine yet.  Treating the tool as merely omitted is
            // worse than rejecting it: it makes an enabled capability silently unreachable and
            // still lets the rest of the run look successful.
            return Err(Error::config(format!(
                "tool `{}` uses Deferred exposure, but deferred discovery (R2-5c) is not \
                 implemented; use Advertised or Hidden",
                tool.origin().qualified_name()
            )));
        }
        // Hidden tools remain registered for host/programmatic dispatch but are deliberately
        // absent from every model-owned surface.
    }

    Ok(EnabledTools { advertised })
}

fn resolve_handoffs(_agent: &AgentSpec) -> Vec<ModelHandoffDefinition> {
    Vec::new()
}

fn resolve_output_schema(_agent: &AgentSpec) -> Option<ModelOutputSchema> {
    None
}

/// Projects the agent's instruction source onto the stable system-instruction slot.
///
/// The failure branch is the point: when R4-11 adds a dynamic prompt source, a turn that cannot
/// render it must say so rather than quietly send a request with no instructions at all.
///
/// The source is read from the execution instance but the error names `public_id`. A configuration
/// error has to point at the agent the user wrote down; naming a prepared clone would send them
/// looking for something that is not in their configuration at all.
fn resolve_instructions(agent: &AgentSpec, public_id: &AgentId) -> Result<Option<String>> {
    match agent.instructions() {
        None => Ok(None),
        Some(instructions) => instructions
            .as_static()
            .map(str::to_owned)
            .map(Some)
            .ok_or_else(|| {
                Error::config(format!(
                    "agent `{public_id}` uses an instruction source that turn preparation cannot \
                     render yet"
                ))
            }),
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
