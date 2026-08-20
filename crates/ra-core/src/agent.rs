//! Immutable agent declarations and their builder.
//!
//! [`AgentSpec`] contains reusable configuration only. Run configuration, session state,
//! credentials, resolved provider objects, and execution-agent bindings belong to higher layers.
//! The type intentionally has no mutating methods and does not implement [`Clone`]: callers share
//! the value as an [`Arc`](std::sync::Arc), while intentional variants go through
//! [`AgentSpec::to_builder`].
//!
//! Several agent concerns have dedicated later milestones. Dynamic prompts, output schemas, hooks,
//! guardrails, capabilities, and handoffs must be added here only after their own protocol-neutral
//! contracts exist. Private fields and the non-exhaustive public types let those additions remain
//! source compatible; placeholder strings would freeze the wrong identities and callback shapes.

use std::{collections::BTreeSet, fmt, future::Future, sync::Arc};

use crate::{
    context::RunContext,
    error::{Error, Result},
    item::{CallId, ToolCallOutput},
    model::ModelSettings,
    prompt::{DynamicPromptHandler, ResolvedPrompt},
    tool::{Tool, ToolOrigin},
};
use async_trait::async_trait;

pub use crate::item::AgentId;

struct DynamicPromptFn<F>(F);

#[async_trait]
impl<F, Fut> DynamicPromptHandler for DynamicPromptFn<F>
where
    F: Fn(&RunContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<ResolvedPrompt>> + Send + 'static,
{
    async fn resolve(&self, context: &RunContext) -> Result<ResolvedPrompt> {
        (self.0)(context).await
    }
}

/// One function-tool result that a [`ToolUseBehavior`] may inspect.
///
/// These values exist only for the duration of a turn, and they are the settled, model-order view
/// of the calls that **ran and produced a value**. Everything a stop policy could otherwise mistake
/// for an answer is excluded at the source: a request awaiting approval, a name the turn never
/// advertised, a propagating failure that became the turn's error, and — the two that are easy to
/// miss — a call the dispatch chain refused before it ran, and a call that ran and failed.
///
/// That last exclusion is the invariant every policy here depends on: `StopOnFirstTool` and
/// `StopAtTools` have no way to inspect what they are stopping on, so a run that ended because a
/// tool *failed* would report [`FinishReason::ToolStop`](crate::finish::FinishReason::ToolStop),
/// whose [`is_complete`](crate::finish::FinishReason::is_complete) is true — telling anything
/// downstream that watches for it that no closeout is owed and the run reached its own success
/// edge, over an error the model never even read.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct ToolUseResult {
    tool: ToolOrigin,
    output: ToolCallOutput,
}

impl ToolUseResult {
    /// Creates the result for one successful function-tool call.
    #[must_use]
    pub fn new(tool: ToolOrigin, output: ToolCallOutput) -> Self {
        Self { tool, output }
    }

    /// ID of the call this result answers.
    #[must_use]
    pub const fn call_id(&self) -> &CallId {
        self.output.call_id()
    }

    /// Stable identity of the tool that produced the result.
    #[must_use]
    pub const fn tool(&self) -> &ToolOrigin {
        &self.tool
    }

    /// Model-visible output observed for the call.
    #[must_use]
    pub const fn output(&self) -> &ToolCallOutput {
        &self.output
    }
}

/// Decides whether a response's function-tool results end the current run.
///
/// The callback receives every [`ToolUseResult`] the response produced, in model order, and is
/// asked only when there is at least one. Returning `true` stops with
/// [`FinishReason::ToolStop`](crate::finish::FinishReason::ToolStop); returning `false` asks the
/// model for its next response. A returned error is propagated rather than silently changing the
/// policy to one of those outcomes.
///
/// # Cancellation
///
/// This is third-party `async` code, so the runtime awaits it inside the turn's cancellation scope
/// rather than bare. On cancellation the returned future is **dropped**, which is safe for a pure
/// future: an implementation that spawns a task or a child process owns draining it, exactly as a
/// [`Tool`] does.
#[async_trait]
pub trait ToolUseBehaviorHandler: Send + Sync + 'static {
    /// Returns whether this batch of tool results should end the run.
    async fn should_stop(&self, tool_results: &[ToolUseResult]) -> Result<bool>;
}

