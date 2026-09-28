//! The skills capability: skills placed in the workspace, an index of them in the instructions, and
//! `load_skill` for skills staged on demand.
//!
//! A port of the reference's `sandbox/capabilities/skills.py`. A skill is a directory with a
//! `SKILL.md` and optional `scripts/`, `references/` and `assets/`. The capability puts skills
//! under one directory of the workspace — `.agents` unless told otherwise, the root Codex discovers
//! skills in — from exactly one of three sources:
//!
//! - **Literal skills** ([`Skill`]), each added to the manifest as a directory of its own.
//! - **An entry** that already holds skill directories — a `local_dir`, a `git_repo`, a `dir` — added
//!   to the manifest at the skills path. The index is read back from the workspace once the session
//!   exists, each skill's name and description taken from its `SKILL.md` frontmatter.
//! - **A lazy source** ([`LazySkillSource`]) that is indexed up front and materializes nothing:
//!   the model calls `load_skill` for a skill before reading it, and exactly that skill is staged.
//!   The built-in one reads a host directory; it is `ra_sandbox::skills::LocalDirLazySkillSource`,
//!   kept with the backends because reading the host is theirs to do.
//!
//! The index lists each skill as `- name: description (file: path)`, sorted by name, followed by
//! the reference's guidance on when and how to use a skill, word for word. With a run working
//! directory, paths are rendered absolute — a skill belongs to the session, not to the run's
//! directory — and a section says so.
//!
//! # Not the coding product's `skill` tool
//!
//! [`crate::skill`] and [`crate::capability::SkillsCapability`] are the coding product's: skill
//! bodies come from a host [`SkillCatalog`](ra_core::skill::SkillCatalog) and are returned as tool
//! output. This capability is the sandbox one: skills are files in the workspace and the model reads
//! them there. Both belong to the `skills` family, so an agent is given one or the other.
//!
//! # Deviations from the reference
//!
//! - **Built by [`SkillsBuilder`], validated when built.** The reference validates its three source
//!   fields and the skills path when the model is constructed; [`SkillsBuilder::build`] runs the same
//!   checks, with the same messages and context, in the same order.
//! - **Binding returns a bound copy**, as for [`super::shell::Shell`]. [`Skills::bound_to`] is the
//!   reference's `bind`: the copy starts with an empty metadata cache. [`Skills::with_run_as`] and
//!   [`Skills::with_workspace_scope`] are its `bind_run_as` and `bind_workspace_scope`.
//! - **The index is rendered from the manifest the binding carries.** The reference's
//!   `instructions(manifest)` is [`Skills::instructions_for`]; [`Capability::instructions`] calls it
//!   with the manifest the session had when the agent was prepared, and an unbound capability has
//!   no index to give.
//! - **An unbound capability contributes no tools rather than raising**, as for the shell:
//!   [`Skills::try_tools`] keeps the reference's error, and [`Capability::bind`] refuses an unbound
//!   capability with it.
//! - **`load_skill` before binding fails with `sandbox_config_invalid`.** The reference raises a
//!   plain `ValueError`; [`Skills::load_skill`] returns a [`SandboxError`], so the same message
//!   travels under that code.
//! - **`load_skill` answers with JSON.** The reference returns the source's dictionary and the
//!   model sees Python's rendering of it; here the model sees the same keys and values, in the same
//!   order, as a JSON object.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use ra_core::{
    capability::{Capability, CapabilityFamily, SandboxBinding},
    context::RunContext,
    error::{Error, Result, SandboxErrorKind, ToolErrorKind},
    prompt::{PromptSection, SectionPosition, SectionStability},
    sandbox::{
        Entry, EntryContent, ErrorCode, LazySkillSource, Manifest, NO_SKILL_DESCRIPTION, OpName,
        PosixPath, SKILL_MARKDOWN, SandboxError, SandboxResult, SandboxSession,
        SandboxWorkspaceScope, SessionPath, SkillLoadResult, SkillMetadata, User,
        parse_skill_frontmatter, windows_absolute_path,
    },
    tool::{FuncSchema, Tool, ToolContext, ToolOptions, ToolOrigin, ToolOutput, ToolSchema},
};
use ra_macros::ToolInput;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{NeedsApproval, decode, sandbox_tool_options, session_failure};

/// Where skills are placed unless told otherwise, relative to the workspace root.
pub const DEFAULT_SKILLS_PATH: &str = ".agents";

/// The name `load_skill` is advertised under.
pub const LOAD_SKILL_TOOL_NAME: &str = "load_skill";

/// What `load_skill` tells the model it does, verbatim from the reference.
pub const LOAD_SKILL_DESCRIPTION: &str = "Load a single lazily configured skill into the sandbox so \
its SKILL.md, scripts, references, and assets can be read from the workspace.";

const UNBOUND: &str = "Skills is not bound to a SandboxSession";

