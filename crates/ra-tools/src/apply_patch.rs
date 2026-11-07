//! The V4A patch-application tool.
//!
//! Editing a file through a diff is not a coding concern: a research agent revising a report and an
//! assistant maintaining a checklist want the same operation, and neither would write a line of
//! this differently. What *is* product content is the discipline around it — whether the patch tool
//! is the only writer, what the prompt says about reading before editing, and which approval a
//! product attaches — and none of that lives here.
//!
//! The tool binds a confined filesystem capability to [`ra_patch`]'s parser and its port of
//! Codex's update application ([`apply_chunks()`]); it owns neither. Operations are committed one
//! at a time in Codex's order and a later failure keeps the earlier ones, so the result reports
//! what was actually written rather than claiming an atomicity this does not have. As in Codex, an
//! added file overwrites one already at its path, a moved file overwrites its destination, and a
//! move writes the destination before it removes the source.
//!
//! Every write replaces the target's directory entry with a fresh inode instead of rewriting a
//! shared inode, and replaces a symbolic link rather than writing through it. Codex's in-place
//! writer runs in a kernel sandbox; this in-process writer cannot rely on that to protect files
//! outside the granted paths that share a hard link or are named by a link. An overwritten file
//! keeps its permissions, while ownership, extended attributes and hard-link relationships are not
//! copied. Write paths cannot traverse symbolic-link directories.

use std::{fmt, io::Read as _, path::Path, sync::Arc};

use async_trait::async_trait;
use ra_core::{
    error::Result,
    event::{
        FileEvent,
        file::{FileChangeKind, FileChangedEvent},
    },
    tool::{
        ResourceClaim, Tool, ToolApprovalPolicy, ToolConcurrency, ToolContext, ToolInput as _,
        ToolOptions, ToolOrigin, ToolOutput, ToolSchema,
    },
};
use ra_exec::fs::{RootedFileSystem, Workspace};
use ra_macros::ToolInput;
use ra_patch::{
    CommittedPatchDelta, PatchAction, PatchConflict, PatchHunk, apply_chunks, parse_patch,
};
use schemars::JsonSchema;
use serde::Deserialize;

const TOOL_NAME: &str = "apply_patch";

#[derive(Debug, Deserialize, JsonSchema, ToolInput)]
#[serde(deny_unknown_fields)]
/// Applies a V4A patch to workspace files. A later failure can leave earlier actions applied.
/// Supply the patch verbatim, from `*** Begin Patch` through `*** End Patch`.
struct ApplyPatchInput {
    /// Freeform V4A patch text. Do not encode file contents separately.
    patch: String,
}

/// An `apply_patch` tool confined to one workspace capability.
#[derive(Clone)]
pub struct ApplyPatchTool {
    origin: ToolOrigin,
    schema: ToolSchema,
    options: ToolOptions,
    filesystem: Arc<RootedFileSystem>,
    approval_resolver: Option<Arc<crate::exec_command::ApprovalResolver>>,
}

impl fmt::Debug for ApplyPatchTool {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ApplyPatchTool")
            .field("origin", &self.origin)
            .finish_non_exhaustive()
    }
}

impl ApplyPatchTool {
    /// Creates a patch tool backed by an already-confined workspace filesystem.
    ///
    /// The capability is the caller's to scope: this tool writes wherever the filesystem it is
    /// handed lets it write, and does not resolve or check a root of its own.
    pub fn new(filesystem: Arc<RootedFileSystem>) -> Result<Self> {
        // An unidentified capability cannot name a lock, so this form serializes against
        // everything rather than claiming a resource no other tool can agree on.
        Self::with_options(
            filesystem,
            base_options().with_concurrency(ToolConcurrency::Exclusive),
        )
    }

    /// Creates a patch tool for an already-opened workspace capability.
    ///
    /// Unlike [`Self::new`], this form runs beside calls that do not touch the same workspace and
    /// excludes only those that do — the workspace's own identity is what makes the narrower
    /// declaration safe.
    pub fn for_workspace(workspace: &Workspace) -> Result<Self> {
        Self::with_options(
            Arc::clone(workspace.filesystem()),
            base_options()
                .with_concurrency(ToolConcurrency::Parallel)
                .with_resource_claim(ResourceClaim::exclusive(workspace.resource_id().clone())),
        )
    }

