//! How the server builds the threads `thread/start`, `thread/resume` and `thread/fork` load: the
//! host's part of Codex's `ThreadManager::start_thread` and its resume and fork variants
//! (`core/src/thread_manager.rs`).
//!
//! Codex's server builds every session itself, from configuration it loads: the model, the tools,
//! the instructions. A thread here runs an agent and models only its host has, so the host hands
//! the server a [`ThreadFactory`], and the server asks it for each thread it loads. The factory
//! prepares the thread — its agent, model resolver, run configuration and services, with the
//! settings adapter and background terminals it is loaded with — and the server does the rest, as
//! Codex's thread manager does: it applies the settings the request and the thread's history
//! carry, gives the thread its history and the recorder of its rollout, and builds it.
//!
//! A factory is asked once per thread loaded, with the thread's id and why it is loaded; one that
//! fails leaves nothing loaded, and the request fails with its error. After applying the final
//! settings, the server also refuses statically declared tools whose effective permissions require
//! approval without a corresponding request. Run-bound capability tools follow the normal
//! per-call approval path.

use std::{collections::BTreeSet, sync::Arc};

use async_trait::async_trait;
use ra_core::{
    permission::PermissionDecision,
    session::{SessionId, rollout::RolloutThreadSpawn},
    tool::{Tool, ToolApprovalPolicy, ToolAvailability},
};
use ra_runtime::thread::AgentThreadBuilder;

use super::{
    SessionSource, background_terminals::ThreadBackgroundTerminals,
    settings_adapter::ThreadSettingsAdapter,
};

/// Why a thread is being loaded.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThreadOrigin {
    /// A new thread, for `thread/start`.
    Start,
    /// A stored thread loaded again, for `thread/resume`.
    Resume,
    /// A new thread on a copy of another's history, for `thread/fork`.
    Fork {
        /// The thread it was forked from.
        source: SessionId,
    },
}

/// The thread a factory is asked to prepare.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadFactoryRequest {
    thread_id: SessionId,
    origin: ThreadOrigin,
    session_source: SessionSource,
    thread_spawn: Option<RolloutThreadSpawn>,
    ephemeral: bool,
    fork_session_id: Option<SessionId>,
}

impl ThreadFactoryRequest {
    pub(crate) const fn new(thread_id: SessionId, origin: ThreadOrigin) -> Self {
        Self {
            thread_id,
            origin,
            session_source: SessionSource::Unknown,
            thread_spawn: None,
            ephemeral: false,
            fork_session_id: None,
        }
    }

    pub(crate) fn with_session_context(
        mut self,
        source: SessionSource,
        spawn: Option<RolloutThreadSpawn>,
        ephemeral: bool,
        fork_session_id: Option<SessionId>,
    ) -> Self {
        self.session_source = source;
        self.thread_spawn = spawn;
        self.ephemeral = ephemeral;
        self.fork_session_id = fork_session_id;
        self
    }

    /// The server-selected source, restored from metadata when resuming a stored thread.
    #[must_use]
    pub const fn session_source(&self) -> &SessionSource {
        &self.session_source
    }

    /// The current thread's spawn ancestry, including its root cache-session identity.
    #[must_use]
    pub const fn thread_spawn(&self) -> Option<&RolloutThreadSpawn> {
        self.thread_spawn.as_ref()
    }

    /// Whether this thread is loaded without a persistent rollout of its own.
    #[must_use]
    pub const fn ephemeral(&self) -> bool {
        self.ephemeral
    }

    /// Codex session identity of the fork source: its spawn root, or its own thread ID.
    #[must_use]
    pub const fn fork_session_id(&self) -> Option<&SessionId> {
        self.fork_session_id.as_ref()
    }

    /// The thread's id, which its builder must carry.
    #[must_use]
    pub const fn thread_id(&self) -> &SessionId {
        &self.thread_id
    }

    /// Why it is loaded.
    #[must_use]
    pub const fn origin(&self) -> &ThreadOrigin {
        &self.origin
    }
}

/// A thread a factory prepared, not yet built.
///
/// The server gives the builder the thread's history and rollout recorder before building it,
/// replacing any the factory set.
#[must_use]
pub struct PreparedThread {
    builder: AgentThreadBuilder,
    adapter: Option<Arc<dyn ThreadSettingsAdapter>>,
    terminals: Option<Arc<dyn ThreadBackgroundTerminals>>,
}

impl std::fmt::Debug for PreparedThread {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedThread")
            .field("thread_id", self.builder.thread_id())
            .field("has_adapter", &self.adapter.is_some())
            .field("has_terminals", &self.terminals.is_some())
            .finish()
    }
}