const SKILLS_SECTION_INTRO: &str = "A skill is a set of local instructions to follow that is stored \
in a `SKILL.md` file. Below is the list of skills that can be used. Each entry includes a name, \
description, and file path so you can open the source for full instructions when using a specific \
skill.";

const SKILL_PATH_GUIDANCE: [&str; 2] = [
    "- Skill paths: Treat each listed path as the skill root. Resolve relative paths in \
`SKILL.md`, including `scripts/`, `references/`, and `assets/`, against that root rather than the \
shell working directory.",
    "- Shared resources: Skill files belong to the sandbox session and may be visible to other \
runs. Unless the task explicitly requires editing a skill, invoke scripts through the listed skill \
root and write task inputs, outputs, caches, and temporary files in the run working directory.",
];

const SCOPED_SKILL_PATH_ERROR: &str =
    "skill path must be non-empty and workspace-relative when sandbox.cwd is configured";

/// The lines both how-to sections share after their first three.
const HOW_TO_USE_SHARED_TAIL: [&str; 11] = [
    "  2) If `SKILL.md` points to extra folders such as `references/`, load only the specific \
files needed for the request; don't bulk-load everything.",
    "  3) If `scripts/` exist, prefer running or patching them instead of retyping large code \
blocks.",
    "  4) If `assets/` or templates exist, reuse them instead of recreating from scratch.",
    "- Coordination and sequencing:",
    "  - If multiple skills apply, choose the minimal set that covers the request and state the \
order you'll use them.",
    "  - Announce which skill(s) you're using and why (one short line). If you skip an obvious \
skill, say why.",
    "- Context hygiene:",
    "  - Keep context small: summarize long sections instead of pasting them; only load extra \
files when needed.",
    "  - Avoid deep reference-chasing: prefer opening only files directly linked from `SKILL.md` \
unless you're blocked.",
    "  - When variants exist (frameworks, providers, domains), pick only the relevant reference \
file(s) and note that choice.",
    "- Safety and fallback: If a skill can't be applied cleanly (missing files, unclear \
instructions), state the issue, pick the next-best approach, and continue.",
];

const TRIGGER_RULES: &str = "- Trigger rules: If the user names a skill (with `$SkillName` or \
plain text) OR the task clearly matches a skill's description shown above, you must use that skill \
for that turn. Multiple mentions mean use them all. Do not carry skills across turns unless \
re-mentioned.";

const MISSING_OR_BLOCKED: &str = "- Missing/blocked: If a named skill isn't in the list or the path \
can't be read, say so briefly and continue with the best fallback.";

const PROGRESSIVE_DISCLOSURE: &str = "- How to use a skill (progressive disclosure):";

const LAZY_LOADING_SECTION: [&str; 4] = [
    "### Lazy loading",
    "- These skills are indexed for planning, but they are not materialized in the workspace yet.",
    "- Call `load_skill` with a single skill name from the list before reading its `SKILL.md` or \
other files from the workspace.",
    "- `load_skill` stages exactly one skill under the listed path. If you need more than one \
skill, call it multiple times.",
];

/// The reference's `_HOW_TO_USE_SKILLS_SECTION` or `_HOW_TO_USE_LAZY_SKILLS_SECTION`.
fn how_to_use_section(lazy: bool) -> String {
    let (discovery, first_step) = if lazy {
        (
            "- Discovery: The list above is the skill index available in this session (name + \
description + workspace path). In lazy mode, those paths are loaded on demand instead of being \
present up front.",
            "  1) After deciding to use a lazy skill, call `load_skill` for that skill first, then \
open its `SKILL.md`.",
        )
    } else {
        (
            "- Discovery: The list above is the skills available in this session (name + \
description + file path). Skill bodies live on disk at the listed paths.",
            "  1) After deciding to use a skill, open its `SKILL.md`. Read only enough to follow \
the workflow.",
        )
    };
    [
        "### How to use skills",
        discovery,
        TRIGGER_RULES,
        MISSING_OR_BLOCKED,
        PROGRESSIVE_DISCLOSURE,
        first_step,
    ]
    .into_iter()
    .chain(HOW_TO_USE_SHARED_TAIL)
    .collect::<Vec<_>>()
    .join("\n")
}

/// The reference's `_validate_relative_path`: a path that names somewhere below the skills root.
///
/// `context` is added to the failure's own `field`, `path` and `reason`.
fn validate_relative_path(
    value: &str,
    field_name: &str,
    context: &[(&str, Value)],
) -> SandboxResult<PosixPath> {
    let refuse = |message: String, path: &str, reason: &str| {
        let mut error = SandboxError::skills_config(message)
            .with_context("field", field_name)
            .with_context("path", path)
            .with_context("reason", reason);
        for (key, value) in context {
            error = error.with_context(*key, value.clone());
        }
        error
    };
    if let Some(windows_path) = windows_absolute_path(value) {
        return Err(refuse(
            format!("{field_name} must be a relative path"),
            &windows_path,
            "absolute",
        ));
    }
    let relative = PosixPath::coerce(value);
    if relative.is_absolute() {
        return Err(refuse(
            format!("{field_name} must be a relative path"),
            relative.as_str(),
            "absolute",
        ));
    }
    if relative.parts().contains(&"..") {
        return Err(refuse(
            format!("{field_name} must not escape the skills root"),
            relative.as_str(),
            "escape_root",
        ));
    }
    if relative.parts().is_empty() {
        return Err(refuse(
            format!("{field_name} must be non-empty"),
            relative.as_str(),
            "empty",
        ));
    }
    Ok(relative)
}

