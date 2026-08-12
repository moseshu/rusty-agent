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

use std::{collections::BTreeSet, fmt, sync::Arc};

use crate::{
    error::{Error, Result},
    item::{CallId, ToolCallOutput},
    model::ModelSettings,
    tool::{Tool, ToolOrigin},
};
use async_trait::async_trait;

pub use crate::item::AgentId;

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
/// whose [`is_complete`](crate::finish::FinishReason::is_complete) is true — telling R15 no closeout
/// is owed and R17-3 to take the success edge, over an error the model never even read.
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
    /// R13 adds MCP tools at run time — so validating here would reject names that are about to
    /// become real. The cost is that a misspelled name simply never matches.
    StopAtTools {
        /// Bare model-facing or qualified tool names that end the run.
        names: BTreeSet<String>,
    },
    /// Let application policy inspect every result before deciding.
    Custom(Arc<dyn ToolUseBehaviorHandler>),
}

impl ToolUseBehavior {
    /// Creates a name-matching stop policy.
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

/// Instructions attached to an agent declaration.
///
/// R3-1c supports static instructions. The private representation deliberately leaves room for
/// R4-11 to add a dynamic prompt source without changing [`AgentSpec::instructions`] or exposing
/// runtime context through the core type prematurely.
#[non_exhaustive]
#[derive(Clone)]
pub struct AgentInstructions {
    source: InstructionSource,
}

#[derive(Clone)]
enum InstructionSource {
    Static(String),
}

impl AgentInstructions {
    /// Creates static instructions.
    #[must_use]
    pub fn static_text(text: impl Into<String>) -> Self {
        Self {
            source: InstructionSource::Static(text.into()),
        }
    }

    /// Returns the static text, or `None` for a future non-static source.
    #[must_use]
    pub fn as_static(&self) -> Option<&str> {
        match &self.source {
            InstructionSource::Static(text) => Some(text),
        }
    }

    fn validate(&self) -> Result<()> {
        match &self.source {
            InstructionSource::Static(text) if text.is_empty() => {
                Err(Error::config("agent instructions must not be empty"))
            }
            InstructionSource::Static(_) => Ok(()),
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