    /// Installs a host policy callback. It also validates the policy immediately before writing.
    #[must_use]
    pub fn with_approval_resolver(
        mut self,
        resolver: Arc<crate::exec_command::ApprovalResolver>,
    ) -> Self {
        self.approval_resolver = Some(resolver);
        self.options = self.options.with_approval(ToolApprovalPolicy::Dynamic);
        self
    }

    fn with_options(filesystem: Arc<RootedFileSystem>, options: ToolOptions) -> Result<Self> {
        Ok(Self {
            origin: ToolOrigin::new(TOOL_NAME)?,
            schema: ApplyPatchInput::tool_schema(TOOL_NAME)?,
            options,
            filesystem,
            approval_resolver: None,
        })
    }
}

/// The policies both constructors share; only the concurrency declaration differs.
fn base_options() -> ToolOptions {
    ToolOptions::new()
        .with_approval(ToolApprovalPolicy::Always)
        .with_action_classes([crate::action_classes::EDIT])
}

#[async_trait]
impl Tool for ApplyPatchTool {
    fn origin(&self) -> &ToolOrigin {
        &self.origin
    }
    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    async fn needs_approval(&self, context: &ToolContext<'_>) -> Result<bool> {
        self.approval_resolver
            .as_ref()
            .map_or(Ok(true), |resolve| resolve(context))
    }

    async fn call(&self, context: ToolContext<'_>) -> Result<ToolOutput> {
        if let Some(resolve) = &self.approval_resolver {
            resolve(&context)?;
        }
        let patch = match context.arguments() {
            // The current provider-neutral call item carries JSON. Accepting a bare JSON string
            // keeps this handler compatible with the custom/freeform wire item once that item is
            // introduced, while the object form remains available to existing function-only
            // adapters.
            serde_json::Value::String(patch) => patch.clone(),
            arguments => {
                serde_json::from_value::<ApplyPatchInput>(arguments.clone())
                    .map_err(|error| {
                        ra_core::error::Error::caller(format!(
                            "invalid apply_patch arguments: {error}"
                        ))
                    })?
                    .patch
            }
        };
        let plan = match parse_patch(&patch) {
            Ok(plan) => plan,
            Err(error) => return Ok(ToolOutput::text(format!("Patch was not applied: {error}."))),
        };
        let operations = match operations(plan.actions()) {
            Ok(operations) => operations,
            Err(error) => {
                return Ok(ToolOutput::text(render_stop(
                    &CommittedPatchDelta::empty(),
                    &error,
                )));
            }
        };
        let mut committed = CommittedPatchDelta::empty();
        for operation in &operations {
            match self.apply_operation(&context, operation, &committed) {
                Ok(delta) => committed = delta,
                Err(stop) => {
                    return Ok(ToolOutput::text(render_stop(
                        &stop.committed,
                        &stop.failure,
                    )));
                }
            }
        }
        Ok(ToolOutput::text(render_success(&committed)))
    }

    fn options(&self) -> ToolOptions {
        self.options.clone()
    }
}

