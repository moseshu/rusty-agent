//! The built-in capabilities: one installable unit per family whose tools live in this crate.
//!
//! A [`Capability`] is the assembly unit — tools, prompt text, sampling settings, and a context
//! transform arrive together or not at all. The types below are the tool half of that promise for
//! the families this crate can back today, and they are here rather than in a product crate for the
//! same reason the tools are: nothing about `exec_command` or `grep` changes when the product does.
//!
//! # A capability is what a tool set means, not a bag holding one
//!
//! [`ShellCapability`] is the clearest case. `exec_command` starts a session and `write_stdin`
//! writes to one, and the second can only address a session the first started when both hold the
//! same [`ProcessManager`]. Installing the two tools separately makes that a fact a host has to know
//! and re-establish at every assembly site; installing the capability makes it structural — the
//! constructor takes one manager and hands it to both, and there is no arrangement of this type in
//! which the pair is mismatched.
//!
//! # What is not here yet, and why it is absent rather than empty
//!
//! The built-in set names ten families. Four are backed here, `compaction` is backed by
//! `ra-context`, and the remaining five — `todo`, `memory`, `view_image`, `web`, `skills` — have no
//! capability type because their tools are not written yet. A capability that contributed nothing
//! would be worse than a missing one: it would put its family in the installed set, so a dependency
//! on it would validate against a capability that does nothing, and a surface that is short several
//! entries would assemble without complaint. The families stay declared as
//! [`CapabilityFamily`] constants and unrepresented until there is something to represent.
//!
//! # Dependencies
//!
//! None of the four declares one. A declared dependency is a hard assembly error and an ordering
//! edge, and none of these four needs another to function: `apply_patch` creates files without
//! reading any, `grep` returns the matched lines rather than a path to go read, and a shell needs
//! nothing. Declaring `apply_patch` -> `filesystem` because editing usually follows reading would
//! refuse a legitimate configuration — one that reads through the shell — to record a habit. The
//! first real edge in this framework arrives with memory, which cannot read its own store without
//! one of the two capabilities that reach the filesystem.

use std::sync::Arc;

use ra_core::{
    capability::{Capability, CapabilityFamily},
    error::Result,
    tool::Tool,
};
use ra_exec::{fs::Workspace, session::ProcessManager};

use crate::{
    apply_patch::ApplyPatchTool, exec_command::ExecCommandTool, glob::GlobTool, grep::GrepTool,
    read_file::ReadFileTool, write_stdin::WriteStdinTool,
};

/// Command execution: starting a session and writing to one that is already running.
///
/// The two entries are one capability because they are one mechanism seen from two ends. A host
/// that installs only `exec_command` gets an agent that can start an interactive process and then
/// has no way to answer its prompt; one that installs only `write_stdin` gets an entry with nothing
/// to address.
#[derive(Debug, Clone)]
pub struct ShellCapability {
    exec_command: Arc<ExecCommandTool>,
    write_stdin: Arc<WriteStdinTool>,
}

impl ShellCapability {
    /// Creates the shell pair rooted in one workspace and sharing one process manager.
    ///
    /// The manager is a parameter rather than something this constructor invents, because process
    /// lifetime outlives any one capability: the host is what cancels sessions, collects background
    /// jobs, and reports them at closeout, and a manager owned privately here would hold sessions
    /// nothing else can see.
    ///
    /// # Errors
    ///
    /// Returns a configuration error when either tool's identity or schema cannot be built.
    pub fn for_workspace(
        workspace: &Workspace,
        process_manager: Arc<ProcessManager>,
    ) -> Result<Self> {
        Ok(Self {
            exec_command: Arc::new(
                ExecCommandTool::for_workspace(workspace)?
                    .with_manager(Arc::clone(&process_manager)),
            ),
            write_stdin: Arc::new(WriteStdinTool::new(process_manager)?),
        })
    }

    /// The entry that starts a command in the workspace.
    #[must_use]
    pub fn exec_command(&self) -> Arc<dyn Tool> {
        self.exec_command.clone()
    }