/// One skill whose files the capability writes into the workspace.
///
/// The reference's `Skill`. The content becomes `SKILL.md`; scripts, references and assets each
/// become a directory of the same name beside it, holding the entries given, at the relative paths
/// given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    name: String,
    description: String,
    content: Entry,
    compatibility: Option<String>,
    scripts: BTreeMap<String, Entry>,
    references: BTreeMap<String, Entry>,
    assets: BTreeMap<String, Entry>,
    deferred: bool,
}

impl Skill {
    /// A skill whose `SKILL.md` holds `content`.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::SkillsConfigInvalid`] for a name that is empty, absolute, or climbs out
    /// of the skills root.
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        content: impl Into<Vec<u8>>,
    ) -> SandboxResult<Self> {
        Self::with_content_entry(name, description, Entry::file(content))
    }

    /// A skill whose `SKILL.md` is the entry given — a `file`, or a `local_file` copied from the
    /// host.
    ///
    /// # Errors
    ///
    /// As [`Self::new`], and [`ErrorCode::SkillsConfigInvalid`] for content that is not file-like.
    pub fn with_content_entry(
        name: impl Into<String>,
        description: impl Into<String>,
        content: Entry,
    ) -> SandboxResult<Self> {
        let name = name.into();
        validate_relative_path(&name, "name", &[("skill_name", Value::from(name.as_str()))])?;
        if !matches!(
            content.content(),
            EntryContent::File { .. } | EntryContent::LocalFile { .. }
        ) {
            return Err(
                SandboxError::skills_config("skill content must be file-like")
                    .with_context("field", "content")
                    .with_context("skill_name", name.as_str())
                    .with_context("content_type", content.entry_type()),
            );
        }
        Ok(Self {
            name,
            description: description.into(),
            content,
            compatibility: None,
            scripts: BTreeMap::new(),
            references: BTreeMap::new(),
            assets: BTreeMap::new(),
            deferred: false,
        })
    }

    /// Adds a file or directory under `scripts/`, at `path`.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::SkillsConfigInvalid`] for a path that is empty, absolute or climbs out,
    /// and for one that names, once normalized, a path already given.
    pub fn with_script(self, path: &str, entry: Entry) -> SandboxResult<Self> {
        self.with_entry(ArtifactField::Scripts, path, entry)
    }

    /// Adds a file or directory under `references/`, at `path`.
    ///
    /// # Errors
    ///
    /// As [`Self::with_script`].
    pub fn with_reference(self, path: &str, entry: Entry) -> SandboxResult<Self> {
        self.with_entry(ArtifactField::References, path, entry)
    }

    /// Adds a file or directory under `assets/`, at `path`.
    ///
    /// # Errors
    ///
    /// As [`Self::with_script`].
    pub fn with_asset(self, path: &str, entry: Entry) -> SandboxResult<Self> {
        self.with_entry(ArtifactField::Assets, path, entry)
    }

    /// Records which environments the skill is meant for.
    ///
    /// Carried as the reference carries it; nothing reads it.
    #[must_use]
    pub fn with_compatibility(mut self, compatibility: impl Into<String>) -> Self {
        self.compatibility = Some(compatibility.into());
        self
    }

    /// Marks the skill as deferred.
    ///
    /// Carried as the reference carries it; nothing reads it.
    #[must_use]
    pub const fn deferred(mut self, deferred: bool) -> Self {
        self.deferred = deferred;
        self
    }

    fn with_entry(mut self, field: ArtifactField, path: &str, entry: Entry) -> SandboxResult<Self> {
        let field_name = field.as_str();
        let relative = validate_relative_path(
            path,
            field_name,
            &[
                ("skill_name", Value::from(self.name.as_str())),
                ("entry_path", Value::from(path)),
            ],
        )?;
        let entries = match field {
            ArtifactField::Scripts => &mut self.scripts,
            ArtifactField::References => &mut self.references,
            ArtifactField::Assets => &mut self.assets,
        };
        let key = relative.as_str().to_owned();
        if entries.contains_key(&key) {
            return Err(SandboxError::skills_config(format!(
                "duplicate entry path in skill {field_name}"
            ))
            .with_context("skill_name", self.name.as_str())
            .with_context("field", field_name)
            .with_context("entry_path", key));
        }
        entries.insert(key, entry);
        Ok(self)
    }

    /// The skill's name, which is also its directory under the skills path.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// What the skill is for, as the index describes it.
    #[must_use]
    pub fn description(&self) -> &str {
        &self.description
    }

    /// The entry that becomes `SKILL.md`.
    #[must_use]
    pub const fn content_artifact(&self) -> &Entry {
        &self.content
    }

    /// Which environments the skill is meant for, if recorded.
    #[must_use]
    pub fn compatibility(&self) -> Option<&str> {
        self.compatibility.as_deref()
    }

    /// Whether the skill is marked deferred.
    #[must_use]
    pub const fn is_deferred(&self) -> bool {
        self.deferred
    }

    /// The entries under `scripts/`, keyed by normalized path.
    #[must_use]
    pub const fn scripts(&self) -> &BTreeMap<String, Entry> {
        &self.scripts
    }

    /// The entries under `references/`, keyed by normalized path.
    #[must_use]
    pub const fn references(&self) -> &BTreeMap<String, Entry> {
        &self.references
    }

    /// The entries under `assets/`, keyed by normalized path.
    #[must_use]
    pub const fn assets(&self) -> &BTreeMap<String, Entry> {
        &self.assets
    }

    /// The directory the skill is written as: `SKILL.md`, and each non-empty artifact directory.
    #[must_use]
    pub fn as_dir_entry(&self) -> Entry {
        let mut directory = Entry::dir().with_child(SKILL_MARKDOWN, self.content.clone());
        for (name, children) in [
            ("scripts", &self.scripts),
            ("references", &self.references),
            ("assets", &self.assets),
        ] {
            if !children.is_empty() {
                directory =
                    directory.with_child(name, Entry::dir().with_children(children.clone()));
            }
        }
        directory
    }
}

