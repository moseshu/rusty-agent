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

use std::{collections::BTreeMap, fmt, sync::Arc};

use futures::future::try_join_all;
use ra_core::{
    agent::{AgentSpec, ResolvedInstructions},
    cancel::CancelScope,
    context::RunContext,
    error::{Error, Result},
    item::{AgentId, ModelInputItem},
    model::{
        Model, ModelHandoffDefinition, ModelOutputSchema, ModelRequest, ModelResolver,
        ModelSelector, ModelSettings, ModelToolDefinition, ModelTracing, ResolvedModelSettings,
    },
    prompt::{CachePlan, PromptProvenance},
    state::ToolUseTracker,
    tool::{Tool, ToolAvailability},
};

use crate::agent::AgentBinding;

/// What to do when two actions claim one model-facing tool name.
///
/// This is a turn-preparation policy rather than a registry policy: registry lookup keys remain
/// unambiguous, while tools and handoffs share one flat provider namespace. [`Warn`](Self::Warn)
/// retains a deterministic winner and records the discarded entry; [`Error`](Self::Error) refuses
/// the turn before a provider call.
#[non_exhaustive]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ToolNameCollisionPolicy {
    /// Keep the current dispatch winner and emit an actionable warning.
    #[default]
    Warn,
    /// Reject the ambiguous model-facing table before calling the provider.
    Error,
}