#[async_trait]
impl<F> ToolUseBehaviorHandler for F
where
    F: Fn(&[ToolUseResult]) -> Result<bool> + Send + Sync + 'static,
{
    async fn should_stop(&self, tool_results: &[ToolUseResult]) -> Result<bool> {
        self(tool_results)
    }
}

/// The policy applied after a response's function tools have settled.
///
/// The default is [`Self::RunLlmAgain`], preserving the ordinary tool-call loop. Every policy reads
/// [`ToolUseResult`]s and nothing else, so an approval stays an interruption and a propagated
/// failure stays the turn error that produced it.
#[non_exhaustive]
#[derive(Clone, Default)]
pub enum ToolUseBehavior {
    /// Send tool results back to the model for another response.
    #[default]
    RunLlmAgain,
    /// Stop once this response produced at least one [`ToolUseResult`].
    StopOnFirstTool,
    /// Stop once any [`ToolUseResult`] came from a listed tool.
    ///
    /// Entries may be either the bare model-facing tool name or the tool's qualified name.
    ///
    /// **Names are not checked against the agent's tools**, deliberately: the turn's action surface
    /// is not knowable when the declaration is built — dynamic availability narrows it per turn and
    /// a future milestone adds MCP tools at run time — so validating here would reject names that
    /// are about to become real. The cost is that a misspelled name simply never matches.
    StopAtTools {
        /// Bare model-facing or qualified tool names that end the run.
        names: BTreeSet<String>,
    },
    /// Delegate the stopping decision to custom asynchronous logic.
    Custom(Arc<dyn ToolUseBehaviorHandler>),
}

impl ToolUseBehavior {
    /// Creates a stop policy that matches any of the designated tool names.
    #[must_use]
    pub fn stop_at_tools(names: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self::StopAtTools {
            names: names.into_iter().map(Into::into).collect(),
        }
    }

    /// Creates a custom asynchronous policy.
    #[must_use]
    pub fn custom(handler: Arc<dyn ToolUseBehaviorHandler>) -> Self {
        Self::Custom(handler)
    }
}

impl fmt::Debug for ToolUseBehavior {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RunLlmAgain => formatter.write_str("ToolUseBehavior::RunLlmAgain"),
            Self::StopOnFirstTool => formatter.write_str("ToolUseBehavior::StopOnFirstTool"),
            Self::StopAtTools { names } => formatter
                .debug_struct("ToolUseBehavior::StopAtTools")
                .field("names", names)
                .finish(),
            Self::Custom(_) => formatter.write_str("ToolUseBehavior::Custom(..)"),
        }
    }
}

/// What an agent's instructions resolved to, tagged with the one placement each may occupy.
///
/// The tag is the whole point of the type. A [`ResolvedPrompt`] lowers to volatile tail messages,
/// while static agent text belongs in the cached prefix; returning one type for both made the
/// wrong placement a plain function call away — resolving a static agent and lowering the result
/// put the entire system constitution into a user message, with nothing about the value saying it
/// had ever been anything else. Every value of this enum names where it goes, so a caller that
/// ignores the distinction no longer compiles.
///
/// Deliberately **not** `#[non_exhaustive]`, unlike most public types here. A third placement would
/// not be an additive detail a consumer may ignore — it would be a new position in the model
/// request, and a `_` arm silently sending it to whichever slot the arm happened to pick is the
/// failure this type exists to prevent. Making that a breaking change is the honest encoding.
#[derive(Clone, Debug)]
pub enum ResolvedInstructions {
    /// Static text, bound for the stable system-instruction prefix.
    Prefix(String),
    /// Generator output, bound for volatile tail messages.
    Generated(ResolvedPrompt),
}