#[derive(Clone, Copy)]
enum ArtifactField {
    Scripts,
    References,
    Assets,
}

impl ArtifactField {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Scripts => "scripts",
            Self::References => "references",
            Self::Assets => "assets",
        }
    }
}

/// Collects a skills capability's configuration, and checks it when it is built.
#[derive(Default)]
pub struct SkillsBuilder {
    skills: Vec<Skill>,
    from: Option<Entry>,
    lazy_from: Option<Arc<dyn LazySkillSource>>,
    skills_path: Option<String>,
}

impl fmt::Debug for SkillsBuilder {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SkillsBuilder")
            .field("skills", &self.skills)
            .field("from", &self.from)
            .field("lazy_from", &self.lazy_from.is_some())
            .field("skills_path", &self.skills_path)
            .finish()
    }
}

impl SkillsBuilder {
    /// Adds a literal skill.
    #[must_use]
    pub fn skill(mut self, skill: Skill) -> Self {
        self.skills.push(skill);
        self
    }

    /// Adds literal skills.
    #[must_use]
    pub fn skills(mut self, skills: impl IntoIterator<Item = Skill>) -> Self {
        self.skills.extend(skills);
        self
    }

    /// Places `entry`, a directory of skill directories, at the skills path.
    ///
    /// The reference's `from_`.
    #[must_use]
    pub fn from_entry(mut self, entry: Entry) -> Self {
        self.from = Some(entry);
        self
    }

    /// Indexes the skills `source` offers and stages each one when the model asks for it.
    #[must_use]
    pub fn lazy_from(mut self, source: impl LazySkillSource) -> Self {
        self.lazy_from = Some(Arc::new(source));
        self
    }

    /// As [`Self::lazy_from`], for a source that is already shared.
    #[must_use]
    pub fn lazy_from_shared(mut self, source: Arc<dyn LazySkillSource>) -> Self {
        self.lazy_from = Some(source);
        self
    }

    /// Places skills at `path` instead of [`DEFAULT_SKILLS_PATH`].
    #[must_use]
    pub fn skills_path(mut self, path: impl Into<String>) -> Self {
        self.skills_path = Some(path.into());
        self
    }

