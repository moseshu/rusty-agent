//! The product's `prompt dump` report, and the comparison that names what invalidated a prefix.
//!
//! **This is the composition layer for the report, and it is here rather than in `ra-cli` on
//! purpose.** Which prefix a dump covers, which tools a host-backed agent installs, and what a
//! report says when no run produced it are product decisions; `ra-prompt` owns the rendering and
//! knows none of them, and the layering gate keeps the binary from reaching past this crate to
//! assemble them itself. Putting the composition in the binary would also have meant two of them —
//! the snapshot gate composes the same report — and the two would have drifted at the first edit.
//!
//! The report the CLI prints is therefore the report the gate commits: the same entry point renders
//! both, and [`PLACEHOLDER_CACHE_SCOPE`] is what makes the bytes comparable, since no cache scope a
//! real run would produce is reproducible from a command line.
//!
//! # Why the comparison is here too
//!
//! A prefix hash that changed says only that every cached prefix was invalidated, which is the
//! least useful half of the fact. [`compare_prompt_dump`] names the sections responsible, including
//! the case no section hash can show on its own: two sections swapping places rewrites the joined
//! prefix while every section in it is byte-identical.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use ra_core::error::{Error, Result};
use ra_core::prompt::{CachePlan, PromptRole};
use ra_prompt::dump::{PromptDump, PromptDumpSection};

use super::{assemble_stable_prefix, assemble_stable_prefix_for_surface};
use crate::agent::{HOST_BACKED_PROFILE, host_backed_surface};
use crate::host::CodingHost;

/// Cache scope recorded in a report that no run produced.
///
/// A dump is rendered outside any run, so there is no run id to name. Writing a placeholder rather
/// than omitting the scope keeps the report's shape identical to the one a run would produce, and
/// keeps the committed snapshot byte-comparable with what the command prints.
pub const PLACEHOLDER_CACHE_SCOPE: &str = "<run>";

/// The role a dump covers when the caller does not choose one.
pub const DEFAULT_ROLE: &str = "main";

/// The roles the product ships, in the order a report offers them.
static SHIPPED_ROLES: [PromptRole; 5] = [
    PromptRole::Main,
    PromptRole::ReadOnlySpecialist,
    PromptRole::Planner,
    PromptRole::OneOffAnswer,
    PromptRole::Coordinator,
];

/// The role names a dump accepts.
#[must_use]
pub fn shipped_role_names() -> Vec<&'static str> {
    SHIPPED_ROLES.iter().map(PromptRole::role_name).collect()
}

/// Resolves a role name against the roles the product ships.
///
/// An unknown name is refused rather than turned into [`PromptRole::Custom`]. A custom role has no
/// guidance text of its own, so accepting one would answer a typo with a plausible-looking report
/// whose role section is a generated placeholder — the report is a verification entry point, and
/// that is the one thing it must not do.
fn resolve_role(name: &str) -> Result<PromptRole> {
    SHIPPED_ROLES
        .iter()
        .find(|role| role.role_name() == name)
        .cloned()
        .ok_or_else(|| {
            Error::config(format!(
                "unknown prompt role `{name}`; expected one of {}",
                shipped_role_names().join(", ")
            ))
        })
}

/// What a prompt dump should cover.
///
/// The role, provider and model are strings rather than typed values because the caller is a
/// command line, and the kernel types they resolve to are not on the binary's dependency edge.
/// Resolution and its error message therefore belong here, where the set of valid names is known.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PromptDumpRequest {
    role: Option<String>,
    provider: Option<String>,
    model: Option<String>,
    cache_scope: Option<String>,
    workspace: Option<PathBuf>,
}

impl PromptDumpRequest {
    /// Creates a request for the default role's tool-free prefix.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Covers the named role instead of [`DEFAULT_ROLE`].
    #[must_use]
    pub fn with_role(mut self, role: impl Into<String>) -> Self {
        self.role = Some(role.into());
        self
    }

    /// Covers the prefix a host-backed agent opened on this workspace would carry.
    ///
    /// Without it the report covers the tool-free prefix `build_agent` produces, which is the
    /// shorter of the two and names no tool.
    #[must_use]
    pub fn with_workspace(mut self, workspace: impl Into<PathBuf>) -> Self {
        self.workspace = Some(workspace.into());
        self
    }

    /// Labels the report with a target provider.
    ///
    /// A label, not an input: no section is assembled per provider today, so recording one states
    /// which endpoint the reader had in mind and changes nothing about the bytes above it.
    #[must_use]
    pub fn with_provider(mut self, provider: impl Into<String>) -> Self {
        self.provider = Some(provider.into());
        self
    }

    /// Labels the report with a target model, on the same terms as [`Self::with_provider`].
    #[must_use]
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    /// Names the cache scope instead of [`PLACEHOLDER_CACHE_SCOPE`].
    #[must_use]
    pub fn with_cache_scope(mut self, scope: impl Into<String>) -> Self {
        self.cache_scope = Some(scope.into());
        self
    }

