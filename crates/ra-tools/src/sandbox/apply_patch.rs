//! Applying patch operations to a sandbox session's workspace.
//!
//! A port of the reference's `sandbox/apply_patch.py`: the [`WorkspaceEditor`] that turns one
//! [`ApplyPatchOperation`] into reads, writes and removals on a session, the [`PatchFormat`] it
//! applies diffs with, and the coercion of JSON operation payloads.
//!
//! # Paths
//!
//! A path the model wrote is anchored under the run's working directory when relative, re-measured
//! from the workspace root — which refuses anything outside the root, grants included — and then
//! resolved by the session, so a backend that follows links decides where the file really is. What
//! the editor reports names the file as the model asked for it: relative to the working directory
//! for a relative request, relative to the root for an absolute one. Neither ever shows where the
//! backend keeps the workspace.
//!
//! # Deviations from the reference
//!
//! - **The session has no `apply_patch` method.** The reference's `BaseSandboxSession.apply_patch`
//!   is `WorkspaceEditor(self).apply_patch(...)`. The session protocol lives in `ra-core`, which
//!   does not carry a diff algorithm, so a caller builds the editor itself:
//!   `WorkspaceEditor::new(session).apply_patch(operations)` is that method, root-relative and as
//!   the session's own user.
//! - **Payloads are typed.** The reference accepts operation objects, dictionaries, or a list of
//!   either, and a format that is either an object or the string `"v4a"`. Here operations are
//!   [`ApplyPatchOperation`] values and a format is a [`PatchFormat`]; [`operations_from_json`]
//!   keeps the dictionary coercion and its refusals for callers that hold JSON.
//! - **A missing file is the session's not-found failure.** The reference also catches Python's
//!   `FileNotFoundError`; a session here reports every missing file as
//!   [`ErrorCode::WorkspaceReadNotFound`].

use ra_core::sandbox::{
    ApplyPatchPathReason, ErrorCode, PosixPath, SandboxError, SandboxResult, SandboxSession,
    SandboxWorkspaceScope, User, windows_absolute_path,
};
use ra_patch::{
    ApplyDiffError, ApplyDiffMode, ApplyPatchOperation, ApplyPatchOperationType, ApplyPatchResult,
    apply_diff,
};
use serde_json::Value;

/// A diff language the editor can apply.
///
/// The reference's `PatchFormat` protocol.
pub trait PatchFormat: Send + Sync {
    /// Applies `diff` to `input`.
    ///
    /// # Errors
    ///
    /// Returns why the diff does not apply; the editor reports it as an invalid diff.
    fn apply_diff(
        &self,
        input: &str,
        diff: &str,
        mode: ApplyDiffMode,
    ) -> Result<String, ApplyDiffError>;
}

/// The V4A format, and the editor's default.
#[derive(Debug, Clone, Copy, Default)]
pub struct V4aFormat;

impl PatchFormat for V4aFormat {
    fn apply_diff(
        &self,
        input: &str,
        diff: &str,
        mode: ApplyDiffMode,
    ) -> Result<String, ApplyDiffError> {
        apply_diff(input, diff, mode)
    }
}

/// What `apply_patch` answers once every operation has been applied.
pub const APPLY_PATCH_DONE: &str = "Done!";

/// Applies patch operations to one session's workspace.
#[derive(Clone)]
pub struct WorkspaceEditor<'a> {
    session: &'a dyn SandboxSession,
    user: Option<User>,
    workspace_scope: SandboxWorkspaceScope,
}

impl std::fmt::Debug for WorkspaceEditor<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WorkspaceEditor")
            .field("backend", &self.session.backend_id())
            .field("user", &self.user)
            .field("workspace_scope", &self.workspace_scope)
            .finish()
    }
}

impl<'a> WorkspaceEditor<'a> {
    /// An editor over `session`'s workspace, measuring relative paths from its root and acting as
    /// the session's own user.
    #[must_use]
    pub fn new(session: &'a dyn SandboxSession) -> Self {
        Self {
            session,
            user: None,
            workspace_scope: SandboxWorkspaceScope::root(),
        }
    }

    /// Reads, writes and removes files as `user`.
    #[must_use]
    pub fn with_user(mut self, user: Option<User>) -> Self {
        self.user = user;
        self
    }

