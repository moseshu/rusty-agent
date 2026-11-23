//! How the coding product prepares the agents its trees spawn.
//!
//! Codex starts each spawned agent as a session of its own: the session gets its own unified-exec
//! process manager and plans its own tools for a sub-agent session source. The tree's factory here
//! does the same with the coding product's assembly — the spawned agent is assembled from the same
//! host for the same role and tier, with a process manager of its own. In product-configured
//! threads a child is offered collaboration when the inherited version is V2 and its current model's
//! catalog entry declares V2, as Codex plans a V2 sub-agent's tools; this includes the children of a
//! thread migrated from the former host assembly. Hosts without product configuration enable child
//! collaboration with [`CodingHost::with_spawned_agent_collaboration`]. Closing the child or tree
//! stops its background terminals, as in Codex.
//! A planner child returns its plan and unresolved questions to the parent; its prompt does not
//! encourage root-only user questions. This is a product adaptation of inherited planner roles.
//!
//! A spawn naming an `agent_type`, or made while a handoff has moved the turn to another agent,
//! runs that registered declaration as it is: the factory prepares only the agent it assembles.
//!
//! # A spawned agent's own session services
//!
//! In a thread with product hooks, every spawned agent — whatever declaration it runs — also gets
//! what Codex's spawned session builds for itself: hooks of its own, built from the configuration
//! its parent's hooks run with now, reporting its own session and transcript, with an MCP executor
//! naming its thread, executor hooks of its own, and, when the thread runs a managed network
//! proxy, a proxy and approval service of its own started from its parent's. They replace what its
//! runs would otherwise inherit from the run that spawned it, and they are released when the agent
//! is closed.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, PoisonError, Weak},
};

use async_trait::async_trait;
use ra_core::{
    agent::AgentId, error::Result, hook::SessionStartSource, prompt::PromptRole,
    session::SessionId, session::rollout::RolloutRecorder, state::RunId,
};
use ra_exec::session::ProcessManager;
use ra_runtime::{
    agent::control::{
        AgentControl, PreparedSpawnedAgent, SpawnedAgentFactory, SpawnedAgentRequest,
        SpawnedAgentResources,
    },
    runner::RunConfig,
};

use crate::{
    agent::assemble_agent, host::CodingHost, profile::CodingProfile,
    thread_hooks::CodingThreadHooks, thread_settings::CodingThreadSettings,
};

/// What a thread's spawned agents' own services start from: the thread's product hooks and its
/// managed network proxy, when it has them.
#[derive(Default)]
pub(crate) struct ThreadServices {
    pub(crate) hooks: Option<Arc<CodingThreadHooks>>,
    pub(crate) network_proxy: Option<Arc<crate::thread_settings::ThreadNetworkProxy>>,
}

/// A tree for the coding agent `id` assembled from `host`, preparing each spawned agent as above.
pub(crate) fn agent_control(
    host: &CodingHost,
    id: AgentId,
    name: String,
    role: &PromptRole,
    profile: CodingProfile,
    cache_session_id: Option<SessionId>,
    services: ThreadServices,
) -> Result<AgentControl> {
    // A product thread's tree runs Codex's `effective_agent_max_threads` spawned agents at once.
    let control = host
        .product_collaboration()
        .map_or_else(AgentControl::new, |product| {
            AgentControl::with_max_concurrent_threads(product.max_spawned_threads())
        });
    control.with_spawned_agent_factory(Arc::new(CodingSpawnedAgents {
        host: host.clone(),
        id,
        name,
        role: role.clone(),
        profile,
        cache_session_id,
        root: services,
        children: Mutex::new(HashMap::new()),
    }))
}

struct CodingSpawnedAgents {
    host: CodingHost,
    id: AgentId,
    name: String,
    role: PromptRole,
    profile: CodingProfile,
    cache_session_id: Option<SessionId>,
    /// The services of the thread the tree belongs to.
    root: ThreadServices,
    /// The services of the spawned agents still alive, by session, for the agents they spawn.
    children: Mutex<HashMap<SessionId, Weak<ChildSession>>>,
}

impl CodingSpawnedAgents {
    /// The hooks and proxy of the agent of `session_id`, which spawns the next agent; the thread's
    /// own for its root, or when the spawner is unknown.
    fn spawner(
        &self,
        session_id: Option<&SessionId>,
    ) -> (
        Option<Arc<CodingThreadHooks>>,
        Option<Arc<crate::thread_settings::ThreadNetworkProxy>>,
    ) {
        let child = session_id.and_then(|session_id| {
            self.children
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get(session_id)
                .and_then(Weak::upgrade)
        });
        match child {
            Some(child) => (child.hooks.clone(), child.network_proxy.clone()),
            None => (self.root.hooks.clone(), self.root.network_proxy.clone()),
        }
    }

