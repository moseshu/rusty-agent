//! How the coding product prepares the agents its trees spawn.
//!
//! Codex starts each spawned agent as a session of its own: the session gets its own unified-exec
//! process manager and plans its own tools for a sub-agent session source. The tree's factory here
//! does the same with the coding product's assembly — the spawned agent is assembled from the same
//! host for the same role and tier, with a process manager of its own, and offered the
//! collaboration tools only when the host says its model takes them (see
//! [`CodingHost::with_spawned_agent_collaboration`]). Its background terminals are stopped once it
//! is closed or the tree shuts down, as Codex's session shutdown stops a thread's processes.
//!
//! A spawn naming an `agent_type`, or made while a handoff has moved the turn to another agent,
//! runs that registered declaration as it is: the factory prepares only the agent it assembles.

use std::sync::Arc;

use async_trait::async_trait;
use ra_core::{agent::AgentId, error::Result, prompt::PromptRole};
use ra_exec::session::ProcessManager;
use ra_runtime::agent::control::{
    AgentControl, PreparedSpawnedAgent, SpawnedAgentFactory, SpawnedAgentRequest,
    SpawnedAgentResources,
};

use crate::{agent::assemble_agent, host::CodingHost, profile::CodingProfile};

/// A tree for the coding agent `id` assembled from `host`, preparing each spawned agent as above.
pub(crate) fn agent_control(
    host: &CodingHost,
    id: AgentId,
    name: String,
    role: &PromptRole,
    profile: CodingProfile,
) -> Result<AgentControl> {
    AgentControl::new().with_spawned_agent_factory(Arc::new(CodingSpawnedAgents {
        host: host.clone(),
        id,
        name,
        role: role.clone(),
        profile,
    }))
}

struct CodingSpawnedAgents {
    host: CodingHost,
    id: AgentId,
    name: String,
    role: PromptRole,
    profile: CodingProfile,
}

#[async_trait]
impl SpawnedAgentFactory for CodingSpawnedAgents {
    async fn prepare(&self, request: SpawnedAgentRequest<'_>) -> Result<PreparedSpawnedAgent> {
        if request.agent_type().is_some() || request.agent().id() != &self.id {
            return Ok(PreparedSpawnedAgent::new(Arc::clone(request.agent())));
        }
        let host = self.host.for_spawned_agent();
        let assembled = assemble_agent(
            self.id.clone(),
            self.name.clone(),
            &self.role,
            &host,
            self.profile,
        )
        .await?;
        Ok(
            PreparedSpawnedAgent::new(assembled.agent).with_resources(Arc::new(SpawnedProcesses(
                Arc::clone(host.process_manager()),
            ))),
        )
    }
}

/// The spawned agent's own process manager.
struct SpawnedProcesses(Arc<ProcessManager>);

#[async_trait]
impl SpawnedAgentResources for SpawnedProcesses {
    async fn shutdown(&self) {
        self.0.terminate_background_terminals().await;
    }
}
