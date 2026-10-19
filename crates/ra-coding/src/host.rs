//! Product host assembly, workspace capability binding, and event routing.

use std::{fmt, io, path::Path, sync::Arc};

use ra_context::{
    budget::ToolResultBudget, compaction::CompactionCapability,
    eviction::ToolOutputReferenceTrimmer,
};
use ra_core::{
    event::{HostEventEmitter, HostEventSink, NoopHostEventSink},
    item::AgentId,
    skill::SkillCatalog,
    state::EventSeqAllocator,
    tool::{Tool, ToolNamespace, ToolServices},
    user_input::UserInputPort,
    web::WebAccess,
};

use ra_exec::{
    fs::{RootedFileSystem, Workspace},
    session::ProcessManager,
};
use ra_runtime::runner::RunConfig;
use ra_tools::agent_ns::COLLABORATION_NAMESPACE;
use ra_tools::capability::{
    ApplyPatchCapability, FilesystemCapability, SearchCapability, ShellCapability,
    SkillsCapability, TodoCapability, ViewImageCapability, WebCapability,
};

/// The host runtime context and capabilities for the coding agent.
///
/// `CodingHost` is the product's concrete implementation of the framework's application context
/// and tool services. It owns the workspace root capability and the host event sink, ensuring
/// clean separation between:
///
/// - **Model-Visible Channel**: [`ToolOutput`](ra_core::tool::ToolOutput) containing structured
///   [`ObservationMetadata`](ra_core::tool::ObservationMetadata) rendered into text for the model.
/// - **Host-Visible Channel**: [`HostEvent`](ra_core::event::HostEvent) emitted to UI, telemetry,
///   and rollout logs without leaking into model prompts.
#[derive(Clone)]
pub struct CodingHost {
    workspace: Workspace,
    mcp: Option<Arc<crate::mcp::CodingMcpRuntime>>,
    process_manager: Arc<ProcessManager>,
    event_sink: Arc<dyn HostEventSink>,
    exec_permission_approvals: bool,
    web: Option<Arc<dyn WebAccess>>,
    skills: Option<Arc<dyn SkillCatalog>>,
    user_input: Option<Arc<dyn UserInputPort>>,
    request_user_input: bool,
    collaboration: Option<Collaboration>,
    /// Set on the host a spawned agent is assembled from.
    spawned_agent: bool,
}

/// Codex's default namespace for the collaboration tools.
pub(crate) fn default_collaboration_namespace() -> ra_core::error::Result<ToolNamespace> {
    ToolNamespace::new(COLLABORATION_NAMESPACE)
}

/// Where the collaboration tools are advertised.
#[derive(Clone)]
enum CollaborationNamespace {
    /// Codex's default, [`COLLABORATION_NAMESPACE`].
    Default,
    /// A namespace the host chose, or bare names with `None`.
    Chosen(Option<ToolNamespace>),
}

/// How the collaboration tools are offered: the host's counterpart of Codex's
/// `multi_agent_v2` configuration.
#[derive(Clone)]
struct Collaboration {
    /// Codex's `tool_namespace`.
    namespace: CollaborationNamespace,
    /// Whether spawned agents are offered the tools as well (see
    /// [`CodingHost::with_spawned_agent_collaboration`]).
    spawned_agents: bool,
}

impl fmt::Debug for CodingHost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CodingHost")
            .field("workspace", &self.workspace)
            .field("has_event_sink", &true)
            .finish_non_exhaustive()
    }
}

impl CodingHost {
    /// Opens the workspace root and creates a coding host with a default no-op event sink.
    ///
    /// # Errors
    ///
    /// Returns [`io::Error`] if `workspace_root` cannot be opened.
    pub fn open(workspace_root: impl AsRef<Path>) -> io::Result<Self> {
        let workspace = Workspace::open(workspace_root)?;
        Ok(Self {
            workspace,
            mcp: None,
            process_manager: Arc::new(ProcessManager::default()),
            event_sink: Arc::new(NoopHostEventSink),
            exec_permission_approvals: false,
            web: None,
            skills: None,
            user_input: None,
            request_user_input: false,
            collaboration: None,
            spawned_agent: false,
        })
    }

    /// The same host with a process manager of its own, for a thread whose commands must not mix
    /// with another's: each Codex session has its own unified-exec process manager, so listing or
    /// stopping one thread's background terminals leaves the others' alone.
    #[must_use]
    pub fn with_new_process_manager(&self) -> Self {
        Self {
            process_manager: Arc::new(ProcessManager::default()),
            ..self.clone()
        }
    }