    /// The entry that writes to a session [`Self::exec_command`] started.
    #[must_use]
    pub fn write_stdin(&self) -> Arc<dyn Tool> {
        self.write_stdin.clone()
    }
}

impl Capability for ShellCapability {
    fn kind(&self) -> CapabilityFamily {
        CapabilityFamily::SHELL
    }

    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        vec![self.exec_command(), self.write_stdin()]
    }
}

/// Reading files out of the workspace.
///
/// One entry, and deliberately not the editing one: this framework advertises `apply_patch` as its
/// own family, so a read-only agent installs this capability and nothing changes about the fact
/// that it cannot write.
#[derive(Debug, Clone)]
pub struct FilesystemCapability {
    read_file: Arc<ReadFileTool>,
}

impl FilesystemCapability {
    /// Creates the file-reading entry confined to one workspace.
    ///
    /// # Errors
    ///
    /// Returns a configuration error when the tool's identity or schema cannot be built.
    pub fn for_workspace(workspace: &Workspace) -> Result<Self> {
        Ok(Self {
            read_file: Arc::new(ReadFileTool::for_workspace(workspace)?),
        })
    }

    /// The file-reading entry.
    #[must_use]
    pub fn read_file(&self) -> Arc<dyn Tool> {
        self.read_file.clone()
    }
}

impl Capability for FilesystemCapability {
    fn kind(&self) -> CapabilityFamily {
        CapabilityFamily::FILESYSTEM
    }

    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        vec![self.read_file()]
    }
}

/// Applying a V4A patch to the workspace: the editing entry.
///
/// Its own family rather than part of [`FilesystemCapability`], because the difference between an
/// agent that reads and one that writes is exactly this entry. Bundling the two would make
/// "install reading" and "install editing" the same decision, and a read-only surface would have to
/// be produced by filtering a capability's tools — which is the thing a capability exists to stop.
#[derive(Debug, Clone)]
pub struct ApplyPatchCapability {
    apply_patch: Arc<ApplyPatchTool>,
}

impl ApplyPatchCapability {
    /// Creates the patch entry confined to one workspace.
    ///
    /// # Errors
    ///
    /// Returns a configuration error when the tool's identity or schema cannot be built.
    pub fn for_workspace(workspace: &Workspace) -> Result<Self> {
        Ok(Self {
            apply_patch: Arc::new(ApplyPatchTool::for_workspace(workspace)?),
        })
    }

    /// The patch entry.
    #[must_use]
    pub fn apply_patch(&self) -> Arc<dyn Tool> {
        self.apply_patch.clone()
    }
}

impl Capability for ApplyPatchCapability {
    fn kind(&self) -> CapabilityFamily {
        CapabilityFamily::APPLY_PATCH
    }

    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        vec![self.apply_patch()]
    }
}

/// Structured discovery: text search and file-pattern lookup.
///
/// The two are one capability because they answer one question — where is it — and differ only in
/// what "it" is. A surface with `grep` and no `glob` sends the model to the shell for the half it is
/// missing, and a shell answer is raw text that neither the context budget nor offline evaluation
/// can read as a result set.
#[derive(Debug, Clone)]
pub struct SearchCapability {
    grep: Arc<GrepTool>,
    glob: Arc<GlobTool>,
}

impl SearchCapability {
    /// Creates the search pair confined to one workspace.
    ///
    /// # Errors
    ///
    /// Returns a configuration error when either tool's identity or schema cannot be built.
    pub fn for_workspace(workspace: &Workspace) -> Result<Self> {
        Ok(Self {
            grep: Arc::new(GrepTool::for_workspace(workspace)?),
            glob: Arc::new(GlobTool::for_workspace(workspace)?),
        })
    }

    /// The text-search entry.
    #[must_use]
    pub fn grep(&self) -> Arc<dyn Tool> {
        self.grep.clone()
    }

    /// The file-pattern lookup entry.
    #[must_use]
    pub fn glob(&self) -> Arc<dyn Tool> {
        self.glob.clone()
    }
}

impl Capability for SearchCapability {
    fn kind(&self) -> CapabilityFamily {
        CapabilityFamily::SEARCH
    }

    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        vec![self.grep(), self.glob()]
    }
}