    /// Checks the configuration and builds the capability.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::SkillsConfigInvalid`], worded as the reference's, for a skills path
    /// that is empty, absolute or climbs out of the workspace; for none of the three sources, or
    /// more than one; for an entry source that is not a directory; and for two literal skills of
    /// the same name.
    pub fn build(self) -> SandboxResult<Skills> {
        let skills_path = validate_relative_path(
            self.skills_path.as_deref().unwrap_or(DEFAULT_SKILLS_PATH),
            "skills_path",
            &[],
        )?;

        if self.skills.is_empty() && self.from.is_none() && self.lazy_from.is_none() {
            return Err(SandboxError::skills_config(
                "skills capability requires `skills`, `from_`, or `lazy_from`",
            )
            .with_context("field", "skills"));
        }
        let configured_sources = usize::from(!self.skills.is_empty())
            + usize::from(self.from.is_some())
            + usize::from(self.lazy_from.is_some());
        if configured_sources > 1 {
            return Err(SandboxError::skills_config(
                "skills capability accepts only one of `skills`, `from_`, or `lazy_from`",
            )
            .with_context("field", "skills")
            .with_context("has_from", self.from.is_some()));
        }
        if let Some(from) = &self.from
            && !from.is_dir()
        {
            return Err(
                SandboxError::skills_config("`from_` must be a directory-like artifact")
                    .with_context("field", "from_")
                    .with_context("artifact_type", from.entry_type()),
            );
        }

        let mut seen_names = Vec::new();
        for skill in &self.skills {
            let relative = validate_relative_path(
                skill.name(),
                "skills[].name",
                &[("skill_name", Value::from(skill.name()))],
            )?;
            if seen_names.contains(&relative) {
                return Err(SandboxError::skills_config(format!(
                    "duplicate skill name: {}",
                    skill.name()
                ))
                .with_context("field", "skills[].name")
                .with_context("skill_name", skill.name()));
            }
            seen_names.push(relative);
        }

        Ok(Skills {
            skills: self.skills,
            from: self.from,
            lazy_from: self.lazy_from,
            skills_path: skills_path.as_str().to_owned(),
            session: None,
            run_as: None,
            workspace_scope: SandboxWorkspaceScope::root(),
            manifest: None,
            metadata: Mutex::new(None),
        })
    }
}

/// The metadata an index was last rendered from, and the grants it was read under.
#[derive(Debug, Clone)]
struct MetadataCache {
    key: Vec<(String, bool, Option<String>)>,
    metadata: Vec<SkillMetadata>,
}

/// The skills capability.
// The field names are the reference's: `skills` and `skills_path` are what its configuration,
// messages and tests call them.
#[allow(clippy::struct_field_names)]
pub struct Skills {
    skills: Vec<Skill>,
    from: Option<Entry>,
    lazy_from: Option<Arc<dyn LazySkillSource>>,
    skills_path: String,
    session: Option<Arc<dyn SandboxSession>>,
    run_as: Option<User>,
    workspace_scope: SandboxWorkspaceScope,
    manifest: Option<Manifest>,
    metadata: Mutex<Option<MetadataCache>>,
}

impl Clone for Skills {
    fn clone(&self) -> Self {
        Self {
            skills: self.skills.clone(),
            from: self.from.clone(),
            lazy_from: self.lazy_from.clone(),
            skills_path: self.skills_path.clone(),
            session: self.session.clone(),
            run_as: self.run_as.clone(),
            workspace_scope: self.workspace_scope.clone(),
            manifest: self.manifest.clone(),
            metadata: Mutex::new(self.cached_metadata()),
        }
    }
}

impl fmt::Debug for Skills {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Skills")
            .field("skills", &self.skills)
            .field("from", &self.from)
            .field("lazy_from", &self.lazy_from.is_some())
            .field("skills_path", &self.skills_path)
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

impl Skills {
    /// Starts a configuration.
    #[must_use]
    pub fn builder() -> SkillsBuilder {
        SkillsBuilder::default()
    }

    /// The literal skills.
    #[must_use]
    pub fn skills(&self) -> &[Skill] {
        &self.skills
    }

    /// The entry placed at the skills path, if that is the source.
    #[must_use]
    pub const fn from_entry(&self) -> Option<&Entry> {
        self.from.as_ref()
    }

    /// The lazy source, if that is the source.
    #[must_use]
    pub fn lazy_from(&self) -> Option<&Arc<dyn LazySkillSource>> {
        self.lazy_from.as_ref()
    }

    /// Where skills are placed, relative to the workspace root, normalized.
    #[must_use]
    pub fn skills_path(&self) -> &str {
        &self.skills_path
    }

    /// Whether a session is bound.
    #[must_use]
    pub const fn is_bound(&self) -> bool {
        self.session.is_some()
    }

    /// A copy bound to `session`, with nothing cached.
    ///
    /// The reference's `bind`, which clears the metadata cache: a new session may hold different
    /// skills.
    #[must_use]
    pub fn bound_to(&self, session: Arc<dyn SandboxSession>) -> Self {
        Self {
            session: Some(session),
            metadata: Mutex::new(None),
            ..self.clone()
        }
    }

    /// Reads and loads skills as `run_as`.
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