    /// Installs the shared backend used by both web tools.
    #[must_use]
    pub fn with_web_access(mut self, access: Arc<dyn WebAccess>) -> Self {
        self.web = Some(access);
        self
    }

    /// Installs the catalog used by the skills listing and the skill-loading tool.
    #[must_use]
    pub fn with_skill_catalog(mut self, catalog: Arc<dyn SkillCatalog>) -> Self {
        self.skills = Some(catalog);
        self
    }

    /// Enables questions in Default mode and routes them to `port` for standalone runs.
    /// App-server threads replace this port with their own client connection.
    #[must_use]
    pub fn with_user_input(mut self, port: Arc<dyn UserInputPort>) -> Self {
        self.user_input = Some(port);
        self.request_user_input = true;
        self
    }

    pub(crate) const fn with_client_user_input(mut self) -> Self {
        self.request_user_input = true;
        self
    }

    pub(crate) const fn request_user_input_enabled(&self) -> bool {
        self.request_user_input
    }

    /// Enables the six collaboration tools in Codex's default `collaboration` namespace.
    ///
    /// The tools act on an agent tree. The thread factory gives each thread a tree of its own;
    /// a standalone caller builds the agent with
    /// [`build_agent_with_control`](crate::build_agent_with_control), which returns the tree to
    /// bind its runs to. Each spawned agent is assembled again from this host with a process
    /// manager of its own, as Codex starts each spawned agent as a session of its own.
    #[must_use]
    pub fn with_collaboration(mut self) -> Self {
        self.collaboration = Some(Collaboration {
            namespace: CollaborationNamespace::Default,
            spawned_agents: false,
        });
        self
    }

    /// Advertises the collaboration tools in `namespace`, or under their bare names with `None`:
    /// Codex's `multi_agent_v2.tool_namespace`.
    ///
    /// Namespaces are an `OpenAI` Responses feature. Chat Completions and Anthropic adapters
    /// refuse a namespaced tool, so a host serving those providers selects bare names here.
    /// Has no effect until collaboration is enabled with [`Self::with_collaboration`].
    #[must_use]
    pub fn with_collaboration_namespace(mut self, namespace: Option<ToolNamespace>) -> Self {
        if let Some(collaboration) = &mut self.collaboration {
            collaboration.namespace = CollaborationNamespace::Chosen(namespace);
        }
        self
    }

    /// Offers the collaboration tools to spawned agents too.
    ///
    /// Codex offers them to a spawned agent only when the agent's model declares native support
    /// for its second multi-agent version in the model catalog. This framework has no model
    /// catalog, so the host states that fact here; without it a spawned agent is assembled without
    /// the tools, as Codex assembles one whose model declares nothing. Has no effect until
    /// collaboration is enabled with [`Self::with_collaboration`].
    #[must_use]
    pub fn with_spawned_agent_collaboration(mut self) -> Self {
        if let Some(collaboration) = &mut self.collaboration {
            collaboration.spawned_agents = true;
        }
        self
    }

    /// The host a spawned agent is assembled from: this host, with a process manager of its own.
    pub(crate) fn for_spawned_agent(&self) -> Self {
        Self {
            spawned_agent: true,
            ..self.with_new_process_manager()
        }
    }

    pub(crate) const fn collaboration_enabled(&self) -> bool {
        self.collaboration.is_some()
    }

    /// Whether the agent assembled from this host is offered the collaboration tools.
    pub(crate) fn collaboration_offered(&self) -> bool {
        self.collaboration
            .as_ref()
            .is_some_and(|collaboration| !self.spawned_agent || collaboration.spawned_agents)
    }

    /// The namespace the collaboration tools are advertised in, if any.
    pub(crate) fn collaboration_namespace(&self) -> ra_core::error::Result<Option<ToolNamespace>> {
        match self
            .collaboration
            .as_ref()
            .map(|collaboration| &collaboration.namespace)
        {
            Some(CollaborationNamespace::Chosen(namespace)) => Ok(namespace.clone()),
            Some(CollaborationNamespace::Default) | None => {
                default_collaboration_namespace().map(Some)
            }
        }
    }

    pub(crate) fn web_capability(&self) -> ra_core::error::Result<Option<WebCapability>> {
        self.web.clone().map(WebCapability::new).transpose()
    }

    pub(crate) fn skills_capability(&self) -> ra_core::error::Result<Option<SkillsCapability>> {
        self.skills.clone().map(SkillsCapability::new).transpose()
    }