/// Instructions attached to an agent declaration.
///
/// Supports both static instruction text (for the stable prefix) and dynamic prompt generators
/// that evaluate against runtime context.
#[non_exhaustive]
#[derive(Clone)]
pub struct AgentInstructions {
    source: InstructionSource,
}

#[derive(Clone)]
enum InstructionSource {
    Static(String),
    Dynamic(Arc<dyn DynamicPromptHandler>),
}

impl AgentInstructions {
    /// Creates static instructions.
    #[must_use]
    pub fn static_text(text: impl Into<String>) -> Self {
        Self {
            source: InstructionSource::Static(text.into()),
        }
    }

    /// Creates dynamic instructions evaluated via a handler.
    #[must_use]
    pub fn dynamic(handler: Arc<dyn DynamicPromptHandler>) -> Self {
        Self {
            source: InstructionSource::Dynamic(handler),
        }
    }

    /// Creates dynamic instructions from an asynchronous function.
    pub fn dynamic_fn<F, Fut>(f: F) -> Self
    where
        F: Fn(&RunContext) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<ResolvedPrompt>> + Send + 'static,
    {
        Self::dynamic(Arc::new(DynamicPromptFn(f)))
    }

    /// Returns the static text, or `None` for a dynamic source.
    #[must_use]
    pub fn as_static(&self) -> Option<&str> {
        match &self.source {
            InstructionSource::Static(text) => Some(text),
            InstructionSource::Dynamic(_) => None,
        }
    }

    /// Returns whether these instructions are generated dynamically.
    #[must_use]
    pub const fn is_dynamic(&self) -> bool {
        matches!(&self.source, InstructionSource::Dynamic(_))
    }

    /// Resolves the instructions against the current run context, tagged with their placement.
    ///
    /// # Errors
    ///
    /// Propagates whatever a dynamic generator returns, unchanged.
    pub async fn resolve(&self, context: &RunContext) -> Result<ResolvedInstructions> {
        match &self.source {
            InstructionSource::Static(text) => Ok(ResolvedInstructions::Prefix(text.clone())),
            InstructionSource::Dynamic(handler) => handler
                .resolve(context)
                .await
                .map(ResolvedInstructions::Generated),
        }
    }

    fn validate(&self) -> Result<()> {
        match &self.source {
            InstructionSource::Static(text) if text.is_empty() => {
                Err(Error::config("agent instructions must not be empty"))
            }
            InstructionSource::Static(_) | InstructionSource::Dynamic(_) => Ok(()),
        }
    }
}

impl fmt::Debug for AgentInstructions {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.source {
            InstructionSource::Static(text) => formatter
                .debug_struct("AgentInstructions")
                .field("kind", &"static")
                .field("bytes", &text.len())
                .finish_non_exhaustive(),
            InstructionSource::Dynamic(_) => formatter
                .debug_struct("AgentInstructions")
                .field("kind", &"dynamic")
                .finish_non_exhaustive(),
        }
    }
}

/// A reusable, immutable agent declaration.
///
/// `id` is the identity used for routing, provenance, and future public/execution-agent bindings.
/// `name` is display metadata and is deliberately allowed to collide. `model` is the unresolved
/// selector accepted by the provider registry; provider resolution and the remaining settings
/// layers happen during turn preparation rather than at construction time.
#[non_exhaustive]
pub struct AgentSpec {
    id: AgentId,
    name: String,
    instructions: Option<AgentInstructions>,
    model: Option<String>,
    model_settings: ModelSettings,
    tools: Vec<Arc<dyn Tool>>,
    tool_use_behavior: ToolUseBehavior,
}

impl AgentSpec {
    /// Starts an empty builder.
    pub fn builder() -> AgentSpecBuilder {
        AgentSpecBuilder::new()
    }

