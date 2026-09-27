//! The filesystem capability: `view_image` and `apply_patch`.
//!
//! A port of the reference's `capabilities/filesystem.py`, and of `capabilities/capabilities.py`'s
//! default set. The capability is bound to a sandbox session when its agent is prepared, and builds
//! both tools from that binding — the agent's user and working directory — then lets a host adjust
//! or replace them through [`Filesystem::with_configure_tools`], which is where approval is set. It
//! adds no instructions.
//!
//! The model reads files through `exec_command`; the reference has no read, grep or glob tool, and
//! neither does this capability. The coding product's tools of those names are its own extension.
//!
//! # Deviations from the reference
//!
//! As for [`super::shell::Shell`]: binding returns a bound copy, and an unbound capability
//! contributes no tools rather than raising — [`Filesystem::try_tools`] keeps the reference's error,
//! and [`Capability::bind`] refuses an unbound capability with it.

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use ra_core::{
    capability::{Capability, CapabilityFamily, SandboxBinding},
    context::RunContext,
    error::{Error, Result},
    prompt::PromptSection,
    sandbox::{SandboxSession, SandboxWorkspaceScope, User},
    tool::Tool,
};

use super::apply_patch_tool::SandboxApplyPatchTool;
use super::shell::Shell;
use super::view_image::ViewImageTool;

const UNBOUND: &str = "Filesystem capability is not bound to a SandboxSession";

/// The tools the filesystem capability is about to hand out, open to a configurator.
///
/// The reference's `FilesystemToolSet`.
#[derive(Debug, Clone)]
pub struct FilesystemToolSet {
    view_image: ViewImageTool,
    apply_patch: SandboxApplyPatchTool,
    workspace_scope: SandboxWorkspaceScope,
}

impl FilesystemToolSet {
    /// The image tool.
    #[must_use]
    pub const fn view_image(&self) -> &ViewImageTool {
        &self.view_image
    }

    /// The image tool, to change in place.
    pub const fn view_image_mut(&mut self) -> &mut ViewImageTool {
        &mut self.view_image
    }

    /// Replaces the image tool.
    pub fn set_view_image(&mut self, view_image: ViewImageTool) {
        self.view_image = view_image;
    }

    /// The patch tool.
    #[must_use]
    pub const fn apply_patch(&self) -> &SandboxApplyPatchTool {
        &self.apply_patch
    }

    /// The patch tool, to change in place.
    pub const fn apply_patch_mut(&mut self) -> &mut SandboxApplyPatchTool {
        &mut self.apply_patch
    }

    /// Replaces the patch tool.
    pub fn set_apply_patch(&mut self, apply_patch: SandboxApplyPatchTool) {
        self.apply_patch = apply_patch;
    }

    /// Where the run's relative paths are measured from.
    #[must_use]
    pub const fn workspace_scope(&self) -> &SandboxWorkspaceScope {
        &self.workspace_scope
    }
}

/// Adjusts the filesystem tools before they are handed out.
pub type FilesystemToolConfigurator = Arc<dyn Fn(&mut FilesystemToolSet) + Send + Sync>;

/// What a filesystem capability is bound to.
#[derive(Clone)]
struct FilesystemBinding {
    session: Arc<dyn SandboxSession>,
    run_as: Option<User>,
    workspace_scope: SandboxWorkspaceScope,
}

/// The filesystem capability.
#[derive(Clone, Default)]
pub struct Filesystem {
    configure_tools: Option<FilesystemToolConfigurator>,
    binding: Option<FilesystemBinding>,
}

impl fmt::Debug for Filesystem {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Filesystem")
            .field("configure_tools", &self.configure_tools.is_some())
            .field(
                "session",
                &self
                    .binding
                    .as_ref()
                    .map(|binding| binding.session.backend_id().to_owned()),
            )
            .finish_non_exhaustive()
    }
}

impl Filesystem {
    /// An unbound capability with the reference's tools as they are.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Lets `configure` adjust or replace the tools each time they are built.
    #[must_use]
    pub fn with_configure_tools(
        mut self,
        configure: impl Fn(&mut FilesystemToolSet) + Send + Sync + 'static,
    ) -> Self {
        self.configure_tools = Some(Arc::new(configure));
        self
    }

    /// A copy bound to `session`, acting as `run_as` from `workspace_scope`.
    #[must_use]
    pub fn bound(
        &self,
        session: Arc<dyn SandboxSession>,
        run_as: Option<User>,
        workspace_scope: SandboxWorkspaceScope,
    ) -> Self {
        Self {
            configure_tools: self.configure_tools.clone(),
            binding: Some(FilesystemBinding {
                session,
                run_as,
                workspace_scope,
            }),
        }
    }

    /// Whether a session is bound.
    #[must_use]
    pub const fn is_bound(&self) -> bool {
        self.binding.is_some()
    }

    /// Builds the tool set and runs the configurator over it.
    ///
    /// # Errors
    ///
    /// Returns the reference's error when no session is bound.
    pub fn toolset(&self) -> Result<FilesystemToolSet> {
        let binding = self
            .binding
            .as_ref()
            .ok_or_else(|| Error::config(UNBOUND))?;
        let mut toolset = FilesystemToolSet {
            view_image: ViewImageTool::new(Arc::clone(&binding.session))?
                .with_user(binding.run_as.clone())
                .with_workspace_scope(binding.workspace_scope.clone()),
            apply_patch: SandboxApplyPatchTool::new(Arc::clone(&binding.session))?
                .with_user(binding.run_as.clone())
                .with_workspace_scope(binding.workspace_scope.clone()),
            workspace_scope: binding.workspace_scope.clone(),
        };
        if let Some(configure) = &self.configure_tools {
            configure(&mut toolset);
        }
        Ok(toolset)
    }

    /// The tools, `view_image` first.
    ///
    /// # Errors
    ///
    /// Returns the reference's error when no session is bound.
    pub fn try_tools(&self) -> Result<Vec<Arc<dyn Tool>>> {
        let toolset = self.toolset()?;
        Ok(vec![
            Arc::new(toolset.view_image),
            Arc::new(toolset.apply_patch),
        ])
    }
}

#[async_trait]
impl Capability for Filesystem {
    fn kind(&self) -> CapabilityFamily {
        CapabilityFamily::FILESYSTEM
    }

    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        self.try_tools().unwrap_or_default()
    }

    async fn instructions(&self) -> Result<Option<PromptSection>> {
        Ok(None)
    }

    fn bind(&self, context: &RunContext) -> Result<Option<Arc<dyn Capability>>> {
        let _ = context;
        if self.binding.is_none() {
            return Err(Error::config(UNBOUND));
        }
        Ok(None)
    }

    fn bind_sandbox(&self, binding: &SandboxBinding) -> Result<Option<Arc<dyn Capability>>> {
        Ok(Some(Arc::new(self.bound(
            Arc::clone(binding.session()),
            binding.run_as().cloned(),
            binding.workspace_scope().clone(),
        ))))
    }
}

/// The capabilities a sandbox agent gets when its host names none.
///
/// The reference's `Capabilities.default()`: `[Filesystem(), Shell(), Compaction()]`, in that order.
#[must_use]
pub fn default_capabilities() -> Vec<Arc<dyn Capability>> {
    vec![
        Arc::new(Filesystem::new()),
        Arc::new(Shell::new()),
        Arc::new(super::compaction::Compaction::new()),
    ]
}