impl PreparedThread {
    /// The thread `builder` builds, with no product settings and no background terminals.
    pub const fn new(builder: AgentThreadBuilder) -> Self {
        Self {
            builder,
            adapter: None,
            terminals: None,
        }
    }

    /// Applies product settings — approval and sandbox policy, named permissions — through
    /// `adapter`, which the thread is then loaded with. Without one, a request that sets them is
    /// refused.
    pub fn with_settings_adapter(mut self, adapter: Arc<dyn ThreadSettingsAdapter>) -> Self {
        self.adapter = Some(adapter);
        self
    }

    /// Hands over the commands the thread's tools leave running, for
    /// `thread/backgroundTerminals/*` and for stopping them with the thread.
    pub fn with_background_terminals(
        mut self,
        terminals: Arc<dyn ThreadBackgroundTerminals>,
    ) -> Self {
        self.terminals = Some(terminals);
        self
    }

    pub(crate) fn into_parts(self) -> PreparedParts {
        (self.builder, self.adapter, self.terminals)
    }
}

/// Checks statically declared tools against the final settings that start, resume or fork applied.
/// Fixed permissions precede a tool's approval fallback, as they do in runtime dispatch.
/// Capabilities are bound per run before their tools are read; their calls use the normal
/// approval path, which refuses a stopped call when its product provides no request.
pub(crate) fn check_approval_requests(
    builder: &AgentThreadBuilder,
    adapter: Option<&dyn ThreadSettingsAdapter>,
) -> ra_core::error::Result<()> {
    let unasked = declared_tools(builder)
        .into_iter()
        .filter(|tool| {
            let (tool, agent_id) = tool;
            let options = tool.options();
            options.approval() == ToolApprovalPolicy::Always
                && options.availability() != ToolAvailability::Disabled
                && builder.config().permission().evaluate(
                    options.action_classes(),
                    tool.origin(),
                    agent_id,
                    PermissionDecision::Ask,
                ) == PermissionDecision::Ask
                && !adapter.is_some_and(|adapter| adapter.requests_approval_for(tool.as_ref()))
        })
        .map(|(tool, _)| format!("`{}`", tool.origin().qualified_name()))
        .collect::<BTreeSet<_>>();
    if unasked.is_empty() {
        return Ok(());
    }
    Err(ra_core::error::Error::config(format!(
        "every call of tool {} needs approval, but the thread's host asks the client about none of them",
        unasked.into_iter().collect::<Vec<_>>().join(", ")
    )))
}

/// The tools a thread's runs can call as `builder` prepares it: its agent's, those of every agent
/// its handoffs reach. Run-bound capability tools are not available at this lifecycle stage.
fn declared_tools(builder: &AgentThreadBuilder) -> Vec<(Arc<dyn Tool>, ra_core::item::AgentId)> {
    let config = builder.config();
    let start = builder.agent().execution();
    let mut reached = BTreeSet::from([start.id().clone()]);
    let mut agents = vec![Arc::clone(start)];
    let mut tools = Vec::new();
    while let Some(agent) = agents.pop() {
        tools.extend(
            agent
                .tools()
                .iter()
                .cloned()
                .map(|tool| (tool, agent.id().clone())),
        );
        for handoff in agent.handoffs() {
            if reached.insert(handoff.target_agent().clone())
                && let Some(target) = config.agent_registry().get(handoff.target_agent())
            {
                agents.push(Arc::clone(target));
            }
        }
    }
    tools
}

/// A prepared thread taken apart: its builder, settings adapter and background terminals.
pub(crate) type PreparedParts = (
    AgentThreadBuilder,
    Option<Arc<dyn ThreadSettingsAdapter>>,
    Option<Arc<dyn ThreadBackgroundTerminals>>,
);

/// Prepares the threads the server loads (see the module's documentation).
#[async_trait]
pub trait ThreadFactory: Send + Sync + 'static {
    /// An optional shared host MCP control port, installed alongside the factory.
    fn mcp_runtime(&self) -> Option<Arc<dyn super::McpRuntimeControl>> {
        None
    }

    /// Prepares the thread `request` describes. Its builder must be for
    /// [`ThreadFactoryRequest::thread_id`].
    ///
    /// # Errors
    ///
    /// Returns an error if the thread cannot be prepared; the request that asked for it fails with
    /// the error, and nothing is loaded.
    async fn prepare_thread(
        &self,
        request: &ThreadFactoryRequest,
    ) -> ra_core::error::Result<PreparedThread>;
}