    fn cached_metadata(&self) -> Option<MetadataCache> {
        self.metadata
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The tools: `load_skill` for a lazy source, and none otherwise.
    ///
    /// # Errors
    ///
    /// Returns the reference's error when a lazy source is configured and no session is bound.
    pub fn try_tools(&self) -> Result<Vec<Arc<dyn Tool>>> {
        if self.lazy_from.is_none() {
            return Ok(Vec::new());
        }
        if self.session.is_none() {
            return Err(Error::config(UNBOUND));
        }
        Ok(vec![Arc::new(LoadSkillTool::new(Arc::new(self.clone()))?)])
    }

    /// Stages one lazily configured skill into the workspace.
    ///
    /// Answers with what the source reported. With a run working directory, the reported `path` is
    /// re-rendered as the absolute path the index shows.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::SkillsConfigInvalid`] when no lazy source is configured, and when a
    /// run working directory is set and the source reported no path or one that is not
    /// workspace-relative; [`ErrorCode::SandboxConfigInvalid`] when no session is bound; and
    /// whatever the source failed with.
    pub async fn load_skill(&self, skill_name: &str) -> SandboxResult<SkillLoadResult> {
        let Some(lazy_from) = &self.lazy_from else {
            return Err(SandboxError::skills_config(
                "load_skill is only available when lazy_from is configured",
            )
            .with_context("skill_name", skill_name));
        };
        let Some(session) = &self.session else {
            return Err(SandboxError::new(
                ErrorCode::SandboxConfigInvalid,
                OpName::Materialize,
                UNBOUND,
            ));
        };
        let result = lazy_from
            .load_skill(skill_name, session, &self.skills_path, self.run_as.as_ref())
            .await?;
        if self.workspace_scope.cwd().is_none() {
            return Ok(result);
        }

        let Some(source_path) = result.get("path") else {
            return Err(SandboxError::skills_config(SCOPED_SKILL_PATH_ERROR)
                .with_context("skill_name", skill_name)
                .with_context("field", "path")
                .with_context("reason", "missing"));
        };
        let model_path =
            self.model_skill_path(&session.state().manifest().root, skill_name, source_path)?;
        Ok(result.with_field("path", model_path))
    }

    /// The reference's `_model_skill_path`: the path as written without a run working directory,
    /// and absolute under the workspace root with one.
    fn model_skill_path(
        &self,
        workspace_root: &str,
        skill_name: &str,
        path: &str,
    ) -> SandboxResult<String> {
        if self.workspace_scope.cwd().is_none() {
            return Ok(path.replace('\\', "/"));
        }
        self.workspace_scope
            .model_resource_path(workspace_root, path)
            .map(String::from)
            .map_err(|error| {
                SandboxError::skills_config(SCOPED_SKILL_PATH_ERROR)
                    .with_context("skill_name", skill_name)
                    .with_context("field", "path")
                    .with_context("path", path)
                    .with_context("reason", "invalid")
                    .with_cause(error)
            })
    }

    /// Reads the index of skills placed from an entry, from the workspace as it now is.
    ///
    /// Every failure means "no skill here": a skills directory that is not there yet, a
    /// subdirectory without a readable `SKILL.md`.
    async fn resolve_runtime_metadata(&self, manifest: &Manifest) -> Vec<SkillMetadata> {
        let Some(session) = &self.session else {
            return Vec::new();
        };
        let skills_root = PosixPath::coerce(&manifest.root).join(&self.skills_path);
        let Ok(entries) = session
            .ls(SessionPath::Posix(&skills_root), self.run_as.clone())
            .await
        else {
            return Vec::new();
        };

        let skills_path = PosixPath::coerce(&self.skills_path);
        let mut metadata = Vec::new();
        for entry in entries {
            if !entry.is_dir() {
                continue;
            }
            let skill_dir = PosixPath::coerce(&entry.path);
            let Some(skill_name) = skill_dir.parts().last().map(|part| (*part).to_owned()) else {
                continue;
            };
            let skill_md = skill_dir.join(SKILL_MARKDOWN);
            let Ok(markdown) = session
                .read(SessionPath::Posix(&skill_md), self.run_as.clone())
                .await
            else {
                continue;
            };
            let mut frontmatter = parse_skill_frontmatter(&String::from_utf8_lossy(&markdown));
            metadata.push(SkillMetadata::new(
                frontmatter
                    .remove("name")
                    .unwrap_or_else(|| skill_name.clone()),
                frontmatter
                    .remove("description")
                    .unwrap_or_else(|| NO_SKILL_DESCRIPTION.to_owned()),
                skills_path.join(&skill_name).as_str(),
            ));
        }
        metadata
    }

    /// The reference's `_skill_metadata`: literal skills, then the lazy source's index or the
    /// workspace's, deduplicated by name and path and sorted by name.
    ///
    /// Cached until the capability is bound again; for a lazy source, also until the manifest's
    /// grants change, because they decide what the source may read.
    async fn skill_metadata(&self, manifest: &Manifest) -> SandboxResult<Vec<SkillMetadata>> {
        let key = self.metadata_cache_key(manifest);
        if let Some(cached) = self.cached_metadata()
            && cached.key == key
        {
            return Ok(cached.metadata);
        }

        let skills_path = PosixPath::coerce(&self.skills_path);
        let mut metadata: Vec<SkillMetadata> = self
            .skills
            .iter()
            .map(|skill| {
                SkillMetadata::new(
                    skill.name(),
                    skill.description(),
                    skills_path.join(skill.name()).as_str(),
                )
            })
            .collect();

        if let Some(lazy_from) = &self.lazy_from {
            metadata.extend(
                lazy_from.list_skill_metadata(&self.skills_path, &manifest.extra_path_grants)?,
            );
        } else if self.from.is_some() {
            metadata.extend(self.resolve_runtime_metadata(manifest).await);
        }

        if metadata.is_empty()
            && let Some(children) = self.from.as_ref().and_then(Entry::children)
        {
            for (key, entry) in children {
                if !matches!(entry.content(), EntryContent::Dir { .. }) {
                    continue;
                }
                let skill_name = PosixPath::coerce(key).as_str().to_owned();
                metadata.push(SkillMetadata::new(
                    skill_name.clone(),
                    entry.description().unwrap_or(NO_SKILL_DESCRIPTION),
                    skills_path.join(&skill_name).as_str(),
                ));
            }
        }

        // First position, last value: what a dictionary keyed by (name, path) keeps.
        let mut deduped: Vec<SkillMetadata> = Vec::new();
        for item in metadata {
            match deduped
                .iter_mut()
                .find(|kept| kept.name() == item.name() && kept.path() == item.path())
            {
                Some(kept) => *kept = item,
                None => deduped.push(item),
            }
        }
        deduped.sort_by(|left, right| left.name().cmp(right.name()));

        *self.metadata.lock().unwrap_or_else(PoisonError::into_inner) = Some(MetadataCache {
            key,
            metadata: deduped.clone(),
        });
        Ok(deduped)
    }

    fn metadata_cache_key(&self, manifest: &Manifest) -> Vec<(String, bool, Option<String>)> {
        if self.lazy_from.is_none() {
            return Vec::new();
        }
        manifest
            .extra_path_grants
            .iter()
            .map(|grant| {
                (
                    grant.path().to_owned(),
                    grant.is_read_only(),
                    grant.host_path().map(str::to_owned),
                )
            })
            .collect()
    }

    /// The index and its guidance for a session whose manifest is `manifest`, or `None` when there
    /// are no skills to list.
    ///
    /// The reference's `instructions(manifest)`.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::SkillsConfigInvalid`] when a run working directory is set and a skill's
    /// path is not workspace-relative, and whatever the lazy source failed to list with.
    pub async fn instructions_for(&self, manifest: &Manifest) -> SandboxResult<Option<String>> {
        let skills = self.skill_metadata(manifest).await?;
        if skills.is_empty() {
            return Ok(None);
        }

        let mut lines = vec![
            "## Skills".to_owned(),
            SKILLS_SECTION_INTRO.to_owned(),
            "### Available skills".to_owned(),
        ];
        for skill in &skills {
            let path =
                self.model_skill_path(&manifest.root, skill.name(), skill.path().as_str())?;
            lines.push(format!(
                "- {}: {} (file: {path})",
                skill.name(),
                skill.description()
            ));
        }
        if self.workspace_scope.cwd().is_some() {
            lines.push("### Run-scoped skill paths".to_owned());
            lines.extend(SKILL_PATH_GUIDANCE.iter().map(|line| (*line).to_owned()));
        }
        if self.lazy_from.is_some() {
            lines.extend(LAZY_LOADING_SECTION.iter().map(|line| (*line).to_owned()));
        }
        lines.push(how_to_use_section(self.lazy_from.is_some()));
        Ok(Some(lines.join("\n")))
    }
}

/// Carries a skills failure out of the instruction channel, keeping it as the source.
fn instruction_failure(error: SandboxError) -> Error {
    Error::sandbox(SandboxErrorKind::Setup, error.to_string()).with_source(error)
}

#[async_trait]
impl Capability for Skills {
    fn kind(&self) -> CapabilityFamily {
        CapabilityFamily::SKILLS
    }

    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        self.try_tools().unwrap_or_default()
    }