impl ApplyPatchTool {
    /// Commits one operation in Codex's order, recording each change as it lands.
    ///
    /// An update that moves its file writes the destination first and removes the source only
    /// once the destination is written, as Codex's `apply_hunks_to_files` does: a destination that
    /// cannot be written leaves the source untouched, and a source that cannot be removed leaves
    /// the written destination committed on its own.
    fn apply_operation(
        &self,
        context: &ToolContext<'_>,
        operation: &Operation<'_>,
        committed: &CommittedPatchDelta,
    ) -> std::result::Result<CommittedPatchDelta, Stop> {
        let stop = |failure| Stop {
            committed: committed.clone(),
            failure,
        };
        match *operation {
            Operation::Add { path, content } => {
                let replaced = self
                    .filesystem
                    .write_file_nofollow(path, content.as_bytes())
                    .map_err(|error| stop(PatchToolFailure::filesystem(&error)))?;
                let added = content.lines().count();
                let kind = if replaced {
                    FileChangeKind::Updated
                } else {
                    FileChangeKind::Added
                };
                record_change(context, path, kind, None, added, 0);
                Ok(append_delta(committed, path, added, 0))
            }
            Operation::Delete { path } => {
                if !self.filesystem.exists(path) {
                    return Err(stop(PatchToolFailure::Conflict(
                        PatchConflict::FileNotFound {
                            path: path.to_path_buf(),
                        },
                    )));
                }
                self.filesystem
                    .remove_file_nofollow(path)
                    .map_err(|error| stop(PatchToolFailure::filesystem(&error)))?;
                record_change(context, path, FileChangeKind::Deleted, None, 0, 0);
                Ok(append_delta(committed, path, 0, 0))
            }
            Operation::Update {
                path,
                hunks,
                move_to,
            } => {
                let original = self.read_text(path).map_err(stop)?;
                let update = apply_chunks(path, &original, hunks)
                    .map_err(|conflict| stop(PatchToolFailure::Conflict(conflict)))?;
                let (added, removed) = (update.lines_added(), update.lines_removed());
                let Some(destination) = move_to else {
                    self.filesystem
                        .replace_file(path, update.content().as_bytes())
                        .map_err(|error| stop(PatchToolFailure::filesystem(&error)))?;
                    record_change(context, path, FileChangeKind::Updated, None, added, removed);
                    return Ok(append_delta(committed, path, added, removed));
                };
                let replaced = self
                    .filesystem
                    .write_file_nofollow(destination, update.content().as_bytes())
                    .map_err(|error| stop(PatchToolFailure::filesystem(&error)))?;
                if let Err(error) = self.filesystem.remove_file_nofollow(path) {
                    let kind = if replaced {
                        FileChangeKind::Updated
                    } else {
                        FileChangeKind::Added
                    };
                    // Only the destination write committed. Account for its complete contents,
                    // as an Add or overwrite does, rather than counting edits to the source
                    // that remains unchanged.
                    let written = count_file_lines(update.content());
                    record_change(context, destination, kind, None, written, 0);
                    return Err(Stop {
                        committed: append_delta(committed, destination, written, 0),
                        failure: PatchToolFailure::Message(format!(
                            "failed to remove the original `{}`: {error}",
                            path.display()
                        )),
                    });
                }
                record_change(
                    context,
                    path,
                    FileChangeKind::Moved,
                    Some(destination),
                    added,
                    removed,
                );
                let delta = append_delta(committed, path, added, removed);
                Ok(append_delta(&delta, destination, 0, 0))
            }
        }
    }

    fn read_text(&self, path: &Path) -> std::result::Result<String, PatchToolFailure> {
        let mut file = self
            .filesystem
            .open_read(path)
            .map_err(|error| PatchToolFailure::filesystem(&error))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .map_err(|error| PatchToolFailure::io(&error))?;
        String::from_utf8(bytes).map_err(|_error| {
            PatchToolFailure::Message(format!("`{}` is not UTF-8 text", path.display()))
        })
    }
}

/// Counts logical lines using the applier's LF, CRLF, and lone-CR terminators.
fn count_file_lines(content: &str) -> usize {
    let bytes = content.as_bytes();
    let terminators = bytes
        .iter()
        .enumerate()
        .filter(|&(index, byte)| {
            *byte == b'\n' || (*byte == b'\r' && bytes.get(index + 1) != Some(&b'\n'))
        })
        .count();
    terminators + usize::from(!content.is_empty() && !content.ends_with(['\r', '\n']))
}

/// Records one committed change on the host's event channel, as it lands.
///
/// **Per operation, not per patch.** Application is best effort: three files written and the
/// fourth refused is three files changed, and a record written once at the end would describe
/// that run as having changed nothing — while the three files sit changed on disk. A channel
/// that refuses the record says so in the log rather than silently, for the same reason: the
/// write has already happened, and a log that quietly disagrees with the filesystem is worse
/// than no log.
fn record_change(
    context: &ToolContext<'_>,
    path: &Path,
    kind: FileChangeKind,
    moved_to: Option<&Path>,
    added: usize,
    removed: usize,
) {
    let Some(emitter) = context.event_emitter() else {
        return;
    };
    let mut event =
        FileChangedEvent::new(context.call_id().clone(), path.display().to_string(), kind)
            .with_line_counts(as_u64(added), as_u64(removed));
    if let Some(destination) = moved_to {
        event = event.with_moved_to(destination.display().to_string());
    }
    if let Err(error) = emitter.emit_file(FileEvent::Changed(event)) {
        tracing::warn!(
            path = %path.display(),
            call_id = %context.call_id(),
            %error,
            "a committed file change could not be recorded on the host event channel"
        );
    }
}