    /// Measures relative paths from `workspace_scope`.
    #[must_use]
    pub fn with_workspace_scope(mut self, workspace_scope: SandboxWorkspaceScope) -> Self {
        self.workspace_scope = workspace_scope;
        self
    }

    /// Applies each operation in order with the V4A format.
    ///
    /// # Errors
    ///
    /// As [`Self::apply_operation`]. Operations before the failing one stay applied.
    pub async fn apply_patch(&self, operations: &[ApplyPatchOperation]) -> SandboxResult<String> {
        self.apply_patch_with_format(operations, &V4aFormat).await
    }

    /// Applies each operation in order with `format`.
    ///
    /// # Errors
    ///
    /// As [`Self::apply_operation`]. Operations before the failing one stay applied.
    pub async fn apply_patch_with_format(
        &self,
        operations: &[ApplyPatchOperation],
        format: &dyn PatchFormat,
    ) -> SandboxResult<String> {
        for operation in operations {
            self.apply_operation_with_format(operation, format).await?;
        }
        Ok(APPLY_PATCH_DONE.to_owned())
    }

    /// Applies one operation with the V4A format.
    ///
    /// # Errors
    ///
    /// As [`Self::apply_operation_with_format`].
    pub async fn apply_operation(
        &self,
        operation: &ApplyPatchOperation,
    ) -> SandboxResult<ApplyPatchResult> {
        self.apply_operation_with_format(operation, &V4aFormat)
            .await
    }

    /// Applies one operation with `format`.
    ///
    /// A delete checks that the file is there and removes it. A create writes the diff's lines. An
    /// update reads the file, applies the diff, and writes the result — to the new path when it
    /// moves, removing the old one unless both resolve to the same file. Parent directories are
    /// created as needed.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::ApplyPatchInvalidPath`] for a blank path or one outside the workspace,
    /// [`ErrorCode::ApplyPatchFileNotFound`] for a missing file,
    /// [`ErrorCode::ApplyPatchDecodeError`] for a file that is not UTF-8,
    /// [`ErrorCode::ApplyPatchInvalidDiff`] for a missing diff or one that does not apply, and the
    /// session's own failures otherwise.
    pub async fn apply_operation_with_format(
        &self,
        operation: &ApplyPatchOperation,
        format: &dyn PatchFormat,
    ) -> SandboxResult<ApplyPatchResult> {
        let (relative_path, display_path) = self.resolve_path(operation.path())?;
        let destination = self
            .session
            .validate_path_access(relative_path.as_str(), false)
            .await?;

        if operation.kind() == ApplyPatchOperationType::DeleteFile {
            self.ensure_exists(&destination, &display_path).await?;
            self.session
                .rm(&destination, false, self.user.clone())
                .await?;
            return Ok(ApplyPatchResult::output(format!("Deleted {display_path}")));
        }

        let Some(diff) = operation.diff() else {
            return Err(SandboxError::apply_patch_invalid_diff(
                format!(
                    "Missing diff for operation type {} on path {}",
                    operation.kind(),
                    operation.path()
                ),
                Some(operation.path()),
            ));
        };

        if operation.kind() == ApplyPatchOperationType::UpdateFile {
            // With a working directory the file is named as the model named it, so the failure
            // does not show where the run's directory sits in the workspace.
            let decode_path = if self.workspace_scope.cwd().is_some() {
                let named = if is_absolute_sandbox_path(operation.path()) {
                    operation.path()
                } else {
                    &display_path
                };
                PosixPath::coerce(named).to_string()
            } else {
                PosixPath::new(destination.as_str()).to_string()
            };
            let original = self
                .read_text(&destination, operation.path(), &decode_path)
                .await?;
            let updated = format
                .apply_diff(&original, diff, ApplyDiffMode::Default)
                .map_err(|error| invalid_diff(error, operation.path()))?;

            let Some(move_to) = operation.move_to() else {
                self.write_text(&destination, &updated).await?;
                return Ok(ApplyPatchResult::output(format!("Updated {display_path}")));
            };

            let (moved_relative_path, moved_display_path) = self.resolve_path(move_to)?;
            let moved_destination = self
                .session
                .validate_path_access(moved_relative_path.as_str(), false)
                .await?;
            self.write_text(&moved_destination, &updated).await?;
            if moved_destination != destination {
                self.session
                    .rm(&destination, false, self.user.clone())
                    .await?;
            }
            return Ok(ApplyPatchResult::output(format!(
                "Updated {display_path}\nMoved {display_path} to {moved_display_path}"
            )));
        }

        let created = format
            .apply_diff("", diff, ApplyDiffMode::Create)
            .map_err(|error| invalid_diff(error, operation.path()))?;
        self.write_text(&destination, &created).await?;
        Ok(ApplyPatchResult::output(format!("Created {display_path}")))
    }