    /// Starts a builder initialized from this declaration.
    ///
    /// Tool implementations remain shared through `Arc`; this is the explicit path for deriving
    /// a configuration variant without introducing mutable runtime configuration.
    pub fn to_builder(&self) -> AgentSpecBuilder {
        AgentSpecBuilder {
            id: Some(self.id.clone()),
            name: Some(self.name.clone()),
            instructions: self.instructions.clone(),
            model: self.model.clone(),
            model_settings: self.model_settings.clone(),
            tools: self.tools.clone(),
            tool_use_behavior: self.tool_use_behavior.clone(),
        }
    }

    /// Stable agent identity.
    #[must_use]
    pub const fn id(&self) -> &AgentId {
        &self.id
    }

    /// Display name. It is not an identity or lookup key.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Configured instruction source.
    #[must_use]
    pub const fn instructions(&self) -> Option<&AgentInstructions> {
        self.instructions.as_ref()
    }

    /// Unresolved provider/model selector, or `None` for registry defaults.
    #[must_use]
    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    /// Agent layer of the four-layer model-settings merge.
    #[must_use]
    pub const fn model_settings(&self) -> &ModelSettings {
        &self.model_settings
    }

    /// Tools declared directly by this agent.
    #[must_use]
    pub fn tools(&self) -> &[Arc<dyn Tool>] {
        &self.tools
    }

    /// Policy applied after this agent's function tools produce observations.
    #[must_use]
    pub const fn tool_use_behavior(&self) -> &ToolUseBehavior {
        &self.tool_use_behavior
    }
}

impl fmt::Debug for AgentSpec {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let tools = self
            .tools
            .iter()
            .map(|tool| tool.origin().qualified_name())
            .collect::<Vec<_>>();
        formatter
            .debug_struct("AgentSpec")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("instructions", &self.instructions)
            .field("model", &self.model)
            .field("tools", &tools)
            .field("tool_use_behavior", &self.tool_use_behavior)
            .finish_non_exhaustive()
    }
}

/// Builder for an immutable, shared [`AgentSpec`].
#[must_use]
pub struct AgentSpecBuilder {
    id: Option<AgentId>,
    name: Option<String>,
    instructions: Option<AgentInstructions>,
    model: Option<String>,
    model_settings: ModelSettings,
    tools: Vec<Arc<dyn Tool>>,
    tool_use_behavior: ToolUseBehavior,
}

impl AgentSpecBuilder {
    /// Creates an empty builder.
    pub fn new() -> Self {
        Self {
            id: None,
            name: None,
            instructions: None,
            model: None,
            model_settings: ModelSettings::new(),
            tools: Vec::new(),
            tool_use_behavior: ToolUseBehavior::default(),
        }
    }

    /// Sets the stable identity.
    pub fn id(mut self, id: AgentId) -> Self {
        self.id = Some(id);
        self
    }

    /// Sets the display name.
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// Sets static instructions.
    pub fn instructions(mut self, instructions: impl Into<String>) -> Self {
        self.instructions = Some(AgentInstructions::static_text(instructions));
        self
    }

    /// Sets dynamic instructions evaluated via a handler.
    pub fn dynamic_instructions(mut self, handler: Arc<dyn DynamicPromptHandler>) -> Self {
        self.instructions = Some(AgentInstructions::dynamic(handler));
        self
    }

    /// Sets dynamic instructions from an asynchronous function.
    pub fn dynamic_instructions_fn<F, Fut>(mut self, f: F) -> Self
    where
        F: Fn(&RunContext) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<ResolvedPrompt>> + Send + 'static,
    {
        self.instructions = Some(AgentInstructions::dynamic_fn(f));
        self
    }

    /// Sets pre-constructed instructions.
    pub fn with_instructions(mut self, instructions: AgentInstructions) -> Self {
        self.instructions = Some(instructions);
        self
    }

    /// Removes instructions inherited through [`AgentSpec::to_builder`].
    pub fn clear_instructions(mut self) -> Self {
        self.instructions = None;
        self
    }

