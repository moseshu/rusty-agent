//! Everything the host installed, and the assembly step that decides what one agent advertises.
//!
//! **Advertising is not owning.** The registry holds every tool the host can dispatch, including
//! the ones no model will ever be shown: tools reserved for host or programmatic callers, tools
//! withheld until discovery surfaces them, tools currently switched off. Which of them reach a
//! model is a separate question, asked once per agent by
//! [`assemble`](ToolRegistry::assemble) against a [`ToolProfile`], and the answer is a
//! [`ToolSurface`] rather than a mutation of anything.
//!
//! Splitting it that way is what makes a small tool surface affordable. A capability does not have
//! to be dropped to be kept out of a turn's schema budget — it stays registered and executable, and
//! only its advertisement is rationed.
//!
//! # The one identity the registry enforces, and the one it does not
//!
//! Registration is keyed by [`ToolLookupKey`], and two tools cannot share one. It deliberately
//! does *not* require model-facing names to be unique: two MCP servers each exporting `search` is
//! a normal thing for a host to install, and the lookup keys keep them apart. That collision is
//! real only for a surface that advertises both, so it is caught during assembly, where the error
//! can name the profile and both keys — see [`ToolRegistry::assemble`].

use std::{collections::BTreeMap, fmt, sync::Arc};

use ra_core::{
    error::{Error, Result},
    tool::{Tool, ToolLookupKey},
};

use super::profile::{ToolProfile, ToolSelection, ToolSurface, is_advertised};

/// Every tool a host can dispatch, keyed by routing identity.
///
/// Cloning shares the tools rather than copying them; the map itself is small.
#[non_exhaustive]
#[derive(Clone, Default)]
pub struct ToolRegistry {
    entries: BTreeMap<ToolLookupKey, Arc<dyn Tool>>,
}

impl ToolRegistry {
    /// Starts an empty builder.
    pub fn builder() -> ToolRegistryBuilder {
        ToolRegistryBuilder {
            entries: Vec::new(),
        }
    }

    /// Finds a tool by routing identity.
    #[must_use]
    pub fn get(&self, key: &ToolLookupKey) -> Option<&Arc<dyn Tool>> {
        self.entries.get(key)
    }

    /// Whether a tool with this routing identity is registered.
    #[must_use]
    pub fn contains(&self, key: &ToolLookupKey) -> bool {
        self.entries.contains_key(key)
    }

    /// How many tools are registered, advertised or not.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether nothing is registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Every routing identity, in sorted order.
    pub fn keys(&self) -> impl Iterator<Item = &ToolLookupKey> {
        self.entries.keys()
    }

    /// Every registered tool, in lookup-key order.
    pub fn tools(&self) -> impl Iterator<Item = &Arc<dyn Tool>> {
        self.entries.values()
    }

    /// Builds one agent's tool surface from this registry.
    ///
    /// Four things can go wrong, and all four are configuration mistakes that are cheaper to hear
    /// about here than one model call later:
    ///
    /// - the profile names a tool the registry does not hold;
    /// - two selected tools project to the same model-facing name;
    /// - the advertised entries fall outside the profile's declared count;
    /// - they cost more than the profile's declared bytes.
    ///
    /// The name check covers the entries that can reach a model surface, not every selected tool:
    /// a name is only ambiguous inside one tool list, and a host-only tool never appears in one.
    /// It is the same rule [`AgentSpec`](ra_core::agent::AgentSpec) applies to the tools it is
    /// handed, restated here only because this is the point that can name the profile and both
    /// colliding lookup keys.
    pub fn assemble(&self, profile: &ToolProfile) -> Result<ToolSurface> {
        let id = profile.id();
        let tools: Vec<Arc<dyn Tool>> = match profile.selection() {
            ToolSelection::Explicit(keys) => {
                let mut selected = Vec::with_capacity(keys.len());
                for key in keys {
                    let tool = self.entries.get(key).ok_or_else(|| {
                        Error::config(format!(
                            "tool profile `{id}` selects lookup key `{key:?}`, which no registered \
                             tool provides"
                        ))
                    })?;
                    selected.push(Arc::clone(tool));
                }
                selected
            }
            ToolSelection::AllRegistered => self.entries.values().map(Arc::clone).collect(),
        };

        let mut names: BTreeMap<&str, &ToolLookupKey> = BTreeMap::new();
        let mut advertised_count = 0;
        let mut advertised_bytes = 0;
        for tool in &tools {
            if tool.options().can_reach_model_surface() {
                let key = tool.origin().lookup_key();
                let name = tool.schema().name();
                if let Some(previous) = names.insert(name, key) {
                    return Err(Error::config(format!(
                        "tool profile `{id}` advertises the name `{name}` from two lookup keys \
                         `{previous:?}` and `{key:?}`; distinct routing identities still have to \
                         project to distinct model-facing names"
                    )));
                }
            }
            if is_advertised(tool.as_ref()) {
                advertised_count += 1;
                advertised_bytes += tool.schema().advertised_bytes()?;
            }
        }

        let budget = profile.budget();
        if advertised_count < budget.min_advertised() {
            return Err(Error::config(format!(
                "tool profile `{id}` advertises {advertised_count} entries, below its declared \
                 floor of {}",
                budget.min_advertised()
            )));
        }
        if advertised_count > budget.max_advertised() {
            return Err(Error::config(format!(
                "tool profile `{id}` advertises {advertised_count} entries, above its declared \
                 ceiling of {}",
                budget.max_advertised()
            )));
        }
        if let Some(max_bytes) = budget.max_advertised_bytes()
            && advertised_bytes > max_bytes
        {
            return Err(Error::config(format!(
                "tool profile `{id}` advertises {advertised_bytes} bytes of tool schema, above \
                 its declared ceiling of {max_bytes}"
            )));
        }

        Ok(ToolSurface::new(
            id.clone(),
            tools,
            advertised_count,
            advertised_bytes,
        ))
    }
}

impl fmt::Debug for ToolRegistry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names = self
            .entries
            .values()
            .map(|tool| tool.origin().qualified_name())
            .collect::<Vec<_>>();
        formatter
            .debug_struct("ToolRegistry")
            .field("tools", &names)
            .finish_non_exhaustive()
    }
}

/// Builder for an immutable [`ToolRegistry`].
///
/// Registration order does not matter: the registry sorts by lookup key, so two hosts that install
/// the same tools in different orders produce the same registry and the same advertised table.
#[must_use]
pub struct ToolRegistryBuilder {
    entries: Vec<Arc<dyn Tool>>,
}

impl ToolRegistryBuilder {
    /// Adds one tool.
    pub fn register(mut self, tool: Arc<dyn Tool>) -> Self {
        self.entries.push(tool);
        self
    }

    /// Adds several tools.
    pub fn register_all(mut self, tools: impl IntoIterator<Item = Arc<dyn Tool>>) -> Self {
        self.entries.extend(tools);
        self
    }

    /// Validates every registration and freezes the registry.
    pub fn build(self) -> Result<ToolRegistry> {
        let mut entries = BTreeMap::new();
        for tool in self.entries {
            tool.validate()?;
            let key = tool.origin().lookup_key().clone();
            if entries.insert(key.clone(), tool).is_some() {
                return Err(Error::config(format!(
                    "tool lookup key `{key:?}` is registered more than once"
                )));
            }
        }
        Ok(ToolRegistry { entries })
    }
}
