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
//! The built-in set names ten families. Five are backed here, `compaction` is backed by
//! `ra-context`, and the remaining four — `todo`, `view_image`, `web`, `skills` — have no
//! capability type because their tools are not written yet. A capability that contributed nothing
//! would be worse than a missing one: it would put its family in the installed set, so a dependency
//! on it would validate against a capability that does nothing, and a surface that is short several
//! entries would assemble without complaint. The families stay declared as
//! [`CapabilityFamily`] constants and unrepresented until there is something to represent.
//!
//! # Dependencies
//!
//! None of the five declares one. A declared dependency is a hard assembly error and an ordering
//! edge, and none of these five needs another to function: `apply_patch` creates files without
//! reading any, `grep` returns the matched lines rather than a path to go read, and a shell needs
//! nothing. Declaring `apply_patch` -> `filesystem` because editing usually follows reading would
//! refuse a legitimate configuration — one that reads through the shell — to record a habit.
//!
//! **Memory was expected to be the first real edge, and it is not.** The reasoning had been that a
//! memory capability cannot read its own store without one of the two families that reach the
//! filesystem — which is true of the arrangement where memory contributes prompt text and the model
//! reads the store with `read_file` or a shell command. [`MemoryCapability`] takes the other branch
//! and brings its own three entries, so the store is reached through a
//! [`MemoryStore`](ra_core::memory::MemoryStore) that no other family is involved in. The edge
//! disappears because the dependency did, not because it was overlooked: the arrangement that has
//! one is the arrangement where the memory root is just another path in the workspace, and that is
//! also the arrangement where nothing structural keeps an agent inside it.
//!
//! So this framework still has no dependency edge in its built-in set. The validation machinery is
//! not thereby unused — a third-party capability declares against these families — but the first
//! built-in edge is still ahead, and inventing one to exercise the mechanism would be the
//! `apply_patch` -> `filesystem` mistake with a different pair of names.
//!
//! # Why the prompt text is here, beside the tools rather than in a product crate
//!
//! Each capability's fragment says what its own entries do and how the pair of them relates — a
//! session `write_stdin` can address, context lines `apply_patch` matches, a result set `grep`
//! returns instead of raw text. That is a description of the mechanism, and the mechanism is what
//! this crate owns; it is the same content as each tool's own model-facing description, one level
//! up. What is *not* here is any statement about when an agent should reach for them, which is a
//! product's decision and lives in the product's own sections.
//!
//! Carrying it on the capability is what makes the two halves inseparable. The alternative — a
//! product paragraph that names `apply_patch`, gated on whether `apply_patch` happens to be
//! advertised — is a gate somebody has to remember to write, once per tool, at every assembly site.
//! Here the fragment cannot arrive without the entries it describes, because the same object
//! carries both.
//!
//! Each fragment declares its own share of the cached prefix. Together the five cost roughly 400
//! estimated tokens against 640 declared, which is room to rewrite a paragraph and not room to
//! teach a second topic under an existing heading.

use std::{fmt, sync::Arc};

use async_trait::async_trait;
use ra_core::{
    capability::{Capability, CapabilityFamily},
    error::Result,
    memory::MemoryStore,
    prompt::{PromptSection, SectionPosition, SectionStability},
    tool::Tool,
};
use ra_exec::{fs::Workspace, session::ProcessManager};

use crate::{
    apply_patch::ApplyPatchTool,
    exec_command::ExecCommandTool,
    glob::GlobTool,
    grep::GrepTool,
    memory::{MemoryListTool, MemoryReadTool, MemorySearchTool},
    read_file::ReadFileTool,
    write_stdin::WriteStdinTool,
};

/// Builds one capability's prefix fragment, attributed to the family that wrote it.
///
/// The name, the source, and the placement all come from the family rather than from each call
/// site: they are the three things assembly checks, and a fragment that had to restate them is a
/// fragment that can get one of them wrong.
fn fragment(
    family: &CapabilityFamily,
    purpose: &str,
    content: &str,
    token_budget: usize,
) -> Result<Option<PromptSection>> {
    PromptSection::new(
        family.prompt_section_name(),
        purpose,
        family.prompt_source(),
        SectionStability::Stable,
        SectionPosition::Prefix,
        content,
    )
    .map(|section| Some(section.with_token_budget(token_budget)))
}

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

/// Cached-prefix allowance for the shell fragment, in estimated tokens.
const SHELL_TOKEN_BUDGET: usize = 128;

