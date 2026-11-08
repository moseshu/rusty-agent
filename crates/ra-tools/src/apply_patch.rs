// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
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
//!
//! # Verification, approval, and the patch's item
//!
//! As Codex's handler does, a patch is verified against the workspace before anybody is asked
//! about it and before anything is written: a patch that cannot be parsed fails at decoding, before
//! any hook, and one whose files do not check out fails with `apply_patch verification failed`
//! without an approval being requested. Whether a patch asks stays with the permission stage, so
//! fixed rules and product presets keep deciding it; the tool prepares the question in
//! [`Tool::approval_state`], which verifies the patch and announces it with a
//! [`PatchApplyBeginEvent`] before the hooks or the host are asked. A call that needs no approval
//! verifies and announces itself. A host policy installed with
//! [`ApplyPatchTool::with_approval_resolver`] is consulted again for a refusal once the patch has
//! been verified, so a patch that is both broken and refused is reported as broken, as Codex
//! reports it. Every announced patch ends with one [`PatchApplyEndEvent`]; a refusal the host or a
//! hook gives is recorded by the runtime as the approval's answer instead.
//!
//! Every failure is returned as an error and rendered by [`Tool::handle_failure`], so a failed
//! call is not reported to post-tool hooks as a result.

use std::{collections::HashSet, fmt, io::Read as _, path::Path, sync::Arc};

use async_trait::async_trait;
use ra_core::{
    error::{Error, Result, ToolErrorKind},
    event::{
        FileEvent,
        file::{
            FileChangeKind, FileChangedEvent, PatchApplyBeginEvent, PatchApplyEndEvent,
            PatchApplyStatus, PatchFileChange,
        },
    },
    tool::{
        DecodedToolInput, ResourceClaim, Tool, ToolApprovalPolicy, ToolConcurrency, ToolContext,
        ToolFailureHandling, ToolInput as _, ToolOptions, ToolOrigin, ToolOutput, ToolSchema,
    },
};
use ra_exec::fs::{RootedFileSystem, Workspace};
use ra_macros::ToolInput;
use ra_patch::{
    CommittedPatchDelta, PatchAction, PatchConflict, PatchHunk, PatchPlan, apply_chunks,
    parse_patch,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use crate::exec_command::rejection_reason;

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

    /// Installs a host policy callback.
    ///
    /// The callback says whether a patch asks before it is applied, and refuses one by returning a
    /// [`rejection`](crate::exec_command::rejection). A refusal is not given while the permission
    /// stage decides whether to ask: it is given once the patch has been verified, when the
    /// callback is consulted again immediately before the question is put or the patch is
    /// written. Its `Ok` answer is not consulted then, so a call a fixed rule allows is not asked.
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
        .with_failure_handling(ToolFailureHandling::Custom)
}

/// The state an approval this tool asked for carries: the patch was verified and its begin was
/// announced before the question was put, so the approved call does not announce it again.
fn approval_state_value() -> Value {
    serde_json::json!({ "apply_patch": { "item_started": true } })
}

#[async_trait]
impl Tool for ApplyPatchTool {
    fn origin(&self) -> &ToolOrigin {
        &self.origin
    }
    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    fn decode_input(&self, arguments: &Value) -> Result<Option<DecodedToolInput>> {
        // Parsing only: the patch is read against the workspace later, where reading may block.
        patch_plan(arguments).map_err(ApplyPatchFailure::into_error)?;
        Ok(None)
    }

    async fn needs_approval(&self, context: &ToolContext<'_>) -> Result<bool> {
        let Some(resolve) = &self.approval_resolver else {
            return Ok(true);
        };
        match resolve(context) {
            // A refusal is given after verification, by the call itself; asking about a patch that
            // is going to be refused would put the question before the answer.
            Err(error) if rejection_reason(&error).is_some() => Ok(false),
            answer => answer,
        }
    }

    async fn approval_state(&self, context: &ToolContext<'_>) -> Result<Option<Value>> {
        let checked = self.check(context)?.map_err(|failure| {
            // Asked before the call runs, a failure ends the call without a question. The runtime
            // shapes only invalid input there; the text the model reads is the failure's own.
            failure.into_error_of_kind(ToolErrorKind::InvalidInput)
        })?;
        announce_begin(context, checked.changes);
        Ok(Some(approval_state_value()))
    }

