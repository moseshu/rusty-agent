//! The memory capability: memory files kept in the sandbox workspace, read into the instructions.
//!
//! A port of the reference's `sandbox/capabilities/memory.py` and the read half of
//! `sandbox/memory/prompts.py`. Memory lives under the workspace as files: `memory_summary.md`, a
//! short summary, `MEMORY.md`, the searchable registry, and per-rollout summaries and skills. When
//! the agent is prepared, the summary is read from the session, truncated to 15,000 tokens and
//! embedded in the reference's memory prompt, word for word; the model then searches the rest with
//! its shell, and — with live updates on — edits `MEMORY.md` when it finds it stale. Hence the
//! dependencies: reading needs `shell`, and live updates need `filesystem` as well.
//!
//! Generating memory — appending run segments and extracting and consolidating them when the
//! session closes — is configured here, by [`Memory::generate`], and run by the runtime, which
//! reads the configuration through [`Capability::sandbox_memory`]. The extraction and
//! consolidation agents run with [`super::filesystem::default_capabilities`], the capabilities the
//! reference's sandbox agents get by default.
//!
//! # Not the memory store
//!
//! [`crate::capability::MemoryCapability`] is the retrieval surface a host's
//! [`MemoryStore`](ra_core::memory::MemoryStore) answers through three dedicated tools. This
//! capability contributes no tools: its memory is workspace files the model reads like any other.
//! Both belong to the `memory` family, so an agent is given one or the other.
//!
//! # Deviations from the reference
//!
//! - **Built by [`MemoryBuilder`], checked when built.** The reference checks the configuration
//!   when the model is constructed and raises `ValueError`; [`MemoryBuilder::build`] runs the same
//!   checks with the same wording and returns a configuration error.
//! - **Binding returns a bound copy**, as for [`super::shell::Shell`]; [`Memory::bound_to`],
//!   [`Memory::with_run_as`] and [`Memory::with_workspace_scope`] are the reference's three binds.
//! - **The instructions are rendered from the manifest the binding carries.** The reference's
//!   `instructions(manifest)` is [`Memory::instructions_for`]; [`Capability::instructions`] calls
//!   it with the manifest the session had when the agent was prepared. An unbound capability is
//!   refused on the run configuration, as the shell is.

use std::collections::BTreeSet;
use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use ra_core::{
    capability::{Capability, CapabilityFamily, SandboxBinding},
    context::RunContext,
    error::{Error, Result, SandboxErrorKind},
    prompt::{PromptSection, SectionPosition, SectionStability},
    sandbox::{
        ErrorCode, Manifest, MemoryGenerateConfig, MemoryLayoutConfig, MemoryReadConfig, PosixPath,
        SandboxMemory, SandboxSession, SandboxWorkspaceScope, SessionPath, User,
        token_truncation::{TruncationPolicy, truncate_text},
    },
};

/// The most tokens of `memory_summary.md` the instructions embed.
pub const MEMORY_SUMMARY_MAX_TOKENS: i64 = 15_000;

/// The file the instructions embed, under the memories directory.
pub const MEMORY_SUMMARY_FILE: &str = "memory_summary.md";

/// The reference's `memory_read_prompt.md` (MIT), carried verbatim. The license is in the
/// repository's `THIRD_PARTY_NOTICES.md`.
pub const MEMORY_READ_PROMPT_TEMPLATE: &str = include_str!("memory_read_prompt.md");

/// What the prompt says when live updates are off, verbatim from the reference.
pub const MEMORY_READ_ONLY_INSTRUCTIONS: &str = "Never update memories. You can only read them.";

/// What the prompt says when live updates are on, verbatim from the reference; `{memory_dir}` is
/// replaced with the memories directory.
pub const MEMORY_LIVE_UPDATE_INSTRUCTIONS: &str = "When to update memory (automatic, same turn; \
required):

- Treat memory as guidance, not truth: if memory conflicts with current workspace
  state, tool outputs, environment, or user feedback, current evidence wins.
- Memory is writable. You are authorized to edit {memory_dir}/MEMORY.md when stale
  guidance is detected.
- If any memory fact conflicts with current evidence, you MUST update memory in the
  same turn. Do not wait for a separate user prompt.
- If you detect stale memory, updating {memory_dir}/MEMORY.md is part of task
  completion, not optional cleanup.
- Required behavior after detecting stale memory:
  1. Verify the correct replacement using local evidence.
  2. Continue the task using current evidence; do not rely on stale memory.
  3. Edit {memory_dir}/MEMORY.md later in the same turn, before your final response.
  4. Finalize the task after the memory update is written.";

const UNBOUND: &str = "Memory capability is not bound to a SandboxSession";

/// The reference's `render_memory_read_prompt`.
///
/// The placeholders are replaced one after another, in the reference's order — the directory, then
/// the update instructions, then the summary — so text substituted early is seen by the later
/// replacements exactly as it is there.
#[must_use]
pub fn render_memory_read_prompt(
    memory_dir: &str,
    memory_summary: &str,
    live_update: bool,
) -> String {
    let update_instructions = if live_update {
        MEMORY_LIVE_UPDATE_INSTRUCTIONS.replace("{memory_dir}", memory_dir)
    } else {
        MEMORY_READ_ONLY_INSTRUCTIONS.to_owned()
    };
    MEMORY_READ_PROMPT_TEMPLATE
        .replace("{memory_dir}", memory_dir)
        .replace("{memory_update_instructions}", &update_instructions)
        .replace("{memory_summary}", memory_summary)
}