    /// Builds the spawned agent's own services from its spawner's, as Codex's spawned session
    /// builds its own from the configuration it copies.
    async fn child_session(
        &self,
        request: &SpawnedAgentRequest<'_>,
    ) -> Result<Option<Arc<ChildSession>>> {
        let (parent_hooks, parent_proxy) = self.spawner(request.parent_session_id());
        if parent_hooks.is_none() && parent_proxy.is_none() {
            return Ok(None);
        }
        let start = if request.forked() {
            SessionStartSource::Fork
        } else {
            SessionStartSource::Startup
        };
        let hooks = match parent_hooks {
            Some(parent_hooks) => {
                let identity = crate::hooks::ChildHookIdentity::spawned(
                    request.agent_type().map(AgentId::as_str),
                    start,
                    parent_hooks.runtime(),
                );
                Some(Arc::new(
                    parent_hooks
                        .spawn_child(request.session_id().clone(), identity, &self.host)
                        .map_err(|error| ra_core::error::Error::config(error.to_string()))?,
                ))
            }
            None => None,
        };
        let network_proxy = match parent_proxy {
            Some(proxy) => Some(
                proxy
                    .spawn_child(
                        hooks.as_ref().map(|hooks| Arc::clone(hooks.runtime())),
                        request.session_id(),
                    )
                    .await
                    .map_err(|error| ra_core::error::Error::config(error.to_string()))?,
            ),
            None => None,
        };
        let child = Arc::new(ChildSession {
            hooks,
            network_proxy,
        });
        self.children
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(request.session_id().clone(), Arc::downgrade(&child));
        Ok(Some(child))
    }
}

#[async_trait]
impl SpawnedAgentFactory for CodingSpawnedAgents {
    async fn prepare(&self, request: SpawnedAgentRequest<'_>) -> Result<PreparedSpawnedAgent> {
        let (prepared, processes) = if request.agent_type().is_some()
            || request.agent().id() != &self.id
        {
            (
                PreparedSpawnedAgent::new(Arc::clone(request.agent()))
                    .with_prepared_capabilities([], []),
                None,
            )
        } else {
            let host = self.host.for_spawned_agent();
            let assembled = assemble_agent(
                self.id.clone(),
                self.name.clone(),
                &self.role,
                &host,
                self.profile,
            )
            .await?;
            (
                PreparedSpawnedAgent::new(assembled.agent)
                    .with_prepared_capabilities(assembled.capabilities, assembled.capability_tools),
                Some(Arc::clone(host.process_manager())),
            )
        };
        // Built last, after anything that can fail, so a refused preparation leaves nothing of the
        // agent's services to release.
        let session = self.child_session(&request).await?;
        let mut prepared = if let Some(session) = &session {
            let session = Arc::clone(session);
            prepared.with_run_config(move |config| session.install(config))
        } else {
            prepared
        };
        if processes.is_some() || session.is_some() {
            prepared = prepared.with_resources(Arc::new(SpawnedResources { processes, session }));
        }
        Ok(if let Some(key) = &self.cache_session_id {
            prepared.with_prompt_cache_key_override(key.as_str())
        } else {
            prepared
        })
    }
}

/// A spawned agent's own session services.
struct ChildSession {
    /// Its own product hooks, in a thread that has them.
    hooks: Option<Arc<CodingThreadHooks>>,
    /// Its own managed network proxy and approval service, which its session state hangs on.
    network_proxy: Option<Arc<crate::thread_settings::ThreadNetworkProxy>>,
}

impl ChildSession {
    /// The agent's run configuration: what it inherited from its spawner, with its own hooks,
    /// executor hooks and tool-result table, and its own session state — managed network proxy,
    /// approvals, permission grants and MCP approvals.
    fn install(&self, config: RunConfig) -> RunConfig {
        let settings = config
            .app_context()
            .and_then(|context| context.downcast_ref::<CodingThreadSettings>())
            .cloned();
        let config = match (settings, &self.network_proxy) {
            (Some(settings), Some(proxy)) => {
                config.with_app_context(Arc::new(settings.for_spawned_session(Arc::clone(proxy))))
            }
            _ => config,
        };
        match &self.hooks {
            Some(hooks) => hooks.install(config),
            None => config,
        }
    }
}

/// What a spawned agent holds apart from its tree: its process manager, when it was assembled with
/// one, and its own session services.
struct SpawnedResources {
    processes: Option<Arc<ProcessManager>>,
    session: Option<Arc<ChildSession>>,
}

#[async_trait]
impl SpawnedAgentResources for SpawnedResources {
    async fn shutdown(&self) {
        if let Some(processes) = &self.processes {
            processes.terminate_background_terminals().await;
        }
        if let Some(session) = &self.session {
            if let Some(hooks) = &session.hooks {
                ra_protocol::server::ThreadProductRuntime::close(hooks.as_ref()).await;
            }
            if let Some(proxy) = &session.network_proxy {
                proxy.stop();
            }
        }
    }

    fn rollout_created(&self, recorder: &Arc<dyn RolloutRecorder>) {
        if let Some(hooks) = self
            .session
            .as_ref()
            .and_then(|session| session.hooks.as_ref())
        {
            hooks.runtime().set_own_rollout(recorder);
        }
    }

    fn run_ended(&self, run_id: &RunId) {
        if let Some(hooks) = self
            .session
            .as_ref()
            .and_then(|session| session.hooks.as_ref())
        {
            hooks.finish_turn(run_id.as_str());
        }
    }
}