    /// Lets `exec_command` ask for permissions of its own: Codex's `exec_permission_approvals`
    /// feature, off by default as Codex's is. The tool then offers `with_additional_permissions`
    /// and `additional_permissions`, and a call that asks for them is checked as Codex checks it
    /// and asks before it runs in a sandbox widened by them.
    #[must_use]
    pub const fn with_exec_permission_approvals(mut self) -> Self {
        self.exec_permission_approvals = true;
        self
    }

    /// Installs an explicitly configured MCP runtime shared by this host's threads.
    #[must_use]
    pub fn with_mcp_runtime(mut self, runtime: Arc<crate::mcp::CodingMcpRuntime>) -> Self {
        self.mcp = Some(runtime);
        self
    }

    /// The runtime used by tool discovery and the app server's control port.
    #[must_use]
    pub fn mcp_runtime(&self) -> Option<&Arc<crate::mcp::CodingMcpRuntime>> {
        self.mcp.as_ref()
    }

    /// Sets the host event sink.
    #[must_use]
    pub fn with_event_sink(mut self, event_sink: Arc<dyn HostEventSink>) -> Self {
        self.event_sink = event_sink;
        self
    }

    /// The capability-scoped filesystem handle for the workspace root.
    #[must_use]
    pub fn workspace_fs(&self) -> &Arc<RootedFileSystem> {
        self.workspace.filesystem()
    }

    /// The canonical workspace root that command tools use as their initial directory.
    #[must_use]
    pub fn workspace_root(&self) -> &Path {
        self.workspace.root()
    }

    /// The manager shared by the command-start and command-interaction tools.
    #[must_use]
    pub fn process_manager(&self) -> &Arc<ProcessManager> {
        &self.process_manager
    }

    /// The background terminals of the commands this host's tools run, for the app server to
    /// load with a thread.
    #[must_use]
    pub fn background_terminals(&self) -> crate::background_terminals::CodingBackgroundTerminals {
        crate::background_terminals::CodingBackgroundTerminals::new(Arc::clone(
            &self.process_manager,
        ))
    }

    /// The items this host's tool calls show in a thread's turns, for the app server to show them
    /// by.
    #[must_use]
    pub fn thread_tool_items(&self) -> crate::thread_items::CodingThreadToolItems {
        crate::thread_items::CodingThreadToolItems::new(self.workspace_root())
    }

    /// The host event sink for this host.
    #[must_use]
    pub fn event_sink(&self) -> &Arc<dyn HostEventSink> {
        &self.event_sink
    }

    /// Constructs [`ToolServices`] with event routing and the coding product's result budget.
    ///
    /// The result budget is a host policy, not a tool setting: all completed observations keep
    /// their full session form, while the runtime projects only an oversized model-facing excerpt.
    pub fn build_tool_services(&self) -> ToolServices {
        let services = ToolServices::new()
            .with_event_sink(Arc::clone(&self.event_sink))
            .with_output_projector(Arc::new(ToolResultBudget::default()));
        match &self.user_input {
            Some(port) => services.with_user_input(Arc::clone(port)),
            None => services,
        }
    }

    /// Constructs the coding product's run policy, including reference-aware tool-output eviction
    /// and model-window-driven context compaction.
    ///
    /// This affects only future model requests. Complete observations stay in the session and the
    /// `RunState` reference ledger lets a resumed run apply the same policy without rebuilding
    /// retention facts from prose.
    ///
    /// Compaction is installed as a capability rather than as a bare context processor, which is
    /// what its own declaration says it is: it belongs to the `compaction` family, and installing
    /// it as a loose processor would leave that family absent from the installed set — so a later
    /// capability that declares a dependency on it would be refused beside the very thing that
    /// satisfies it.
    pub fn build_run_config(&self) -> RunConfig {
        RunConfig::new()
            .with_context_filter(Arc::new(ToolOutputReferenceTrimmer::default()))
            .with_capability(Arc::new(CompactionCapability::default()))
    }

    /// Creates the workspace-confined editing capability.
    pub fn apply_patch_capability(&self) -> ra_core::error::Result<ApplyPatchCapability> {
        Ok(ApplyPatchCapability::for_workspace(&self.workspace)?
            .with_approval_resolver(Arc::new(crate::thread_settings::patch_approval)))
    }

    /// Creates the workspace-confined file-reading capability.
    pub fn filesystem_capability(&self) -> ra_core::error::Result<FilesystemCapability> {
        FilesystemCapability::for_workspace(&self.workspace)
    }

    /// Creates the workspace-confined structured-discovery capability.
    pub fn search_capability(&self) -> ra_core::error::Result<SearchCapability> {
        SearchCapability::for_workspace(&self.workspace)
    }