    /// Assembles the prefix this request describes and summarizes it.
    ///
    /// Asynchronous because a capability resolves its prompt fragment asynchronously, and a report
    /// that skipped the fragments to stay synchronous would be a report about a prefix the product
    /// does not send.
    ///
    /// # Errors
    ///
    /// Returns an error for an unknown role name, a workspace that cannot be opened, and any
    /// prompt assembly failure underneath.
    async fn build(&self) -> Result<PromptDump> {
        let role = resolve_role(self.role.as_deref().unwrap_or(DEFAULT_ROLE))?;
        let prefix = match &self.workspace {
            Some(workspace) => {
                // The same tier the host-backed builder assembles, read from it rather than chosen
                // here: a report about a different profile than the product ships would look
                // exactly as trustworthy as this one.
                let host = open_host(workspace)?;
                let assembled = host_backed_surface(&role, &host, HOST_BACKED_PROFILE).await?;
                assemble_stable_prefix_for_surface(
                    &role,
                    assembled.tool_surface(),
                    assembled.capability_sections(),
                )?
            }
            None => assemble_stable_prefix(&role)?,
        };
        let scope = self
            .cache_scope
            .as_deref()
            .unwrap_or(PLACEHOLDER_CACHE_SCOPE);
        let plan = CachePlan::for_prefix(prefix.system_instructions(), Some(scope));
        Ok(PromptDump::from_assembled(
            &prefix,
            Some(plan),
            self.provider.as_deref(),
            self.model.as_deref(),
        ))
    }
}

fn open_host(workspace: &Path) -> Result<CodingHost> {
    CodingHost::open(workspace).map_err(|error| {
        Error::config(format!(
            "cannot open workspace `{}`: {error}",
            workspace.display()
        ))
    })
}

/// Renders the human-readable prompt dump report.
///
/// # Errors
///
/// Propagates the failures described on [`PromptDumpRequest::build`].
pub async fn render_prompt_dump(request: &PromptDumpRequest) -> Result<String> {
    Ok(request.build().await?.render_text())
}

/// Renders the prompt dump as JSON, the form [`compare_prompt_dump`] reads back.
///
/// # Errors
///
/// Propagates the failures described on [`PromptDumpRequest::build`], and serialization failures.
pub async fn render_prompt_dump_json(request: &PromptDumpRequest) -> Result<String> {
    request.build().await?.to_json()
}

/// Compares a recorded dump against the one this build produces.
///
/// `baseline_json` is the output of [`render_prompt_dump_json`] from an earlier build.
///
/// # Errors
///
/// Returns an error if the baseline is not a prompt dump in JSON form, plus the failures described
/// on [`PromptDumpRequest::build`].
pub async fn compare_prompt_dump(
    request: &PromptDumpRequest,
    baseline_json: &str,
) -> Result<PromptDumpDiff> {
    let baseline: PromptDump = serde_json::from_str(baseline_json).map_err(|error| {
        Error::config(format!(
            "baseline is not a prompt dump in JSON form: {error}. Record one with \
             `prompt dump --json`"
        ))
    })?;
    Ok(PromptDumpDiff::between(&baseline, &request.build().await?))
}

/// One section's fate between two dumps.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SectionChange {
    /// The section is new.
    Added {
        /// Section name.
        name: String,
        /// Tokens it adds.
        tokens: usize,
    },
    /// The section is gone.
    Removed {
        /// Section name.
        name: String,
        /// Tokens it used to cost.
        tokens: usize,
    },
    /// The section's content changed.
    Rewritten {
        /// Section name.
        name: String,
        /// Content hash before.
        before: String,
        /// Content hash after.
        after: String,
        /// Change in estimated tokens.
        token_delta: isize,
    },
    /// The section is byte-identical but the prefix presents it in a different order.
    ///
    /// Worth its own variant because no content hash shows it, and the joined prefix — the span a
    /// provider caches — is rewritten all the same.
    ///
    /// Reported on the section's rank *among the sections both dumps share*, not on its absolute
    /// index. Inserting one section shifts the index of everything below it, and a report that
    /// called all of them moved would bury the insertion that actually caused the change under the
    /// consequences of it. The indices carried here are still the absolute ones, because those are
    /// the rows a reader is looking at.
    Moved {
        /// Section name.
        name: String,
        /// Index before.
        before: usize,
        /// Index after.
        after: usize,
    },
}

impl SectionChange {
    /// The section this change is about.
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Self::Added { name, .. }
            | Self::Removed { name, .. }
            | Self::Rewritten { name, .. }
            | Self::Moved { name, .. } => name,
        }
    }
}

/// What changed between two prompt dumps, and which sections caused it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PromptDumpDiff {
    before_prefix_hash: String,
    after_prefix_hash: String,
    before_tokens: usize,
    after_tokens: usize,
    changes: Vec<SectionChange>,
}

impl PromptDumpDiff {
    fn between(before: &PromptDump, after: &PromptDump) -> Self {
        Self {
            before_prefix_hash: before.prefix_hash().to_owned(),
            after_prefix_hash: after.prefix_hash().to_owned(),
            before_tokens: before.total_prefix_tokens(),
            after_tokens: after.total_prefix_tokens(),
            changes: section_changes(before.sections(), after.sections()),
        }
    }