    /// The operation with its paths in the canonical form of the workspace's path policy.
    ///
    /// Relative to the workspace root, after anchoring under the run's working directory — the
    /// form an approval check is shown, so it sees the file the operation will actually touch.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorCode::ApplyPatchInvalidPath`] for a blank path or one outside the workspace.
    pub fn normalize_operation(
        &self,
        operation: &ApplyPatchOperation,
    ) -> SandboxResult<ApplyPatchOperation> {
        let mut normalized = ApplyPatchOperation::new(
            operation.kind(),
            self.validate_path(operation.path())?.as_str(),
        );
        if let Some(diff) = operation.diff() {
            normalized = normalized.with_diff(diff);
        }
        if let Some(move_to) = operation.move_to() {
            normalized = normalized.with_move_to(self.validate_path(move_to)?.as_str());
        }
        Ok(normalized)
    }

    /// The workspace-relative path to act on, and the path to report.
    fn resolve_path(&self, path: &str) -> SandboxResult<(PosixPath, String)> {
        let relative_path = self.validate_path(path)?;
        let normalized_path = PosixPath::coerce(path);
        let display_path = self
            .workspace_scope
            .display_path(normalized_path.as_str(), relative_path.as_str())
            .map_err(|error| {
                SandboxError::apply_patch_invalid_path(path, ApplyPatchPathReason::EscapeRoot)
                    .with_cause(error)
            })?;
        Ok((relative_path, display_path.to_string()))
    }

    /// Re-measures a model's path from the workspace root.
    ///
    /// The text is left as the model wrote it until the path policy reads it, apart from treating
    /// backslashes as separators, so what a backslash means does not depend on the host this runs
    /// on.
    fn validate_path(&self, path: &str) -> SandboxResult<PosixPath> {
        if path.chars().all(is_py_space) {
            return Err(SandboxError::apply_patch_invalid_path(
                path,
                ApplyPatchPathReason::Empty,
            ));
        }
        let normalized_path = PosixPath::coerce(path);
        let scoped_path = self.workspace_scope.anchor(normalized_path.as_str());
        self.session
            .workspace_path_policy()?
            .relative_path(&scoped_path)
            .map_err(|error| {
                if error.error_code() == ErrorCode::InvalidManifestPath {
                    SandboxError::apply_patch_invalid_path(path, ApplyPatchPathReason::EscapeRoot)
                        .with_sandbox_cause(error)
                } else {
                    error
                }
            })
    }

    async fn ensure_exists(&self, destination: &str, display_path: &str) -> SandboxResult<()> {
        match self.session.read(destination, self.user.clone()).await {
            Ok(_) => Ok(()),
            Err(error) => Err(not_found_or(error, display_path)),
        }
    }

    async fn read_text(
        &self,
        destination: &str,
        operation_path: &str,
        decode_path: &str,
    ) -> SandboxResult<String> {
        let payload = self
            .session
            .read(destination, self.user.clone())
            .await
            .map_err(|error| not_found_or(error, operation_path))?;
        String::from_utf8(payload)
            .map_err(|error| SandboxError::apply_patch_decode_error(decode_path).with_cause(error))
    }

    async fn write_text(&self, destination: &str, text: &str) -> SandboxResult<()> {
        self.session
            .mkdir(&parent(destination), true, self.user.clone())
            .await?;
        self.session
            .write(destination, text.as_bytes().to_vec(), self.user.clone())
            .await
    }
}

/// Reports a session's not-found as the patch's missing file, naming it as `path`; anything else is
/// passed through.
fn not_found_or(error: SandboxError, path: &str) -> SandboxError {
    if error.error_code() == ErrorCode::WorkspaceReadNotFound {
        // The reference names the file through a host path, which tidies its spelling.
        SandboxError::apply_patch_file_not_found(PosixPath::new(path).as_str())
            .with_sandbox_cause(error)
    } else {
        error
    }
}