    /// Creates the workspace-confined image capability.
    pub fn view_image_capability(&self) -> ra_core::error::Result<ViewImageCapability> {
        ViewImageCapability::for_workspace(&self.workspace)
    }

    /// Creates the plan-board capability.
    ///
    /// The only one that takes nothing from this host: a plan reaches no workspace and no process,
    /// so there is nothing here for it to be confined to. It is a method anyway, beside the others,
    /// because where a coding agent's capabilities come from is one answer rather than "the host,
    /// except for this one".
    pub fn todo_capability(&self) -> ra_core::error::Result<TodoCapability> {
        TodoCapability::new()
    }

    /// The `request_permissions` tool, asking the client of the thread it runs in. Codex offers it
    /// only behind its `request_permissions_tool` feature, which is off by default; a host that
    /// wants it adds it to its agent.
    ///
    /// # Errors
    ///
    /// Returns a configuration error when the tool's identity or schema cannot be built.
    pub fn request_permissions_tool(&self) -> ra_core::error::Result<Arc<dyn ra_core::tool::Tool>> {
        Ok(Arc::new(
            ra_tools::request_permissions::RequestPermissionsTool::new(Arc::new(
                crate::request_permissions::CodingPermissionRequests,
            ))?,
        ))
    }

    /// Creates the workspace-rooted execution capability on this host's session manager.
    ///
    /// The manager is this host's rather than the capability's own, so the sessions an agent starts
    /// are the ones this host cancels and reports at closeout.
    pub fn shell_capability(&self) -> ra_core::error::Result<ShellCapability> {
        let base = self.process_manager.environment().clone();
        let mut policy = crate::thread_settings::CodingCommandPolicy::new(base);
        let mut capability =
            ShellCapability::for_workspace(&self.workspace, Arc::clone(&self.process_manager))?;
        if self.exec_permission_approvals {
            policy = policy.with_exec_permission_approvals();
            capability = capability.with_additional_permissions();
        }
        Ok(capability
            .with_command_policy(Arc::new(policy))
            .with_stdin_policy(Arc::new(crate::thread_settings::CodingStdinPolicy)))
    }

    /// Creates the coding product's workspace-confined patch tool.
    ///
    /// This and the five entries below reach their tool through the capability that owns it rather
    /// than constructing one directly. A capability is where a tool's configuration is decided —
    /// which workspace confines it, which process manager it shares — so a second construction path
    /// beside it is a second set of those decisions, and the two disagree the first time either
    /// changes.
    pub fn apply_patch_tool(&self) -> ra_core::error::Result<Arc<dyn Tool>> {
        Ok(self.apply_patch_capability()?.apply_patch())
    }

    /// Creates the workspace-confined file-reading entry.
    pub fn read_file_tool(&self) -> ra_core::error::Result<Arc<dyn Tool>> {
        Ok(self.filesystem_capability()?.read_file())
    }

    /// Creates the workspace-confined text-search entry.
    pub fn grep_tool(&self) -> ra_core::error::Result<Arc<dyn Tool>> {
        Ok(self.search_capability()?.grep())
    }

    /// Creates the workspace-confined file-pattern lookup entry.
    pub fn glob_tool(&self) -> ra_core::error::Result<Arc<dyn Tool>> {
        Ok(self.search_capability()?.glob())
    }

    /// Creates the workspace-confined image entry.
    pub fn view_image_tool(&self) -> ra_core::error::Result<Arc<dyn Tool>> {
        Ok(self.view_image_capability()?.view_image())
    }

    /// Creates the plan-board entry.
    pub fn update_plan_tool(&self) -> ra_core::error::Result<Arc<dyn Tool>> {
        Ok(self.todo_capability()?.update_plan())
    }

    /// Creates the workspace-rooted command entry backed by this host's session manager.
    pub fn exec_command_tool(&self) -> ra_core::error::Result<Arc<dyn Tool>> {
        Ok(self.shell_capability()?.exec_command())
    }

    /// Creates the input entry for sessions started by [`Self::exec_command_tool`].
    pub fn write_stdin_tool(&self) -> ra_core::error::Result<Arc<dyn Tool>> {
        Ok(self.shell_capability()?.write_stdin())
    }

    /// Creates an authenticated [`HostEventEmitter`] bound to this host's sink and the given allocator.
    #[must_use]
    pub fn emitter(&self, agent_id: AgentId, allocator: EventSeqAllocator) -> HostEventEmitter {
        HostEventEmitter::new(agent_id, allocator, Arc::clone(&self.event_sink))
    }
}