/// Checks one layout directory as the reference does: relative, inside the root, and naming
/// something.
///
/// Checked as the session will resolve it, with a backslash read as a separator, so a directory
/// that would leave the workspace once read that way is refused here rather than at first use.
fn validate_layout_path(name: &str, parsed: &PosixPath) -> Result<()> {
    if parsed.is_absolute() {
        return Err(Error::config(format!(
            "{name} must be relative to the sandbox workspace root, got: {parsed}"
        )));
    }
    if parsed.parts().contains(&"..") {
        return Err(Error::config(format!(
            "{name} must not escape root, got: {parsed}"
        )));
    }
    if parsed.parts().is_empty() {
        return Err(Error::config(format!("{name} must be non-empty")));
    }
    Ok(())
}

/// Python's `str.strip()`: Unicode whitespace and the four information separators.
fn python_strip(text: &str) -> &str {
    text.trim_matches(|character: char| {
        character.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&character)
    })
}

/// Collects a memory capability's configuration, and checks it when it is built.
#[derive(Debug, Clone)]
pub struct MemoryBuilder {
    layout: MemoryLayoutConfig,
    read: Option<MemoryReadConfig>,
    generate: Option<MemoryGenerateConfig>,
}

impl Default for MemoryBuilder {
    fn default() -> Self {
        Self {
            layout: MemoryLayoutConfig::default(),
            read: Some(MemoryReadConfig::default()),
            generate: Some(MemoryGenerateConfig::default()),
        }
    }
}

impl MemoryBuilder {
    /// Keeps memory files where `layout` says.
    #[must_use]
    pub fn layout(mut self, layout: MemoryLayoutConfig) -> Self {
        self.layout = layout;
        self
    }

    /// Reads memory as `read` says, or not at all when `None`.
    #[must_use]
    pub const fn read(mut self, read: Option<MemoryReadConfig>) -> Self {
        self.read = read;
        self
    }

    /// Generates memory as `generate` says, or not at all when `None`.
    #[must_use]
    pub fn generate(mut self, generate: Option<MemoryGenerateConfig>) -> Self {
        self.generate = generate;
        self
    }

    /// Checks the configuration and builds the capability.
    ///
    /// # Errors
    ///
    /// Returns a configuration error, worded as the reference's, when neither reading nor
    /// generating is configured, and for a layout directory that is absolute, climbs out of the
    /// workspace or names nothing.
    pub fn build(self) -> Result<Memory> {
        if self.read.is_none() && self.generate.is_none() {
            return Err(Error::config(
                "Memory requires at least one of `read` or `generate`.",
            ));
        }
        validate_layout_path("layout.memories_dir", &self.layout.memories_path())?;
        validate_layout_path("layout.sessions_dir", &self.layout.sessions_path())?;
        Ok(Memory {
            layout: self.layout,
            read: self.read,
            generate: self.generate,
            session: None,
            run_as: None,
            workspace_scope: SandboxWorkspaceScope::root(),
            manifest: None,
        })
    }
}

/// The memory capability.
#[derive(Clone)]
pub struct Memory {
    layout: MemoryLayoutConfig,
    read: Option<MemoryReadConfig>,
    generate: Option<MemoryGenerateConfig>,
    session: Option<Arc<dyn SandboxSession>>,
    run_as: Option<User>,
    workspace_scope: SandboxWorkspaceScope,
    manifest: Option<Manifest>,
}

impl fmt::Debug for Memory {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Memory")
            .field("layout", &self.layout)
            .field("read", &self.read)
            .field("generate", &self.generate)
            .field(
                "session",
                &self
                    .session
                    .as_ref()
                    .map(|session| session.backend_id().to_owned()),
            )
            .field("run_as", &self.run_as)
            .field("workspace_scope", &self.workspace_scope)
            .finish_non_exhaustive()
    }
}

impl Default for Memory {
    fn default() -> Self {
        Self::new()
    }
}

impl Memory {
    /// The reference's default: the default layout, read with live updates, and generated.
    #[must_use]
    pub fn new() -> Self {
        Self {
            layout: MemoryLayoutConfig::default(),
            read: Some(MemoryReadConfig::default()),
            generate: Some(MemoryGenerateConfig::default()),
            session: None,
            run_as: None,
            workspace_scope: SandboxWorkspaceScope::root(),
            manifest: None,
        }
    }

    /// Starts a configuration from the reference's default.
    #[must_use]
    pub fn builder() -> MemoryBuilder {
        MemoryBuilder::default()
    }

    /// Where memory files are kept.
    #[must_use]
    pub const fn layout(&self) -> &MemoryLayoutConfig {
        &self.layout
    }