    async fn call(&self, context: ToolContext<'_>) -> Result<ToolOutput> {
        let begun = resumed_after_begin(&context)?;
        // An announced patch ends with an end event however the call ends, and one that was never
        // announced has no item to end.
        let end = |status, failure: ApplyPatchFailure| {
            if begun {
                announce_end(&context, status, &failure.to_string());
            }
            Err(failure.into_error())
        };
        let checked = match self.check(&context)? {
            Ok(checked) => checked,
            Err(failure @ ApplyPatchFailure::Rejected(_)) => {
                return end(PatchApplyStatus::Declined, failure);
            }
            Err(failure) => return end(PatchApplyStatus::Failed, failure),
        };
        if !begun {
            announce_begin(&context, checked.changes);
        }
        let operations = operations(checked.plan.actions())
            .map_err(|failure| ApplyPatchFailure::Verification(failure.to_string()).into_error())?;
        let mut committed = CommittedPatchDelta::empty();
        for operation in &operations {
            match self.apply_operation(&context, operation, &committed) {
                Ok(delta) => committed = delta,
                Err(stop) => {
                    let failure =
                        ApplyPatchFailure::Application(render_stop(&stop.committed, &stop.failure));
                    announce_end(&context, PatchApplyStatus::Failed, &failure.to_string());
                    return Err(failure.into_error());
                }
            }
        }
        let text = render_success(&committed);
        announce_end(&context, PatchApplyStatus::Completed, &text);
        Ok(ToolOutput::text(text))
    }

    async fn handle_failure(
        &self,
        _context: &ToolContext<'_>,
        error: &Error,
    ) -> Result<Option<ToolOutput>> {
        // Matched on the typed cause the error carries, never on its message.
        Ok(ApplyPatchFailure::of(error).map(|failure| ToolOutput::text(failure.to_string())))
    }

    fn options(&self) -> ToolOptions {
        self.options.clone()
    }
}

/// A patch that parsed, verified against the workspace, and was not refused.
struct Checked {
    plan: PatchPlan,
    changes: Vec<PatchFileChange>,
}

impl ApplyPatchTool {
    /// Parses and verifies the call's patch, then asks the host policy whether it refuses it.
    ///
    /// Verification comes first, as Codex verifies a patch before its safety check. The outer
    /// error is a policy that failed rather than refused; it is not the patch's failure.
    fn check(
        &self,
        context: &ToolContext<'_>,
    ) -> Result<std::result::Result<Checked, ApplyPatchFailure>> {
        let checked = match self.verified(context.arguments()) {
            Ok(checked) => checked,
            Err(failure) => return Ok(Err(failure)),
        };
        let Some(resolve) = &self.approval_resolver else {
            return Ok(Ok(checked));
        };
        match resolve(context) {
            Ok(_) => Ok(Ok(checked)),
            Err(error) => match rejection_reason(&error) {
                Some(reason) => Ok(Err(ApplyPatchFailure::Rejected(reason.to_owned()))),
                None => Err(error),
            },
        }
    }

    fn verified(&self, arguments: &Value) -> std::result::Result<Checked, ApplyPatchFailure> {
        let plan = patch_plan(arguments)?;
        let changes = {
            let operations = operations(plan.actions())
                .map_err(|failure| ApplyPatchFailure::Verification(failure.to_string()))?;
            self.verify(&operations)
                .map_err(ApplyPatchFailure::Verification)?
        };
        Ok(Checked { plan, changes })
    }

