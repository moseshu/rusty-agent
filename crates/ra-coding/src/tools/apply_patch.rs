//! The coding product's sole file-editing tool.

use std::{fmt, io::Read as _, path::Path, sync::Arc};

use async_trait::async_trait;
use ra_core::{
    error::Result,
    tool::{
        Tool, ToolConcurrency, ToolContext, ToolInput as _, ToolOptions, ToolOrigin, ToolOutput,
        ToolSchema,
    },
};
use ra_exec::fs::RootedFileSystem;
use ra_macros::ToolInput;
use ra_patch::{CommittedPatchDelta, PatchAction, PatchConflict, apply_hunks, parse_patch};
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
pub(crate) struct ApplyPatchTool {
    origin: ToolOrigin,
    schema: ToolSchema,
    options: ToolOptions,
    filesystem: Arc<RootedFileSystem>,
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
    pub(crate) fn new(filesystem: Arc<RootedFileSystem>) -> Result<Self> {
        Ok(Self {
            origin: ToolOrigin::new(TOOL_NAME)?,
            schema: ApplyPatchInput::tool_schema(TOOL_NAME)?,
            options: ToolOptions::new().with_concurrency(ToolConcurrency::Exclusive),
            filesystem,
        })
    }
}

#[async_trait]
impl Tool for ApplyPatchTool {
    fn origin(&self) -> &ToolOrigin {
        &self.origin
    }
    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    async fn call(&self, context: ToolContext<'_>) -> Result<ToolOutput> {
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
        let mut committed = CommittedPatchDelta::empty();
        for action in plan.actions() {
            match self.apply_action(action, &committed) {
                Ok(delta) => committed = delta,
                Err(error) => return Ok(ToolOutput::text(render_stop(&committed, &error))),
            }
        }
        Ok(ToolOutput::text(render_success(&committed)))
    }

    fn options(&self) -> ToolOptions {
        self.options.clone()
    }
}

impl ApplyPatchTool {
    fn apply_action(
        &self,
        action: &PatchAction,
        committed: &CommittedPatchDelta,
    ) -> std::result::Result<CommittedPatchDelta, PatchToolFailure> {
        match action {
            PatchAction::AddFile { path, content } => {
                if self.filesystem.exists(path) {
                    return Err(PatchToolFailure::Conflict(
                        PatchConflict::FileAlreadyExists { path: path.clone() },
                    ));
                }
                self.filesystem
                    .write_file(path, content.as_bytes())
                    .map_err(|error| PatchToolFailure::filesystem(&error))?;
                Ok(append_delta(committed, path, content.lines().count(), 0))
            }
            PatchAction::DeleteFile { path } => {
                if !self.filesystem.exists(path) {
                    return Err(PatchToolFailure::Conflict(PatchConflict::FileNotFound {
                        path: path.clone(),
                    }));
                }
                self.filesystem
                    .remove_file(path)
                    .map_err(|error| PatchToolFailure::filesystem(&error))?;
                Ok(append_delta(committed, path, 0, 0))
            }
            PatchAction::UpdateFile { path, hunks } => {
                let original = self.read_text(path)?;
                let update =
                    apply_hunks(path, &original, hunks).map_err(PatchToolFailure::Conflict)?;
                self.filesystem
                    .write_file(path, update.content().as_bytes())
                    .map_err(|error| PatchToolFailure::filesystem(&error))?;
                Ok(append_delta(
                    committed,
                    path,
                    update.lines_added(),
                    update.lines_removed(),
                ))
            }
            PatchAction::MoveFile { from, to } => {
                if !self.filesystem.exists(from) {
                    return Err(PatchToolFailure::Conflict(
                        PatchConflict::SourceFileNotFound { path: from.clone() },
                    ));
                }
                if self.filesystem.exists(to) {
                    return Err(PatchToolFailure::Conflict(
                        PatchConflict::DestinationAlreadyExists { path: to.clone() },
                    ));
                }
                self.filesystem
                    .rename(from, to)
                    .map_err(|error| PatchToolFailure::filesystem(&error))?;
                let mut files = committed.applied_files().to_vec();
                if !files.contains(from) {
                    files.push(from.clone());
                }
                if !files.contains(to) {
                    files.push(to.clone());
                }
                Ok(CommittedPatchDelta::new(
                    files,
                    committed.lines_added(),
                    committed.lines_removed(),
                ))
            }
            _ => Err(PatchToolFailure::Message(
                "this build cannot apply this patch action".to_owned(),
            )),
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