    /// Sets the unresolved provider/model selector.
    pub fn model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    /// Uses the provider registry's default model selection.
    pub fn clear_model(mut self) -> Self {
        self.model = None;
        self
    }

    /// Replaces the agent layer of model settings.
    pub fn model_settings(mut self, model_settings: ModelSettings) -> Self {
        self.model_settings = model_settings;
        self
    }

    /// Sets the policy applied after this agent's function tools produce observations.
    pub fn tool_use_behavior(mut self, tool_use_behavior: ToolUseBehavior) -> Self {
        self.tool_use_behavior = tool_use_behavior;
        self
    }

    /// Adds one directly declared tool.
    pub fn tool(mut self, tool: Arc<dyn Tool>) -> Self {
        self.tools.push(tool);
        self
    }

    /// Adds directly declared tools in iteration order.
    pub fn tools(mut self, tools: impl IntoIterator<Item = Arc<dyn Tool>>) -> Self {
        self.tools.extend(tools);
        self
    }

    /// Removes tools inherited through [`AgentSpec::to_builder`].
    pub fn clear_tools(mut self) -> Self {
        self.tools.clear();
        self
    }

    /// Validates the declaration and returns its shared immutable form.
    pub fn build(self) -> Result<Arc<AgentSpec>> {
        let id = self
            .id
            .ok_or_else(|| Error::config("agent spec requires a stable `id`"))?;
        let name = self
            .name
            .ok_or_else(|| Error::config("agent spec requires a display `name`"))?;

        validate_required_text("agent id", id.as_str())?;
        validate_required_text("agent name", &name)?;
        if let Some(instructions) = &self.instructions {
            instructions.validate()?;
        }
        if let Some(model) = &self.model {
            validate_required_text("agent model selector", model)?;
        }

        // Two identities have to be unique, and neither implies the other. The lookup key is how
        // dispatch finds the executable object; the model-facing name is what the turn advertises.
        // A namespace separates two lookup keys without separating the names they project to, so
        // two servers that both expose `search` would build fine here and then fail on every model
        // call instead.
        //
        // **The two checks cover different sets, and that is the point.** Every declared tool
        // needs a distinct lookup key, because dispatch has to find it whoever called. Only the
        // tools that can reach a model surface need a distinct name, because a name is only ever
        // ambiguous inside one tool list — a host-only tool named `search` beside an
        // integration's `search` is an ordinary installation, and rejecting it here would refuse a
        // configuration no provider would ever see.
        let mut tool_keys = BTreeSet::new();
        let mut advertised_names = BTreeSet::new();
        for tool in &self.tools {
            tool.validate()?;
            let key = tool.origin().lookup_key();
            if !tool_keys.insert(key.clone()) {
                return Err(Error::config(format!(
                    "agent `{id}` declares tool lookup key `{key:?}` more than once"
                )));
            }
            if !tool.options().can_reach_model_surface() {
                continue;
            }
            let advertised = tool.model_definition().name().to_owned();
            if !advertised_names.insert(advertised.clone()) {
                return Err(Error::config(format!(
                    "agent `{id}` advertises the tool name `{advertised}` more than once; \
                     distinct lookup keys still have to project to distinct model-facing names"
                )));
            }
        }

        Ok(Arc::new(AgentSpec {
            id,
            name,
            instructions: self.instructions,
            model: self.model,
            model_settings: self.model_settings,
            tools: self.tools,
            tool_use_behavior: self.tool_use_behavior,
        }))
    }
}

impl Default for AgentSpecBuilder {
    fn default() -> Self {
        Self::new()
    }
}

fn validate_required_text(label: &str, value: &str) -> Result<()> {
    if value.is_empty() || value.trim() != value || value.chars().any(char::is_control) {
        return Err(Error::config(format!(
            "{label} must be non-empty, trimmed, and contain no control characters"
        )));
    }
    Ok(())
}