    async fn instructions(&self) -> Result<Option<PromptSection>> {
        let Some(manifest) = &self.manifest else {
            return Ok(None);
        };
        let Some(text) = self
            .instructions_for(manifest)
            .await
            .map_err(instruction_failure)?
        else {
            return Ok(None);
        };
        let family = self.kind();
        PromptSection::new(
            family.prompt_section_name(),
            "sandbox skills index",
            family.prompt_source(),
            SectionStability::Stable,
            SectionPosition::Prefix,
            text,
        )
        .map(Some)
    }

    fn process_manifest(&self, manifest: &mut Manifest) -> SandboxResult<()> {
        let skills_root = PosixPath::coerce(&self.skills_path);
        let existing = |manifest: &Manifest, path: &PosixPath| -> Option<Entry> {
            manifest
                .entries
                .iter()
                .find(|(key, _)| PosixPath::coerce(key) == *path)
                .map(|(_, entry)| entry.clone())
        };

        if self.lazy_from.is_some() {
            // A lazy source claims nothing in the manifest up front, so the whole namespace is
            // reserved here: any entry at, above or below the skills path would collide with a
            // skill staged later.
            let mut overlaps: Vec<String> = manifest
                .entries
                .keys()
                .map(|key| PosixPath::coerce(key))
                .filter(|path| skills_root.is_under(path) || path.is_under(&skills_root))
                .map(String::from)
                .collect();
            overlaps.sort();
            if !overlaps.is_empty() {
                return Err(SandboxError::skills_config(
                    "skills lazy_from path overlaps existing manifest entries",
                )
                .with_context("path", skills_root.as_str())
                .with_context("source", "lazy_from")
                .with_context("overlaps", overlaps));
            }
            return Ok(());
        }

        if let Some(from) = &self.from {
            if let Some(existing_entry) = existing(manifest, &skills_root) {
                if existing_entry.is_dir() {
                    return Ok(());
                }
                return Err(SandboxError::skills_config(
                    "skills root path already exists in manifest",
                )
                .with_context("path", skills_root.as_str())
                .with_context("source", "from_")
                .with_context("existing_type", existing_entry.entry_type()));
            }
            manifest
                .entries
                .insert(skills_root.as_str().to_owned(), from.clone());
        }

        for skill in &self.skills {
            let relative_path = skills_root.join(PosixPath::coerce(skill.name()).as_str());
            let rendered = skill.as_dir_entry();
            if let Some(existing_entry) = existing(manifest, &relative_path) {
                if existing_entry == rendered {
                    continue;
                }
                return Err(
                    SandboxError::skills_config("skill path already exists in manifest")
                        .with_context("path", relative_path.as_str())
                        .with_context("skill_name", skill.name()),
                );
            }
            manifest
                .entries
                .insert(relative_path.as_str().to_owned(), rendered);
        }
        Ok(())
    }

