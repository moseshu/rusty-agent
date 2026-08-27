//! Product host assembly, workspace capability binding, and event routing.

use std::{fmt, io, path::Path, sync::Arc};

use ra_core::{
    event::{HostEventEmitter, HostEventSink, NoopHostEventSink},
    item::AgentId,
    state::EventSeqAllocator,
    tool::{Tool, ToolServices},
};

use ra_exec::fs::RootedFileSystem;
use ra_tools::apply_patch::ApplyPatchTool;

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
    workspace_fs: Arc<RootedFileSystem>,
    event_sink: Arc<dyn HostEventSink>,
}

impl fmt::Debug for CodingHost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CodingHost")
            .field("workspace_fs", &"<RootedFileSystem>")
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
        let workspace_fs = Arc::new(RootedFileSystem::open(workspace_root)?);
        Ok(Self {
            workspace_fs,
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
        &self.workspace_fs
    }

    /// The host event sink for this host.
    #[must_use]
    pub fn event_sink(&self) -> &Arc<dyn HostEventSink> {
        &self.event_sink
    }

    /// Constructs [`ToolServices`] equipped with this host's event sink.
    pub fn build_tool_services(&self) -> ToolServices {
        ToolServices::new().with_event_sink(Arc::clone(&self.event_sink))
    }

    /// Creates the coding product's workspace-confined patch tool.
    pub fn apply_patch_tool(&self) -> ra_core::error::Result<Arc<dyn Tool>> {
        Ok(Arc::new(ApplyPatchTool::new(Arc::clone(
            &self.workspace_fs,
        ))?))
    }

    /// Creates an authenticated [`HostEventEmitter`] bound to this host's sink and the given allocator.
    #[must_use]
    pub fn emitter(&self, agent_id: AgentId, allocator: EventSeqAllocator) -> HostEventEmitter {
        HostEventEmitter::new(agent_id, allocator, Arc::clone(&self.event_sink))
    }
}