    /// Whether the stable prefix hash changed, invalidating every cached prefix.
    #[must_use]
    pub fn prefix_changed(&self) -> bool {
        self.before_prefix_hash != self.after_prefix_hash
    }

    /// The section-level changes, in the order the current prefix presents them.
    #[must_use]
    pub fn changes(&self) -> &[SectionChange] {
        &self.changes
    }

    /// Formats the comparison as a human-readable report.
    #[must_use]
    #[allow(clippy::cast_possible_wrap)]
    pub fn render_text(&self) -> String {
        // Writing into a `String` cannot fail, so the `fmt::Result` values below are discarded.
        let mut out = String::new();
        out.push_str("==================== PROMPT DUMP COMPARISON ====================\n");
        if self.prefix_changed() {
            let _ = writeln!(out, "Stable Prefix Hash:   {}", self.before_prefix_hash);
            let _ = writeln!(out, "                   -> {}", self.after_prefix_hash);
        } else {
            let _ = writeln!(
                out,
                "Stable Prefix Hash:   {} (unchanged)",
                self.before_prefix_hash
            );
        }
        let delta = self.after_tokens as isize - self.before_tokens as isize;
        let _ = writeln!(
            out,
            "Total Prefix Tokens:  ~{} -> ~{} ({delta:+})",
            self.before_tokens, self.after_tokens
        );
        out.push_str("----------------------------------------------------------------\n");
        if self.changes.is_empty() {
            out.push_str("No section changed.\n");
        } else {
            for change in &self.changes {
                let _ = writeln!(out, "{}", render_change(change));
            }
        }
        out.push_str("----------------------------------------------------------------\n");
        // The invalidation trigger, stated as a line rather than left to be inferred from the rows.
        // A prefix hash can move while every section hash holds — a reorder does exactly that — so
        // "which rows are above" is not the same question as "what invalidated the prefix".
        if self.prefix_changed() {
            let names = self
                .changes
                .iter()
                .map(SectionChange::name)
                .collect::<Vec<_>>()
                .join(", ");
            let names = if names.is_empty() {
                "no section row; the prefix text changed outside the sections".to_owned()
            } else {
                names
            };
            let _ = writeln!(out, "Invalidated by:       {names}");
        } else {
            out.push_str("Invalidated by:       nothing; cached prefixes survive\n");
        }
        out.push_str("================================================================\n");
        out
    }
}

fn render_change(change: &SectionChange) -> String {
    match change {
        SectionChange::Added { name, tokens } => {
            format!("+ {name:<22} added                                        (~{tokens} tokens)")
        }
        SectionChange::Removed { name, tokens } => {
            format!("- {name:<22} removed                                      (~{tokens} tokens)")
        }
        SectionChange::Rewritten {
            name,
            before,
            after,
            token_delta,
        } => format!(
            "~ {name:<22} rewritten  {} -> {}  ({token_delta:+} tokens)",
            short_hash(before),
            short_hash(after)
        ),
        SectionChange::Moved {
            name,
            before,
            after,
        } => format!("> {name:<22} moved      position {before} -> {after}"),
    }
}

fn short_hash(hash: &str) -> &str {
    hash.get(..12).unwrap_or(hash)
}

/// Pairs the two section lists by name and classifies each pair.
///
/// Ordered by the current prefix so the report reads top to bottom like the prefix does; removed
/// sections have no current position and follow at the end.
#[allow(clippy::cast_possible_wrap)]
fn section_changes(
    before: &[PromptDumpSection],
    after: &[PromptDumpSection],
) -> Vec<SectionChange> {
    let locate = |sections: &[PromptDumpSection], name: &str| {
        sections.iter().position(|section| section.name() == name)
    };
    // Rank among the shared sections, which is what separates a genuine reorder from the index
    // shift that follows any insertion. See [`SectionChange::Moved`].
    let shared_rank = |sections: &[PromptDumpSection], other: &[PromptDumpSection], name: &str| {
        sections
            .iter()
            .filter(|section| locate(other, section.name()).is_some())
            .position(|section| section.name() == name)
    };

    let mut changes = Vec::new();
    for (after_index, section) in after.iter().enumerate() {
        let name = section.name();
        let Some(before_index) = locate(before, name) else {
            changes.push(SectionChange::Added {
                name: name.to_owned(),
                tokens: section.token_estimate(),
            });
            continue;
        };
        let previous = &before[before_index];
        if previous.content_hash() != section.content_hash() {
            changes.push(SectionChange::Rewritten {
                name: name.to_owned(),
                before: previous.content_hash().to_owned(),
                after: section.content_hash().to_owned(),
                token_delta: section.token_estimate() as isize - previous.token_estimate() as isize,
            });
        } else if shared_rank(before, after, name) != shared_rank(after, before, name) {
            changes.push(SectionChange::Moved {
                name: name.to_owned(),
                before: before_index,
                after: after_index,
            });
        }
    }
    for section in before {
        if locate(after, section.name()).is_none() {
            changes.push(SectionChange::Removed {
                name: section.name().to_owned(),
                tokens: section.token_estimate(),
            });
        }
    }
    changes
}