    /// How memory is read, or `None` when it is not.
    #[must_use]
    pub const fn read(&self) -> Option<&MemoryReadConfig> {
        self.read.as_ref()
    }

    /// How memory is generated, or `None` when it is not.
    #[must_use]
    pub const fn generate(&self) -> Option<&MemoryGenerateConfig> {
        self.generate.as_ref()
    }

    /// Whether a session is bound.
    #[must_use]
    pub const fn is_bound(&self) -> bool {
        self.session.is_some()
    }

    /// A copy bound to `session`.
    #[must_use]
    pub fn bound_to(&self, session: Arc<dyn SandboxSession>) -> Self {
        Self {
            session: Some(session),
            ..self.clone()
        }
    }

    /// Reads memory as `run_as`.
    #[must_use]
    pub fn with_run_as(mut self, run_as: Option<User>) -> Self {
        self.run_as = run_as;
        self
    }

    /// Renders paths for a run working from `workspace_scope`.
    #[must_use]
    pub fn with_workspace_scope(mut self, workspace_scope: SandboxWorkspaceScope) -> Self {
        self.workspace_scope = workspace_scope;
        self
    }

    /// The memory prompt for a session whose manifest is `manifest`, or `None` when reading is off
    /// or there is no summary to embed.
    ///
    /// The reference's `instructions(manifest)`.
    ///
    /// # Errors
    ///
    /// Returns a configuration error when reading is on and no session is bound, the session's
    /// failure to read the summary other than its absence, and a configuration error when a run
    /// working directory is set and the memories directory cannot be rendered under the root.
    pub async fn instructions_for(&self, manifest: &Manifest) -> Result<Option<String>> {
        let Some(read) = &self.read else {
            return Ok(None);
        };
        let Some(session) = &self.session else {
            return Err(Error::config(UNBOUND));
        };

        let memory_dir = self.layout.memories_path();
        let summary_path = memory_dir.join(MEMORY_SUMMARY_FILE);
        let payload = match session
            .read(SessionPath::Posix(&summary_path), self.run_as.clone())
            .await
        {
            Ok(payload) => payload,
            Err(error) if error.error_code() == ErrorCode::WorkspaceReadNotFound => {
                return Ok(None);
            }
            Err(error) => {
                return Err(
                    Error::sandbox(SandboxErrorKind::Setup, error.to_string()).with_source(error)
                );
            }
        };

        let memory_summary = truncate_text(
            python_strip(&String::from_utf8_lossy(&payload)),
            TruncationPolicy::tokens(MEMORY_SUMMARY_MAX_TOKENS),
        );
        if memory_summary.is_empty() {
            return Ok(None);
        }

        // Without a working directory the prompt keeps the configured spelling, as the
        // reference's does; that spelling names the directory the summary was read from, since
        // the session was handed it as a path.
        let model_memory_dir = if self.workspace_scope.cwd().is_none() {
            self.layout.memories_dir().to_owned()
        } else {
            self.workspace_scope
                .model_resource_path(&manifest.root, &memory_dir)
                .map_err(|error| Error::config(error.to_string()))?
                .into()
        };
        Ok(Some(render_memory_read_prompt(
            &model_memory_dir,
            &memory_summary,
            read.live_update(),
        )))
    }
}

#[async_trait]
impl Capability for Memory {
    fn kind(&self) -> CapabilityFamily {
        CapabilityFamily::MEMORY
    }

    fn required_capabilities(&self) -> BTreeSet<CapabilityFamily> {
        match &self.read {
            None => BTreeSet::new(),
            Some(read) if read.live_update() => {
                BTreeSet::from([CapabilityFamily::FILESYSTEM, CapabilityFamily::SHELL])
            }
            Some(_) => BTreeSet::from([CapabilityFamily::SHELL]),
        }
    }

    async fn instructions(&self) -> Result<Option<PromptSection>> {
        let Some(manifest) = &self.manifest else {
            return Ok(None);
        };
        let Some(text) = self.instructions_for(manifest).await? else {
            return Ok(None);
        };
        let family = self.kind();
        PromptSection::new(
            family.prompt_section_name(),
            "sandbox memory",
            family.prompt_source(),
            SectionStability::Stable,
            SectionPosition::Prefix,
            text,
        )
        .map(Some)
    }

    fn bind(&self, context: &RunContext) -> Result<Option<Arc<dyn Capability>>> {
        let _ = context;
        if self.session.is_none() {
            return Err(Error::config(UNBOUND));
        }
        Ok(None)
    }

    fn sandbox_memory(&self) -> Option<SandboxMemory> {
        Some(SandboxMemory::new(
            self.layout.clone(),
            self.generate.clone(),
            super::filesystem::default_capabilities(),
        ))
    }

    fn bind_sandbox(&self, binding: &SandboxBinding) -> Result<Option<Arc<dyn Capability>>> {
        let mut bound = self
            .bound_to(Arc::clone(binding.session()))
            .with_run_as(binding.run_as().cloned())
            .with_workspace_scope(binding.workspace_scope().clone());
        bound.manifest = Some(binding.manifest().clone());
        Ok(Some(Arc::new(bound)))
    }
}
