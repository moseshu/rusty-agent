//! Immutable agent declarations and their builder.
//!
//! [`AgentSpec`] contains reusable configuration only. Run configuration, session state,
//! credentials, resolved provider objects, and execution-agent bindings belong to higher layers.
//! The type intentionally has no mutating methods and does not implement [`Clone`]: callers share
//! the value as an [`Arc`](std::sync::Arc), while intentional variants go through
//! [`AgentSpec::to_builder`].
//!
//! Several agent concerns have dedicated later milestones. Dynamic prompts, output schemas,
//! tool-use behavior, hooks, guardrails, capabilities, and handoffs must be added here only after
//! their own protocol-neutral contracts exist. Private fields and the non-exhaustive public types
//! let those additions remain source compatible; placeholder strings would freeze the wrong
//! identities and callback shapes.

use std::{collections::BTreeSet, fmt, sync::Arc};

use crate::{
    error::{Error, Result},
    model::ModelSettings,
    tool::Tool,
};

pub use crate::item::AgentId;

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
