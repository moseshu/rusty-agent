//! Product host assembly, workspace capability binding, and event routing.

use std::{fmt, io, path::Path, sync::Arc};

use ra_context::{
    budget::ToolResultBudget, compaction::CompactionCapability,
    eviction::ToolOutputReferenceTrimmer,
};
use ra_core::{
    event::{HostEventEmitter, HostEventSink, NoopHostEventSink},
    item::AgentId,
    state::EventSeqAllocator,
    tool::{Tool, ToolServices},
};

use ra_exec::{
    fs::{RootedFileSystem, Workspace},
    session::ProcessManager,
};
use ra_runtime::runner::RunConfig;
use ra_tools::capability::{
    ApplyPatchCapability, FilesystemCapability, SearchCapability, ShellCapability,
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
    process_manager: Arc<ProcessManager>,
    event_sink: Arc<dyn HostEventSink>,
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
            process_manager: Arc::new(ProcessManager::default()),
            event_sink: Arc::new(NoopHostEventSink),
        })
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
        ToolServices::new()
            .with_event_sink(Arc::clone(&self.event_sink))
            .with_output_projector(Arc::new(ToolResultBudget::default()))
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
            .with_model_input_projector(Arc::new(ToolOutputReferenceTrimmer::default()))
            .with_capability(Arc::new(CompactionCapability::default()))
    }

    /// Creates the workspace-confined editing capability.
    pub fn apply_patch_capability(&self) -> ra_core::error::Result<ApplyPatchCapability> {
        ApplyPatchCapability::for_workspace(&self.workspace)
    }

    /// Creates the workspace-confined file-reading capability.
    pub fn filesystem_capability(&self) -> ra_core::error::Result<FilesystemCapability> {
        FilesystemCapability::for_workspace(&self.workspace)
    }

    /// Creates the workspace-confined structured-discovery capability.
    pub fn search_capability(&self) -> ra_core::error::Result<SearchCapability> {
        SearchCapability::for_workspace(&self.workspace)
    }

    /// Creates the workspace-rooted execution capability on this host's session manager.
    ///
    /// The manager is this host's rather than the capability's own, so the sessions an agent starts
    /// are the ones this host cancels and reports at closeout.
    pub fn shell_capability(&self) -> ra_core::error::Result<ShellCapability> {
        ShellCapability::for_workspace(&self.workspace, Arc::clone(&self.process_manager))
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