    fn bind(&self, context: &RunContext) -> Result<Option<Arc<dyn Capability>>> {
        let _ = context;
        if self.session.is_none() {
            return Err(Error::config(UNBOUND));
        }
        Ok(None)
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

/// The arguments `load_skill` takes.
///
/// Unknown fields are ignored, as the reference's model ignores them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, ToolInput)]
#[tool_input(
    strict = false,
    description = "Load a single lazily configured skill into the sandbox so its SKILL.md, scripts, \
                   references, and assets can be read from the workspace."
)]
pub(crate) struct LoadSkillArgs {
    // Undocumented on purpose: the reference's schema gives the field no description.
    skill_name: String,
}

/// Runs `load_skill` for a bound skills capability.
///
/// Private, as the reference's `_LoadSkillTool` is: a host reaches it through [`Skills`], and the
/// tool's name and description are public as [`LOAD_SKILL_TOOL_NAME`] and
/// [`LOAD_SKILL_DESCRIPTION`].
#[derive(Clone)]
pub(crate) struct LoadSkillTool {
    origin: ToolOrigin,
    func_schema: FuncSchema,
    skills: Arc<Skills>,
    needs_approval: NeedsApproval,
}

impl fmt::Debug for LoadSkillTool {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LoadSkillTool")
            .field("skills", &self.skills)
            .field("needs_approval", &self.needs_approval)
            .finish_non_exhaustive()
    }
}

impl LoadSkillTool {
    /// A tool loading skills through `skills`, without approval.
    ///
    /// # Errors
    ///
    /// Returns a configuration error if the schema cannot be built, which is a defect here rather
    /// than a condition a caller can cause.
    pub(crate) fn new(skills: Arc<Skills>) -> Result<Self> {
        Ok(Self {
            origin: ToolOrigin::new(LOAD_SKILL_TOOL_NAME)?,
            func_schema: FuncSchema::for_input::<LoadSkillArgs>(LOAD_SKILL_TOOL_NAME)?,
            skills,
            needs_approval: NeedsApproval::Never,
        })
    }

    /// Runs one call, answering with what the source reported, as a JSON object.
    ///
    /// # Errors
    ///
    /// Returns a failure carrying what [`Skills::load_skill`] failed with as its source.
    async fn run(&self, args: &LoadSkillArgs) -> Result<ToolOutput> {
        let result = self
            .skills
            .load_skill(&args.skill_name)
            .await
            .map_err(|error| session_failure(LOAD_SKILL_TOOL_NAME, error))?;
        let text = serde_json::to_string(&result).map_err(|error| {
            Error::tool(
                ToolErrorKind::ExecutionFailed,
                LOAD_SKILL_TOOL_NAME,
                error.to_string(),
            )
        })?;
        Ok(ToolOutput::text(text))
    }
}

#[async_trait]
impl Tool for LoadSkillTool {
    fn origin(&self) -> &ToolOrigin {
        &self.origin
    }

    fn schema(&self) -> &ToolSchema {
        self.func_schema.tool_schema()
    }

    fn func_schema(&self) -> Option<&FuncSchema> {
        Some(&self.func_schema)
    }

    fn options(&self) -> ToolOptions {
        sandbox_tool_options(&self.needs_approval)
    }

    async fn call(&self, mut context: ToolContext<'_>) -> Result<ToolOutput> {
        let args: LoadSkillArgs = decode(&mut context, &self.func_schema)?;
        self.run(&args).await
    }
}