/// Inputs needed to prepare one model call.
///
/// The selected model and settings can be overridden for one run without mutating the agent.
/// Provider registration defaults and model defaults remain owned by [`ModelResolver`].
#[must_use]
#[non_exhaustive]
pub struct TurnPreparationRequest<'a> {
    agent: &'a AgentBinding,
    model_resolver: &'a dyn ModelResolver,
    run: &'a RunContext,
    cancel: &'a CancelScope,
    tool_use: &'a ToolUseTracker,
    input: Vec<ModelInputItem>,
    model_override: Option<String>,
    model_settings: ModelSettings,
    tracing: ModelTracing,
    collision_policy: ToolNameCollisionPolicy,
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
    ///
    /// `tool_use` is required for the same class of reason. It is what releases a forced
    /// `tool_choice` after the model has complied, and a caller allowed to omit it would have a
    /// run that forces the same call on every turn until the turn cap — silently, and only when a
    /// forced selection is configured.
    ///
    /// `run` is the same live context this turn's tools are later handed. Dynamic availability is
    /// the first stage that enters third-party code, and it has to read the run from the same place
    /// [`Tool::call`] will.
    pub fn new(
        agent: &'a AgentBinding,
        model_resolver: &'a dyn ModelResolver,
        run: &'a RunContext,
        cancel: &'a CancelScope,
        tool_use: &'a ToolUseTracker,
        input: Vec<ModelInputItem>,
    ) -> Self {
        Self {
            agent,
            model_resolver,
            run,
            cancel,
            tool_use,
            input,
            model_override: None,
            model_settings: ModelSettings::new(),
            tracing: ModelTracing::Disabled,
            collision_policy: ToolNameCollisionPolicy::Warn,
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

    /// Sets how collisions in the final model-facing action table are handled.
    pub const fn with_tool_name_collision_policy(
        mut self,
        collision_policy: ToolNameCollisionPolicy,
    ) -> Self {
        self.collision_policy = collision_policy;
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
    instruction_provenance: Option<PromptProvenance>,
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

    /// Identity of the generated prompt this turn used, or `None` for static instructions.
    ///
    /// The text itself is already inside the request; what this keeps is which generator produced
    /// it, what it hashed to, and the version and provenance it declared. Without the record, the
    /// only way to answer "what did the generator emit on turn 7" afterwards is to re-run the
    /// generator against a run context that no longer exists.
    #[must_use]
    pub const fn instruction_provenance(&self) -> Option<&PromptProvenance> {
        self.instruction_provenance.as_ref()
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
            .field("instruction_provenance", &self.instruction_provenance)
            .field("surface", &self.surface)
            .finish_non_exhaustive()
    }
}

/// What one turn advertised, retained across the model call.
///
/// The model answers with names, and settlement has to map each name back to the exact object the
/// turn offered. Keeping tools and handoffs together in one snapshot is what makes that mapping
/// total: a name resolves to a tool, to a handoff, or to nothing — never to two things at once.
/// [`ToolNameCollisionPolicy::Error`] rejects an overlap; [`ToolNameCollisionPolicy::Warn`]
/// removes every non-winning action before this snapshot is built.
#[non_exhaustive]
pub struct TurnActionSurface {
    tools: Vec<Arc<dyn Tool>>,
    tool_names: Vec<String>,
    handoffs: Vec<ModelHandoffDefinition>,
}

impl TurnActionSurface {
    /// Builds a snapshot, rejecting a surface that advertises one name twice.
    ///
    /// Use [`Self::new_with_collision_policy`] to retain a deterministic winner instead.
    pub fn new(tools: Vec<Arc<dyn Tool>>, handoffs: Vec<ModelHandoffDefinition>) -> Result<Self> {
        Self::new_with_collision_policy(tools, handoffs, ToolNameCollisionPolicy::Error)
    }

    /// Builds a snapshot under an explicit model-facing name-collision policy.
    pub fn new_with_collision_policy(
        mut tools: Vec<Arc<dyn Tool>>,
        mut handoffs: Vec<ModelHandoffDefinition>,
        policy: ToolNameCollisionPolicy,
    ) -> Result<Self> {
        let tool_names: Vec<String> = tools
            .iter()
            .map(|tool| tool.model_definition().name().to_owned())
            .collect();

        let mut owners: BTreeMap<&str, Vec<ActionOwner>> = BTreeMap::new();
        for (index, name) in tool_names.iter().enumerate() {
            owners
                .entry(name)
                .or_default()
                .push(ActionOwner::Tool(index));
        }
        for (index, handoff) in handoffs.iter().enumerate() {
            owners
                .entry(handoff.name())
                .or_default()
                .push(ActionOwner::Handoff(index));
        }

        let mut retained_tools = vec![true; tools.len()];
        let mut retained_handoffs = vec![true; handoffs.len()];
        for (name, entries) in owners {
            if entries.len() < 2 {
                continue;
            }
            match policy {
                ToolNameCollisionPolicy::Error => {
                    return Err(Error::config(format!(
                        "the turn advertises the name `{name}` more than once; a model call on it \
                         would be ambiguous"
                    )));
                }
                ToolNameCollisionPolicy::Warn => {}
            }

            // Handoffs own their wire name when present, matching response classification. Within
            // one kind, the last declaration wins. The filtered snapshot carries that exact
            // decision through both provider submission and response settlement.
            let Some(winner) = entries
                .iter()
                .rev()
                .find(|entry| matches!(entry, ActionOwner::Handoff(_)))
                .or_else(|| entries.last())
                .copied()
            else {
                continue;
            };
            tracing::warn!(
                tool_name = name,
                winner = winner.kind(),
                discarded = entries.len() - 1,
                "model-facing tool name collision; only the dispatch winner is advertised"
            );
            for entry in entries {
                if entry == winner {
                    continue;
                }
                match entry {
                    ActionOwner::Tool(index) => retained_tools[index] = false,
                    ActionOwner::Handoff(index) => retained_handoffs[index] = false,
                }
            }
        }

        let mut retained_tool_names = Vec::with_capacity(tools.len());
        tools = tools
            .into_iter()
            .zip(tool_names)
            .zip(retained_tools)
            .filter_map(|((tool, name), retained)| retained.then_some((tool, name)))
            .map(|(tool, name)| {
                retained_tool_names.push(name);
                tool
            })
            .collect();
        handoffs = handoffs
            .into_iter()
            .zip(retained_handoffs)
            .filter_map(|(handoff, retained)| retained.then_some(handoff))
            .collect();
        Ok(Self {
            tools,
            tool_names: retained_tool_names,
            handoffs,
        })
    }

    /// Executable tools this turn advertised.
    #[must_use]
    pub fn tools(&self) -> &[Arc<dyn Tool>] {
        &self.tools
    }

    /// Model-tool projections for exactly the executable tools in this snapshot.
    ///
    /// The provider request must be built from this rather than from the pre-policy inventory, or
    /// a warning-resolved collision could be filtered for settlement but still be sent to the
    /// provider as an ambiguous table.
    #[must_use]
    pub fn tool_definitions(&self) -> Vec<ModelToolDefinition> {
        self.tools
            .iter()
            .map(|tool| tool.model_definition())
            .collect()
    }

    /// Handoffs this turn advertised.
    #[must_use]
    pub fn handoffs(&self) -> &[ModelHandoffDefinition] {
        &self.handoffs
    }

    /// Resolves a model-facing name to its executable tool.
    #[must_use]
    pub fn find_tool(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        self.tools
            .iter()
            .zip(&self.tool_names)
            .rev()
            .find_map(|(tool, advertised)| (advertised == name).then_some(tool))
    }

    /// Resolves a model-facing name to its handoff definition.
    #[must_use]
    pub fn find_handoff(&self, name: &str) -> Option<&ModelHandoffDefinition> {
        self.handoffs
            .iter()
            .rev()
            .find(|handoff| handoff.name() == name)
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
        self.tool_names
            .iter()
            .map(String::as_str)
            .chain(self.handoffs.iter().map(ModelHandoffDefinition::name))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ActionOwner {
    Tool(usize),
    Handoff(usize),
}

impl ActionOwner {
    const fn kind(self) -> &'static str {
        match self {
            Self::Tool(_) => "tool",
            Self::Handoff(_) => "handoff",
        }
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
    // Execution-owned stages read the **execution** instance. Preparation decides which tools,
    // model, and settings the model receives, and taking those from the public agent would offer
    // capabilities the prepared instance may no longer run. The output schema is the deliberate
    // exception below: it is a public delivery promise, not an execution capability.
    let agent = request.agent.execution();

    // 1. Resolve dynamic availability first. Every later stage observes this exact snapshot.
    let tools = resolve_enabled_tools(agent, request.run, request.cancel).await?;

    // 2. Resolve enabled handoffs after tools. Sealing the two into one surface here — not lazily
    // at settlement — is what applies the collision policy before the model call is paid for.
    let handoffs = resolve_handoffs(agent);
    let surface = TurnActionSurface::new_with_collision_policy(
        tools.advertised,
        handoffs,
        request.collision_policy,
    )?;
    let tool_definitions = surface.tool_definitions();

    // 3. Project the public structured-output promise after handoffs. A prepared execution
    // instance may alter its tools or model, but it must not silently downgrade the output
    // contract callers configured and future closeout validation will enforce.
    let output_schema = resolve_output_schema(request.agent.public());

    // 4. Resolve the provider and model only after the advertised action surface is stable.
    request.cancel.ensure_not_cancelled()?;
    let model_name = request.model_override.as_deref().or(agent.model());
    let resolved_model = request.model_resolver.resolve_model(model_name)?;

    // 5. Complete provider -> agent -> model -> run settings resolution, then reconcile the result
    // against the surface stages 1 and 2 produced. This is the stage that makes the order load
    // bearing rather than merely conventional: `tool_choice` and `parallel_tool_calls` cannot be
    // settled before the turn knows which tools it will advertise.
    //
    // Releasing a forced selection belongs to the same stage and to the resolved value, never to
    // one of the four input layers: a layer carries the caller's standing intent, while this is a
    // fact about the turn just settled. The two must not be conflated, or complying once would
    // permanently overwrite what the agent asked for.
    let mut model_settings = resolved_model
        .resolve_settings(agent.model_settings(), &request.model_settings)
        .reconcile_tool_surface(surface.advertised_names());
    if request
        .tool_use
        .used_any_this_turn(request.agent.public_id())
    {
        model_settings = model_settings.reset_tool_choice();
    }

    // 6. Resolve instructions. A generated prompt reads the run, so it runs inside the cancel
    // scope like every other stage that enters third-party code.
    let instructions = resolve_instructions(
        agent,
        request.agent.public_id(),
        request.run,
        request.cancel,
    )
    .await?;

    let mut input = request.input;
    input.extend(instructions.tail_items);

    // 7. Model-input filters are always last, so a filter sees the final tool surface and the
    // settings that go with it. R10-6b will replace this identity implementation with the
    // report-producing filter chain without changing the surrounding stage order.
    let (input, instructions, instruction_provenance) = apply_model_input_filters(
        input,
        instructions.prefix,
        instructions.provenance,
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
        // The cache scope is the run for now. It wants to be the session id, so that a resumed or
        // follow-up run reuses the same cache entry; until sessions exist, the run is the widest
        // span this stage can name, and naming a narrower one would partition the cache rather
        // than share it.
        //
        // **An agent with generated instructions gets no plan at all**, because it has no stable
        // prefix and `CachePlan` is a plan for one. That is honest but leaves value on the table:
        // a cache scope is useful even without stable instructions, since the tool table and the
        // history ahead of the tail are still a prefix. Modelling a scope that does not name a
        // prefix is a change to the type, and it belongs with the rest of the session-scoped
        // caching work rather than here.
        //
        // The plan says only which bytes are stable and which calls should share an entry. Whether
        // that becomes `cache_control` marks, a `prompt_cache_key`, or nothing is the adapter's
        // decision, and depends on the endpoint rather than on the protocol.
        //
        // The plan is attached unconditionally, with no judgement about whether it is worth
        // acting on. That judgement needs the whole cached prefix — instructions plus the final
        // tool table, hosted tools included — and hosted tools have no protocol-neutral form: they
        // exist only after the adapter merges them into the wire request. Deciding here would mean
        // deciding on the instructions alone, which denies caching to precisely the requests that
        // most need it.
        let cache_plan = CachePlan::for_prefix(&instructions, Some(request.run.run_id().as_str()));
        model_request = model_request.with_cache_plan(cache_plan);
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
        instruction_provenance,
    })
}

struct EnabledTools {
    advertised: Vec<Arc<dyn Tool>>,
}

async fn resolve_enabled_tools(
    agent: &AgentSpec,
    context: &RunContext,
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

fn resolve_output_schema(public_agent: &AgentSpec) -> Option<ModelOutputSchema> {
    public_agent.output_schema().to_model_output_schema()
}

/// What the instruction stage contributed to one turn.
#[derive(Default)]
struct TurnInstructions {
    /// Text for the stable system-instruction slot. Only a static source can fill this.
    prefix: Option<String>,
    /// Volatile tail items, appended after the caller's input.
    tail_items: Vec<ModelInputItem>,
    /// Identity of a generated prompt, retained for the turn record.
    provenance: Option<PromptProvenance>,
}

/// Resolves the agent's instructions into a stable prefix and volatile tail items.
///
/// **A static source owns the prefix; a generated one may only reach the tail.** A generator reads
/// the live run, so its text is a function of the turn — the date, the budget, the run id. Placing
/// that in the prefix changes the cached span on every call, and the prefix is the span the whole
/// cache plan is built to hold still. The placement comes from [`ResolvedInstructions`] rather than
/// from a test this function performs, and lowering rejects a generator that asks for the prefix
/// rather than dropping the section, so the generator learns its text went nowhere instead of
/// quietly losing it.
///
/// The source is read from the execution instance but errors name `public_id`. A configuration
/// error has to point at the agent the user wrote down; naming a prepared clone would send them
/// looking for something that is not in their configuration at all.
async fn resolve_instructions(
    agent: &AgentSpec,
    public_id: &AgentId,
    context: &RunContext,
    cancel: &CancelScope,
) -> Result<TurnInstructions> {
    let Some(instructions) = agent.instructions() else {
        return Ok(TurnInstructions::default());
    };

    // Both failures below keep the original error's variant. Rewriting them into a configuration
    // error would report a cancelled run as needing human intervention and would make
    // `Error::is_cancelled` — the only cancellation test the contract allows — answer `false`.
    let resolved = cancel
        .run(instructions.resolve(context))
        .await?
        .map_err(|err| {
            err.with_context(format!(
                "resolving dynamic instructions for agent `{public_id}`"
            ))
        })?;

    match resolved {
        ResolvedInstructions::Prefix(text) => Ok(TurnInstructions {
            prefix: Some(text),
            ..TurnInstructions::default()
        }),
        ResolvedInstructions::Generated(prompt) => {
            let (tail_items, provenance) = prompt.lower().map_err(|err| {
                err.with_context(format!(
                    "lowering dynamic instructions for agent `{public_id}`"
                ))
            })?;
            Ok(TurnInstructions {
                prefix: None,
                tail_items,
                provenance: Some(provenance),
            })
        }
    }
}

/// Applies the model-input filter chain, currently an identity pass with no filters installed.
///
/// **The provenance record travels with the text it describes.** A filter can rewrite the input
/// items and the instructions, and the record names the exact bytes a generated prompt contributed
/// — so a chain that edits those bytes and leaves the record alone produces a turn whose only
/// evidence of what was sent describes something else. Passing the record through this signature
/// puts it in the implementer's hands rather than leaving the invariant to be rediscovered.
fn apply_model_input_filters(
    input: Vec<ModelInputItem>,
    instructions: Option<String>,
    provenance: Option<PromptProvenance>,
    _tools: &[Arc<dyn Tool>],
    _model_settings: &ResolvedModelSettings,
) -> (
    Vec<ModelInputItem>,
    Option<String>,
    Option<PromptProvenance>,
) {
    (input, instructions, provenance)
}