/// What the two entries are to each other, which neither schema can state on its own.
///
/// The second sentence is the whole reason this pair is one capability: an agent that does not know
/// a running command keeps a session will answer an interactive prompt by starting a second
/// command, and read the same question again.
const SHELL_FRAGMENT: &str = "Commands:\n\
                              - `exec_command` runs a command from the workspace root and returns \
                              its output. A command that has not exited keeps its session, and the \
                              output names it.\n\
                              - `write_stdin` writes to a session `exec_command` started. It is \
                              the only way to answer a running command, and it cannot start one.";

#[async_trait]
impl Capability for ShellCapability {
    fn kind(&self) -> CapabilityFamily {
        CapabilityFamily::SHELL
    }

    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        vec![self.exec_command(), self.write_stdin()]
    }

    async fn static_instructions(&self) -> Result<Option<PromptSection>> {
        fragment(
            &self.kind(),
            "How the command-execution pair addresses a session",
            SHELL_FRAGMENT,
            SHELL_TOKEN_BUDGET,
        )
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

/// Cached-prefix allowance for the filesystem fragment, in estimated tokens.
const FILESYSTEM_TOKEN_BUDGET: usize = 96;

/// What the read entry accepts and what confines it.
///
/// The confinement is stated because a refused read is otherwise indistinguishable from a missing
/// file, and an agent that reads the two as the same thing spends a turn looking for a path it was
/// never going to be allowed to open.
const FILESYSTEM_FRAGMENT: &str = "Files:\n\
                                   - `read_file` returns workspace file contents. Paths are \
                                   relative to the workspace root, and a path leading outside it \
                                   is refused rather than empty.\n\
                                   - Ask for a line range when a file is large, and read the region \
                                   you are about to change before changing it.";

#[async_trait]
impl Capability for FilesystemCapability {
    fn kind(&self) -> CapabilityFamily {
        CapabilityFamily::FILESYSTEM
    }

    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        vec![self.read_file()]
    }

    async fn static_instructions(&self) -> Result<Option<PromptSection>> {
        fragment(
            &self.kind(),
            "What the file-reading entry accepts and what confines it",
            FILESYSTEM_FRAGMENT,
            FILESYSTEM_TOKEN_BUDGET,
        )
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

/// Cached-prefix allowance for the patch fragment, in estimated tokens.
const APPLY_PATCH_TOKEN_BUDGET: usize = 128;

/// How the entry reads a patch, and the one precondition a caller can violate silently.
///
/// Context matching is stated because it is the failure an agent cannot diagnose from the error
/// alone: a patch written against a remembered version of a file fails on text that looks correct
/// in the transcript, and the fix is to read the file again rather than to reword the patch.
///
/// It says nothing about *when* to edit, what to avoid editing with, or which operations the patch
/// format supports. A product that advertises this entry states the first two as policy, and the
/// third is in the tool's own schema — restating either here would put one rule in two spans of the
/// same cached prefix, where editing one leaves the model holding both versions of it.
const APPLY_PATCH_FRAGMENT: &str = "Patches:\n\
                                    - `apply_patch` takes one patch, and a single call may change \
                                    several files at once.\n\
                                    - Its context lines must match the file as it is on disk now, \
                                    so re-read a file that changed since you last saw it, and keep \
                                    each hunk to the lines you mean to change.";

#[async_trait]
impl Capability for ApplyPatchCapability {
    fn kind(&self) -> CapabilityFamily {
        CapabilityFamily::APPLY_PATCH
    }

    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        vec![self.apply_patch()]
    }

    async fn static_instructions(&self) -> Result<Option<PromptSection>> {
        fragment(
            &self.kind(),
            "How the patch entry reads a patch and what it matches against",
            APPLY_PATCH_FRAGMENT,
            APPLY_PATCH_TOKEN_BUDGET,
        )
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

/// Cached-prefix allowance for the search fragment, in estimated tokens.
const SEARCH_TOKEN_BUDGET: usize = 128;

/// What each of the two entries answers, and why neither is a command away.
///
/// The second line is the same argument that makes these one capability, addressed to the model
/// instead of to a host: an agent holding an execution entry can always run a search command, and
/// what comes back is raw text that no downstream reader can treat as a result set.
const SEARCH_FRAGMENT: &str = "Search:\n\
                               - `grep` matches file contents and returns the matching lines with \
                               their locations; `glob` matches path patterns and returns paths.\n\
                               - Prefer them to running a search command. A command answers with \
                               raw text, which neither the context budget nor a later replay can \
                               read as a result set.";

#[async_trait]
impl Capability for SearchCapability {
    fn kind(&self) -> CapabilityFamily {
        CapabilityFamily::SEARCH
    }

    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        vec![self.grep(), self.glob()]
    }

    async fn static_instructions(&self) -> Result<Option<PromptSection>> {
        fragment(
            &self.kind(),
            "What each discovery entry answers, and why neither is a command away",
            SEARCH_FRAGMENT,
            SEARCH_TOKEN_BUDGET,
        )
    }
}

/// Reading what earlier work concluded: finding it, reading it, and seeing what exists.
///
/// The three are one capability because they are one traversal — search to locate, read to see the
/// whole of what was located, list to learn what there is to search. A surface holding search alone
/// can find a line and never see the paragraph it sits in; one holding read alone requires the
/// model to already know a path it has no way to have learned.
///
/// **The store is a parameter, and that is what makes this family independent.** The three entries
/// reach a [`MemoryStore`] and nothing else, so installing memory neither requires nor implies a
/// capability that reaches the workspace, and an agent given memory cannot read past its root — not
/// because the prompt asked it not to, but because these entries cannot express a path the store
/// will not resolve.
pub struct MemoryCapability {
    search: Arc<MemorySearchTool>,
    read: Arc<MemoryReadTool>,
    list: Arc<MemoryListTool>,
}

impl fmt::Debug for MemoryCapability {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MemoryCapability")
            .finish_non_exhaustive()
    }
}

impl MemoryCapability {
    /// Creates the memory triad over one store.
    ///
    /// The store is shared rather than owned three times over, for the reason the shell pair shares
    /// one process manager: a listing and the read that follows it have to be answered by the same
    /// store, or a path the model was just handed comes back as missing.
    ///
    /// # Errors
    ///
    /// Returns a configuration error when any of the three identities or schemas cannot be built.
    pub fn new(store: Arc<dyn MemoryStore>) -> Result<Self> {
        Ok(Self {
            search: Arc::new(MemorySearchTool::new(Arc::clone(&store))?),
            read: Arc::new(MemoryReadTool::new(Arc::clone(&store))?),
            list: Arc::new(MemoryListTool::new(store)?),
        })
    }

    /// The entry that finds lines.
    #[must_use]
    pub fn search(&self) -> Arc<dyn Tool> {
        self.search.clone()
    }

    /// The entry that returns a whole document.
    #[must_use]
    pub fn read(&self) -> Arc<dyn Tool> {
        self.read.clone()
    }

    /// The entry that enumerates what the store holds.
    #[must_use]
    pub fn list(&self) -> Arc<dyn Tool> {
        self.list.clone()
    }
}

/// Cached-prefix allowance for the memory fragment, in estimated tokens.
const MEMORY_TOKEN_BUDGET: usize = 160;

/// What the store is, how the three entries relate, and the one thing a model must not assume.
///
/// The staleness line is here rather than in a product's policy because it is a property of the
/// mechanism: a store returns what an earlier run concluded, and nothing in the retrieval path
/// re-checks that it is still true. A model that reads memory as current fact will report a moved
/// function or a renamed flag with the confidence of something it just looked at.
///
/// **This text is a constant, and that is load-bearing.** It is resolved before any run exists and
/// lands in the cached prefix, so it must not vary with what the store holds — a fragment built by
/// reading the store would move the prefix every time memory changed, and one built per query would
/// move it every turn. What the store holds arrives as tool results, in the tail.
const MEMORY_FRAGMENT: &str = "Memory:\n\
                               - Stored memory holds what earlier runs concluded about this \
                               workspace. `memory_search` finds records, `memory_read` returns \
                               one, and `memory_list` shows what exists. Each result carries the \
                               identifier the other two take; send it back exactly, and do not \
                               invent one.\n\
                               - It records what was true when it was written. Verify anything you \
                               are about to act on, and say when an answer rests on memory you did \
                               not re-check.";

#[async_trait]
impl Capability for MemoryCapability {
    fn kind(&self) -> CapabilityFamily {
        CapabilityFamily::MEMORY
    }

    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        vec![self.search(), self.read(), self.list()]
    }

    async fn static_instructions(&self) -> Result<Option<PromptSection>> {
        fragment(
            &self.kind(),
            "What the memory entries reach, and why what they return is not current fact",
            MEMORY_FRAGMENT,
            MEMORY_TOKEN_BUDGET,
        )
    }
}
