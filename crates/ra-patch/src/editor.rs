//! The operations an `apply_patch` call is made of, and what applying one reports.
//!
//! A port of `openai-agents-python`'s `agents/editor.py`. A patch is split into one operation per
//! file — create, update, or delete — and each is handed to whatever edits files for the host.
//!
//! The reference's operation also carries the run context it was requested in (`ctx_wrapper`).
//! Here the context travels beside the operation to whoever needs it, so the operation stays a
//! plain value that can be compared, cloned and shown to an approval check.

use std::fmt;

use serde::{Deserialize, Serialize};

/// What an operation does to its file.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApplyPatchOperationType {
    /// Writes a new file from an all-insertions diff.
    CreateFile,
    /// Applies a diff to an existing file, optionally moving it.
    UpdateFile,
    /// Removes an existing file.
    DeleteFile,
}

impl ApplyPatchOperationType {
    /// The reference's name for it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CreateFile => "create_file",
            Self::UpdateFile => "update_file",
            Self::DeleteFile => "delete_file",
        }
    }
}

impl fmt::Display for ApplyPatchOperationType {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// One file operation the model asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyPatchOperation {
    kind: ApplyPatchOperationType,
    path: String,
    diff: Option<String>,
    move_to: Option<String>,
}

impl ApplyPatchOperation {
    /// An operation of `kind` on `path`, with no diff and no move.
    ///
    /// Creates and updates need a diff, added with [`Self::with_diff`]; applying one without it is
    /// refused then, not here, as the reference does.
    #[must_use]
    pub fn new(kind: ApplyPatchOperationType, path: impl Into<String>) -> Self {
        Self {
            kind,
            path: path.into(),
            diff: None,
            move_to: None,
        }
    }

    /// Creates `path` from `diff`.
    #[must_use]
    pub fn create_file(path: impl Into<String>, diff: impl Into<String>) -> Self {
        Self::new(ApplyPatchOperationType::CreateFile, path).with_diff(diff)
    }

    /// Applies `diff` to `path`.
    #[must_use]
    pub fn update_file(path: impl Into<String>, diff: impl Into<String>) -> Self {
        Self::new(ApplyPatchOperationType::UpdateFile, path).with_diff(diff)
    }

    /// Removes `path`.
    #[must_use]
    pub fn delete_file(path: impl Into<String>) -> Self {
        Self::new(ApplyPatchOperationType::DeleteFile, path)
    }

    /// Sets the diff.
    #[must_use]
    pub fn with_diff(mut self, diff: impl Into<String>) -> Self {
        self.diff = Some(diff.into());
        self
    }

    /// Moves the updated file to `move_to`.
    #[must_use]
    pub fn with_move_to(mut self, move_to: impl Into<String>) -> Self {
        self.move_to = Some(move_to.into());
        self
    }

    /// What the operation does.
    #[must_use]
    pub const fn kind(&self) -> ApplyPatchOperationType {
        self.kind
    }

    /// The file, as the model wrote it.
    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }

    /// The diff, when there is one.
    #[must_use]
    pub fn diff(&self) -> Option<&str> {
        self.diff.as_deref()
    }

    /// Where an update moves the file, when it does.
    #[must_use]
    pub fn move_to(&self) -> Option<&str> {
        self.move_to.as_deref()
    }
}

/// How an applied operation ended, when the editor says.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApplyPatchStatus {
    /// It was applied.
    Completed,
    /// It was not.
    Failed,
}

/// What applying one operation reports.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ApplyPatchResult {
    status: Option<ApplyPatchStatus>,
    output: Option<String>,
}

impl ApplyPatchResult {
    /// A result with `output` and no status.
    #[deprecated(
        note = "build it as `ApplyPatchResult::default().with_output(..)`, as the reference's \
                `ApplyPatchResult(output=..)` does; `output` becomes the reader in 0.3.0"
    )]
    #[must_use]
    pub fn output(output: impl Into<String>) -> Self {
        Self::default().with_output(output)
    }

    /// Sets what the editor said it did.
    #[must_use]
    pub fn with_output(mut self, output: impl Into<String>) -> Self {
        self.output = Some(output.into());
        self
    }

    /// Sets the status.
    #[must_use]
    pub const fn with_status(mut self, status: ApplyPatchStatus) -> Self {
        self.status = Some(status);
        self
    }

    /// The status, when the editor gave one.
    #[must_use]
    pub const fn status(&self) -> Option<ApplyPatchStatus> {
        self.status
    }

    /// What the editor said it did.
    ///
    /// The reference names this field `output`. The name is held by the deprecated constructor
    /// until 0.3.0, which removes that constructor and names this reader `output`.
    #[must_use]
    pub fn output_text(&self) -> Option<&str> {
        self.output.as_deref()
    }
}
