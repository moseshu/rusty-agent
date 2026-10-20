//! How a host assembles each spawned agent.
//!
//! Codex starts every spawned agent as a thread of its own: the thread manager builds a new
//! session for it, and that session owns its own services — in particular its own unified-exec
//! process manager — and plans its own tools for a sub-agent session source. Without a factory a
//! spawned agent here runs the declaration it was resolved to, sharing every tool instance, and
//! whatever those tools hold, with the agent that spawned it. A host whose tools own per-thread
//! resources gives the tree a [`SpawnedAgentFactory`] with
//! [`AgentControl::with_spawned_agent_factory`](super::AgentControl::with_spawned_agent_factory),
//! which prepares each spawned agent the way the host prepares a thread.

use std::sync::Arc;

use crate::runner::RunConfig;
use async_trait::async_trait;
use ra_core::{
    agent::{AgentId, AgentSpec, control::AgentPath},
    capability::Capability,
    error::Result,
    tool::Tool,
};

/// Prepares each agent a tree spawns, as Codex's thread manager prepares each spawned thread.
#[async_trait]
pub trait SpawnedAgentFactory: Send + Sync {
    /// Prepares the agent about to be spawned at [`SpawnedAgentRequest::path`].
    ///
    /// Called once per spawn, before the agent's rollout is created and before it is added to
    /// the tree. An error refuses the spawn, and the model is told why.
    ///
    /// # Errors
    ///
    /// Whatever keeps the host from preparing the agent.
    async fn prepare(&self, request: SpawnedAgentRequest<'_>) -> Result<PreparedSpawnedAgent>;
}

/// What a spawn asks a [`SpawnedAgentFactory`] to prepare.
#[derive(Debug, Clone, Copy)]
pub struct SpawnedAgentRequest<'a> {
    path: &'a AgentPath,
    agent: &'a Arc<AgentSpec>,
    agent_type: Option<&'a AgentId>,
}

impl<'a> SpawnedAgentRequest<'a> {
    pub(super) const fn new(
        path: &'a AgentPath,
        agent: &'a Arc<AgentSpec>,
        agent_type: Option<&'a AgentId>,
    ) -> Self {
        Self {
            path,
            agent,
            agent_type,
        }
    }

    /// Where the agent will live in the tree.
    #[must_use]
    pub const fn path(&self) -> &AgentPath {
        self.path
    }

    /// The declaration the spawn resolved: the registered agent named by `agent_type`, or the
    /// agent running the caller's current turn.
    #[must_use]
    pub const fn agent(&self) -> &Arc<AgentSpec> {
        self.agent
    }

    /// The agent type the caller asked for, if any.
    #[must_use]
    pub const fn agent_type(&self) -> Option<&AgentId> {
        self.agent_type
    }
}

/// A spawned agent as its host prepared it.
pub struct PreparedSpawnedAgent {
    agent: Arc<AgentSpec>,
    resources: Option<Arc<dyn SpawnedAgentResources>>,
    capabilities: Option<PreparedCapabilities>,
}

struct PreparedCapabilities {
    capabilities: Vec<Arc<dyn Capability>>,
    tools: Vec<Arc<dyn Tool>>,
}

impl PreparedSpawnedAgent {
    /// The agent will run `agent`, holding nothing the tree has to release.
    #[must_use]
    pub const fn new(agent: Arc<AgentSpec>) -> Self {
        Self {
            agent,
            resources: None,
            capabilities: None,
        }
    }

    /// Gives the agent resources of its own, released once the agent is closed or the tree shuts
    /// down — after its last run has ended, as Codex's session shutdown releases a thread's
    /// processes.
    #[must_use]
    pub fn with_resources(mut self, resources: Arc<dyn SpawnedAgentResources>) -> Self {
        self.resources = Some(resources);
        self
    }

    /// Replaces the parent's prepared capability surface with this spawned agent's own, while
    /// retaining the other inherited run settings. Empty collections explicitly withdraw it.
    ///
    /// The agent must already contain the static instructions and exact shared tools. Runtime
    /// assembly skips that static channel, checks bound declarations and replaces handlers at
    /// their original positions, as in [`RunConfig::with_prepared_capabilities`].
    #[must_use]
    pub fn with_prepared_capabilities(
        mut self,
        capabilities: impl IntoIterator<Item = Arc<dyn Capability>>,
        tools: impl IntoIterator<Item = Arc<dyn Tool>>,
    ) -> Self {
        self.capabilities = Some(PreparedCapabilities {
            capabilities: capabilities.into_iter().collect(),
            tools: tools.into_iter().collect(),
        });
        self
    }

    pub(super) fn configure_run(&self, config: RunConfig) -> RunConfig {
        match &self.capabilities {
            Some(prepared) => config.with_prepared_capabilities(
                prepared.capabilities.iter().cloned(),
                prepared.tools.iter().cloned(),
            ),
            None => config,
        }
    }

    pub(super) fn into_parts(self) -> (Arc<AgentSpec>, Option<Arc<dyn SpawnedAgentResources>>) {
        (self.agent, self.resources)
    }
}

impl std::fmt::Debug for PreparedSpawnedAgent {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PreparedSpawnedAgent")
            .field("agent", &self.agent.id())
            .field("has_resources", &self.resources.is_some())
            .field("has_prepared_capabilities", &self.capabilities.is_some())
            .finish()
    }
}

/// What one spawned agent holds apart from the rest of the tree.
#[async_trait]
pub trait SpawnedAgentResources: Send + Sync {
    /// Releases the resources once the agent's runs have stopped. Called at most once.
    async fn shutdown(&self);
}
