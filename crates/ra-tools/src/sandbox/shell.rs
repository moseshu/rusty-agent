//! The shell capability: `exec_command`, and `write_stdin` where the session offers terminals.
//!
//! A port of the reference's `capabilities/shell.py`. The capability is bound to a sandbox session
//! when its agent is prepared, and builds its tools from that binding: `exec_command` always, bound
//! to the agent's user and working directory, and `write_stdin` only when the session can allocate
//! a terminal — offering it elsewhere would hand the model a tool that cannot do anything. A host
//! can adjust or replace the tools through [`Shell::with_configure_tools`] before they are handed
//! out, which is where per-tool approval is set.
//!
//! # Deviations from the reference
//!
//! - **Binding returns a bound copy.** The reference clones the capability per run and sets its
//!   session, user and scope on the clone. [`Capability::bind_sandbox`] returns the bound value
//!   instead, so nothing is cloned to be mutated.
//! - **An unbound capability contributes no tools rather than raising.** The reference raises from
//!   `tools()` when no session is bound; [`Capability::tools`] cannot fail. [`Shell::try_tools`]
//!   keeps the reference's error, and [`Capability::bind`] — the run-configuration route, the one
//!   place an unbound shell would be installed and then read — refuses an unbound shell with it.

use std::borrow::Cow;
use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use ra_core::{
    capability::{Capability, CapabilityFamily, SandboxBinding},
    context::RunContext,
    error::{Error, Result},
    prompt::{PromptSection, SectionPosition, SectionStability},
    sandbox::{SandboxSession, SandboxWorkspaceScope, User},
    tool::Tool,
};

use super::shell_tool::{ExecCommandTool, WriteStdinTool};

/// The guidance the capability adds to a sandbox agent's instructions, verbatim from the
/// reference.
pub const SHELL_INSTRUCTIONS: &str = "When using the shell:\n\
- Use `exec_command` for shell execution.\n\
- If available, use `write_stdin` to interact with or poll running sessions.\n\
- To interrupt a long-running process via `write_stdin`, start it with `tty=true` and send Ctrl-C \
(`\\u0003`).\n\
- Prefer `rg` and `rg --files` for text/file discovery when available.\n\
- Avoid using Python scripts just to print large file chunks.";

const UNBOUND: &str = "Shell capability is not bound to a SandboxSession";

/// The tools the shell capability is about to hand out, open to a configurator.
///
/// The reference's `ShellToolSet`: a mutable bundle, so a configurator can change a tool in place
/// or replace it.
#[derive(Debug, Clone)]
pub struct ShellToolSet {
    exec_command: ExecCommandTool,
    write_stdin: Option<WriteStdinTool>,
    workspace_scope: SandboxWorkspaceScope,
}

impl ShellToolSet {
    /// The command tool.
    #[must_use]
    pub const fn exec_command(&self) -> &ExecCommandTool {
        &self.exec_command
    }

    /// The command tool, to change in place.
    pub const fn exec_command_mut(&mut self) -> &mut ExecCommandTool {
        &mut self.exec_command
    }

    /// Replaces the command tool.
    pub fn set_exec_command(&mut self, exec_command: ExecCommandTool) {
        self.exec_command = exec_command;
    }

    /// The input tool, absent when the session offers no terminals.
    #[must_use]
    pub const fn write_stdin(&self) -> Option<&WriteStdinTool> {
        self.write_stdin.as_ref()
    }

    /// The input tool, to change in place.
    pub const fn write_stdin_mut(&mut self) -> Option<&mut WriteStdinTool> {
        self.write_stdin.as_mut()
    }

    /// Replaces or removes the input tool.
    pub fn set_write_stdin(&mut self, write_stdin: Option<WriteStdinTool>) {
        self.write_stdin = write_stdin;
    }

    /// Where the run's relative paths are measured from.
    #[must_use]
    pub const fn workspace_scope(&self) -> &SandboxWorkspaceScope {
        &self.workspace_scope
    }
}

/// Adjusts the shell's tools before they are handed out.
pub type ShellToolConfigurator = Arc<dyn Fn(&mut ShellToolSet) + Send + Sync>;

/// What a shell is bound to.
#[derive(Clone)]
struct ShellBinding {
    session: Arc<dyn SandboxSession>,
    run_as: Option<User>,
    workspace_scope: SandboxWorkspaceScope,
}

/// The shell capability.
#[derive(Clone, Default)]
pub struct Shell {
    configure_tools: Option<ShellToolConfigurator>,
    binding: Option<ShellBinding>,
}

impl fmt::Debug for Shell {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Shell")
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

impl Shell {
    /// An unbound shell with the reference's tools as they are.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Lets `configure` adjust or replace the tools each time they are built.
    #[must_use]
    pub fn with_configure_tools(
        mut self,
        configure: impl Fn(&mut ShellToolSet) + Send + Sync + 'static,
    ) -> Self {
        self.configure_tools = Some(Arc::new(configure));
        self
    }

    /// A copy bound to `session`, running commands as `run_as` from `workspace_scope`.
    #[must_use]
    pub fn bound(
        &self,
        session: Arc<dyn SandboxSession>,
        run_as: Option<User>,
        workspace_scope: SandboxWorkspaceScope,
    ) -> Self {
        Self {
            configure_tools: self.configure_tools.clone(),
            binding: Some(ShellBinding {
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
    pub fn toolset(&self) -> Result<ShellToolSet> {
        let binding = self
            .binding
            .as_ref()
            .ok_or_else(|| Error::config(UNBOUND))?;
        let exec_command = ExecCommandTool::new(Arc::clone(&binding.session))?
            .with_user(binding.run_as.clone())
            .with_workspace_scope(binding.workspace_scope.clone());
        let write_stdin = if binding.session.supports_pty() {
            Some(WriteStdinTool::new(Arc::clone(&binding.session))?)
        } else {
            None
        };
        let mut toolset = ShellToolSet {
            exec_command,
            write_stdin,
            workspace_scope: binding.workspace_scope.clone(),
        };
        if let Some(configure) = &self.configure_tools {
            configure(&mut toolset);
        }
        Ok(toolset)
    }

    /// The tools, `exec_command` first.
    ///
    /// # Errors
    ///
    /// Returns the reference's error when no session is bound.
    pub fn try_tools(&self) -> Result<Vec<Arc<dyn Tool>>> {
        let toolset = self.toolset()?;
        let mut tools: Vec<Arc<dyn Tool>> = vec![Arc::new(toolset.exec_command)];
        if let Some(write_stdin) = toolset.write_stdin {
            tools.push(Arc::new(write_stdin));
        }
        Ok(tools)
    }
}

#[async_trait]
impl Capability for Shell {
    fn kind(&self) -> CapabilityFamily {
        CapabilityFamily::SHELL
    }

    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        self.try_tools().unwrap_or_default()
    }

    async fn instructions(&self) -> Result<Option<PromptSection>> {
        let family = self.kind();
        PromptSection::new(
            family.prompt_section_name(),
            "sandbox shell guidance",
            family.prompt_source(),
            SectionStability::Stable,
            SectionPosition::Prefix,
            Cow::Borrowed(SHELL_INSTRUCTIONS),
        )
        .map(Some)
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