    /// Codex's `verify_apply_patch_args`: each operation checked against the workspace as it is
    /// now, on its own, with nothing written. Returns the changes the patch's begin announces.
    fn verify(
        &self,
        operations: &[Operation<'_>],
    ) -> std::result::Result<Vec<PatchFileChange>, String> {
        let mut targeted = HashSet::new();
        let mut changes = Vec::with_capacity(operations.len());
        for operation in operations {
            let path = operation.path();
            // Keyed as Codex keys a patch's changes, by the path each resolves to: `./a` and `a`
            // name one file.
            let resolved: std::path::PathBuf = path
                .components()
                .filter(|component| !matches!(component, std::path::Component::CurDir))
                .collect();
            if !targeted.insert(resolved) {
                return Err(format!("multiple operations target {}", path.display()));
            }
            let read = |path: &Path| {
                self.read_text(path)
                    .map_err(|failure| format!("Failed to read {}: {failure}", path.display()))
            };
            changes.push(match *operation {
                Operation::Add { path, content } => {
                    PatchFileChange::new(display(path), FileChangeKind::Added, content)
                }
                Operation::Delete { path } => {
                    read(path)?;
                    PatchFileChange::new(display(path), FileChangeKind::Deleted, "")
                }
                Operation::Update {
                    path,
                    hunks,
                    move_to,
                } => {
                    let original = read(path)?;
                    apply_chunks(path, &original, hunks)
                        .map_err(|conflict| PatchToolFailure::Conflict(conflict).to_string())?;
                    match move_to {
                        Some(destination) => PatchFileChange::new(
                            display(path),
                            FileChangeKind::Moved,
                            hunks_diff(hunks),
                        )
                        .with_moved_to(display(destination)),
                        None => PatchFileChange::new(
                            display(path),
                            FileChangeKind::Updated,
                            hunks_diff(hunks),
                        ),
                    }
                }
            });
        }
        Ok(changes)
    }

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

/// The patch text of a call's arguments, parsed.
///
/// The provider-neutral call item carries JSON. A bare JSON string is accepted so this handler
/// stays compatible with the custom/freeform wire item once that item is introduced, while the
/// object form remains available to function-only adapters.
fn patch_plan(arguments: &Value) -> std::result::Result<PatchPlan, ApplyPatchFailure> {
    let patch = match arguments {
        Value::String(patch) => patch.clone(),
        arguments => {
            serde_json::from_value::<ApplyPatchInput>(arguments.clone())
                .map_err(|error| {
                    ApplyPatchFailure::Verification(format!("invalid arguments: {error}"))
                })?
                .patch
        }
    };
    parse_patch(&patch).map_err(|error| ApplyPatchFailure::Verification(error.to_string()))
}

/// Whether this call resumes an approval this tool asked for, whose begin it already announced.
fn resumed_after_begin(context: &ToolContext<'_>) -> Result<bool> {
    match context.resume_state() {
        None => Ok(false),
        Some(state) if *state == approval_state_value() => Ok(true),
        Some(_) => Err(ApplyPatchFailure::ApprovalState.into_error()),
    }
}

/// Announces a verified patch before it is asked about or applied: Codex's `emitter.begin`.
fn announce_begin(context: &ToolContext<'_>, changes: Vec<PatchFileChange>) {
    let event = PatchApplyBeginEvent::new(context.call_id().clone(), changes);
    emit_patch_event(context, FileEvent::PatchApplyBegin(event));
}

/// Ends an announced patch: Codex's `emitter.finish`.
fn announce_end(context: &ToolContext<'_>, status: PatchApplyStatus, output: &str) {
    let event = PatchApplyEndEvent::new(context.call_id().clone(), status, output);
    emit_patch_event(context, FileEvent::PatchApplyEnd(event));
}

/// A host without an event channel has no item to show, as with committed changes.
fn emit_patch_event(context: &ToolContext<'_>, event: FileEvent) {
    let Some(emitter) = context.event_emitter() else {
        return;
    };
    if let Err(error) = emitter.emit_file(event) {
        tracing::warn!(
            call_id = %context.call_id(),
            %error,
            "a patch boundary could not be recorded on the host event channel"
        );
    }
}

fn display(path: &Path) -> String {
    path.display().to_string()
}

/// A changed file's hunks as the patch gives them: context lines, then the lines it removes and
/// the lines it adds, each under the anchor it is found by.
fn hunks_diff(hunks: &[PatchHunk]) -> String {
    let mut diff = String::new();
    for hunk in hunks {
        let anchors = hunk.context_anchors().join(" ");
        diff.push_str("@@");
        if !anchors.is_empty() {
            diff.push(' ');
            diff.push_str(&anchors);
        }
        diff.push('\n');
        let lines = hunk
            .context_before()
            .iter()
            .map(|line| (' ', line))
            .chain(hunk.removed_lines().iter().map(|line| ('-', line)))
            .chain(hunk.added_lines().iter().map(|line| ('+', line)))
            .chain(hunk.context_after().iter().map(|line| (' ', line)));
        for (prefix, line) in lines {
            diff.push(prefix);
            diff.push_str(line);
            diff.push('\n');
        }
    }
    diff
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

impl Operation<'_> {
    /// The path the operation is keyed by: its source, as Codex keys a patch's changes.
    const fn path(&self) -> &Path {
        match *self {
            Self::Add { path, .. } | Self::Delete { path } | Self::Update { path, .. } => path,
        }
    }
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

/// Why a patch call ended without its patch applied in full, carried as the error's typed source
/// so [`Tool::handle_failure`] renders it without reading prose back.
#[derive(Debug)]
enum ApplyPatchFailure {
    /// The arguments, the patch text, or the patch against the workspace did not check out.
    Verification(String),
    /// The host's patch policy refused the patch; the text is its reason, read verbatim.
    Rejected(String),
    /// Application stopped; the text reports what was written before it did.
    Application(String),
    /// The approval this call resumes from is not one this tool asked for.
    ApprovalState,
}

impl ApplyPatchFailure {
    const fn kind(&self) -> ToolErrorKind {
        match self {
            Self::Verification(_) | Self::ApprovalState => ToolErrorKind::InvalidInput,
            Self::Rejected(_) => ToolErrorKind::PermissionDenied,
            Self::Application(_) => ToolErrorKind::ExecutionFailed,
        }
    }

    fn into_error(self) -> Error {
        let kind = self.kind();
        self.into_error_of_kind(kind)
    }

    fn into_error_of_kind(self, kind: ToolErrorKind) -> Error {
        Error::tool(kind, TOOL_NAME, self.to_string()).with_source(self)
    }

    fn of(error: &Error) -> Option<&Self> {
        std::error::Error::source(error).and_then(<dyn std::error::Error + 'static>::downcast_ref)
    }
}

impl fmt::Display for ApplyPatchFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Verification(diagnosis) => {
                write!(formatter, "apply_patch verification failed: {diagnosis}")
            }
            Self::Rejected(text) | Self::Application(text) => formatter.write_str(text),
            Self::ApprovalState => {
                formatter.write_str("apply_patch cannot resume an approval it did not ask for")
            }
        }
    }
}

impl std::error::Error for ApplyPatchFailure {}