fn invalid_diff(error: ApplyDiffError, path: &str) -> SandboxError {
    SandboxError::apply_patch_invalid_diff(error.message().to_owned(), Some(path)).with_cause(error)
}

/// Whether Python's `str.isspace` holds for `character`.
fn is_py_space(character: char) -> bool {
    character.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&character)
}

/// Whether a path is absolute in POSIX or Windows drive syntax.
fn is_absolute_sandbox_path(path: &str) -> bool {
    windows_absolute_path(path).is_some() || path.starts_with('/')
}

/// The directory holding `path`, as a host path's `parent` answers: the root is its own parent,
/// and a single relative name's is `.`.
fn parent(path: &str) -> String {
    let path = PosixPath::new(path);
    let parts = path.parts();
    let Some((_, init)) = parts.split_last() else {
        return ".".to_owned();
    };
    match init.split_first() {
        None if path.is_absolute() => path.to_string(),
        None => ".".to_owned(),
        // An absolute path's first part is its anchor, which already ends in a separator.
        Some((anchor, rest)) if path.is_absolute() => format!("{anchor}{}", rest.join("/")),
        Some(_) => init.join("/"),
    }
}

/// Reads operations from JSON the way the reference coerces its dictionaries.
///
/// Takes one operation object or an array of them. Each needs a `type` of `create_file`,
/// `update_file` or `delete_file` and a string `path`; `diff` and `move_to` are optional strings.
/// Anything else is refused with the reference's message, which names the offending value by its
/// Python type — `int`, `NoneType`, `dict` — because that is what a model reading the reference's
/// wording has been shown.
///
/// # Errors
///
/// Returns [`ErrorCode::ApplyPatchInvalidDiff`] for a payload that is not an object or array, or an
/// operation with a missing or mistyped field.
pub fn operations_from_json(value: &Value) -> SandboxResult<Vec<ApplyPatchOperation>> {
    match value {
        Value::Object(_) => Ok(vec![operation_from_json(value)?]),
        Value::Array(operations) => operations
            .iter()
            .map(|operation| {
                if operation.is_object() {
                    operation_from_json(operation)
                } else {
                    Err(SandboxError::apply_patch_invalid_diff(
                        format!(
                            "Invalid apply_patch operation type: {}",
                            python_type_name(Some(operation))
                        ),
                        None,
                    ))
                }
            })
            .collect(),
        other => Err(SandboxError::apply_patch_invalid_diff(
            format!(
                "Invalid apply_patch operations payload: {}",
                python_type_name(Some(other))
            ),
            None,
        )),
    }
}

fn operation_from_json(operation: &Value) -> SandboxResult<ApplyPatchOperation> {
    let field = |name: &str| operation.get(name).filter(|value| !value.is_null());
    let refuse = |what: &str, value: Option<&Value>| {
        SandboxError::apply_patch_invalid_diff(
            format!(
                "Invalid apply_patch {what} type: {}",
                python_type_name(value)
            ),
            None,
        )
    };

    let kind = match field("type").and_then(Value::as_str) {
        Some("create_file") => ApplyPatchOperationType::CreateFile,
        Some("update_file") => ApplyPatchOperationType::UpdateFile,
        Some("delete_file") => ApplyPatchOperationType::DeleteFile,
        _ => return Err(refuse("operation", field("type"))),
    };
    let Some(path) = field("path").and_then(Value::as_str) else {
        return Err(refuse("path", field("path")));
    };
    let mut parsed = ApplyPatchOperation::new(kind, path);
    match field("diff") {
        None => {}
        Some(Value::String(diff)) => parsed = parsed.with_diff(diff.as_str()),
        other => return Err(refuse("diff", other)),
    }
    match field("move_to") {
        None => {}
        Some(Value::String(move_to)) => parsed = parsed.with_move_to(move_to.as_str()),
        other => return Err(refuse("move_to", other)),
    }
    Ok(parsed)
}

/// The Python type a JSON value decodes to, which is how the reference names a mistyped field.
fn python_type_name(value: Option<&Value>) -> &'static str {
    match value {
        None | Some(Value::Null) => "NoneType",
        Some(Value::Bool(_)) => "bool",
        Some(Value::Number(number)) if number.is_f64() => "float",
        Some(Value::Number(_)) => "int",
        Some(Value::String(_)) => "str",
        Some(Value::Array(_)) => "list",
        Some(Value::Object(_)) => "dict",
    }
}