/// One Codex operation. The parser gives an update's move as an action of its own after the
/// update; Codex applies the two as one, so a moved file is written once, at its destination.
enum Operation<'a> {
    Add {
        path: &'a Path,
        content: &'a str,
    },
    Delete {
        path: &'a Path,
    },
    Update {
        path: &'a Path,
        hunks: &'a [PatchHunk],
        move_to: Option<&'a Path>,
    },
}

/// The plan's actions as Codex's operations.
fn operations(
    actions: &[PatchAction],
) -> std::result::Result<Vec<Operation<'_>>, PatchToolFailure> {
    let mut operations = Vec::new();
    let mut actions = actions.iter().peekable();
    while let Some(action) = actions.next() {
        operations.push(match action {
            PatchAction::AddFile { path, content } => Operation::Add { path, content },
            PatchAction::DeleteFile { path } => Operation::Delete { path },
            PatchAction::UpdateFile { path, hunks } => {
                let move_to = match actions.peek() {
                    Some(PatchAction::MoveFile { from, to }) if from == path => {
                        actions.next();
                        Some(to.as_path())
                    }
                    _ => None,
                };
                Operation::Update {
                    path,
                    hunks,
                    move_to,
                }
            }
            // A move without changes is an update with no chunks.
            PatchAction::MoveFile { from, to } => Operation::Update {
                path: from,
                hunks: &[],
                move_to: Some(to),
            },
            _ => {
                return Err(PatchToolFailure::Message(
                    "this build cannot apply this patch action".to_owned(),
                ));
            }
        });
    }
    Ok(operations)
}

/// Where application stopped: what was committed before it, and why.
struct Stop {
    committed: CommittedPatchDelta,
    failure: PatchToolFailure,
}

fn as_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

fn append_delta(
    previous: &CommittedPatchDelta,
    path: &Path,
    added: usize,
    removed: usize,
) -> CommittedPatchDelta {
    let mut files = previous.applied_files().to_vec();
    if !files.contains(&path.to_path_buf()) {
        files.push(path.to_path_buf());
    }
    CommittedPatchDelta::new(
        files,
        previous.lines_added() + added,
        previous.lines_removed() + removed,
    )
}

fn render_success(delta: &CommittedPatchDelta) -> String {
    if delta.is_empty() {
        return "Patch contained no changes.".to_owned();
    }
    format!(
        "Applied patch across {} path(s): {} line(s) added, {} line(s) removed.",
        delta.applied_files().len(),
        delta.lines_added(),
        delta.lines_removed()
    )
}

fn render_stop(delta: &CommittedPatchDelta, failure: &PatchToolFailure) -> String {
    if delta.is_empty() {
        format!("Patch was not applied: {failure}.")
    } else {
        format!(
            "Patch stopped after changing {} path(s) ({} line(s) added, {} line(s) removed): {failure}.",
            delta.applied_files().len(),
            delta.lines_added(),
            delta.lines_removed()
        )
    }
}

#[derive(Debug)]
enum PatchToolFailure {
    Conflict(PatchConflict),
    Message(String),
}

impl PatchToolFailure {
    fn filesystem(error: &ra_exec::fs::RootedOpenError) -> Self {
        Self::Message(error.to_string())
    }
    fn io(error: &std::io::Error) -> Self {
        Self::Message(error.to_string())
    }
}

impl fmt::Display for PatchToolFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Conflict(PatchConflict::HunkFailed {
                path, hunk_index, ..
            }) => write!(
                formatter,
                "hunk {} did not match `{}`; read the file again and retry",
                hunk_index + 1,
                path.display()
            ),
            Self::Conflict(PatchConflict::AmbiguousMatch {
                path,
                hunk_index,
                candidate_count,
            }) => write!(
                formatter,
                "hunk {} matches {candidate_count} locations in `{}`; add more context",
                hunk_index + 1,
                path.display()
            ),
            Self::Conflict(conflict) => write!(formatter, "{conflict:?}"),
            Self::Message(message) => formatter.write_str(message),
        }
    }
}
