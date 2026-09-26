//! The structured error every sandbox operation fails with.
//!
//! # One error type, not a hierarchy
//!
//! The reference implementation grows a class per failure and groups them by inheritance. What its
//! callers actually branch on is the leaf: the recovery sites catch a specific archive or exec
//! failure, not the category above it. So the discriminator here is [`ErrorCode`] — the same stable
//! string the reference documents as its machine-readable handle — and [`ErrorCategory`] is derived
//! from it for the few places that want the coarser grouping. Callers `match` on a code instead of
//! downcasting, and a new code cannot silently join a category it was never classified into.
//!
//! # Relation to the framework taxonomy
//!
//! [`crate::error::SandboxErrorKind`] is a different thing with a similar name, and the two are not
//! yet connected. That one is a slot in the framework-wide `Error` — four coarse reasons a host
//! sees when something crossed a layer boundary. This one is the sandbox protocol's own failure,
//! with the codes the reference implementation publishes. A projection from these codes into that
//! taxonomy belongs where a backend first surfaces an error to the rest of the framework, and is
//! deliberately not written here: inventing the mapping before there is a caller would fix a
//! correspondence nothing has tested.

use std::fmt;

use serde::{Deserialize, Serialize};

use super::types::ErrorContext;

mod constructors;
pub use constructors::{PTY_STDIN_UNAVAILABLE_MESSAGE, SandboxErrorDetails};

/// The stable, machine-readable identity of a sandbox failure.
///
/// These strings cross process boundaries: they are what a host branches on and what a transcript
/// records. Renaming one is a breaking change even when the surrounding message is rewritten.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// A manifest path was absolute or escaped the workspace root.
    InvalidManifestPath,
    /// A workspace write named no compression scheme, or one that is not supported.
    InvalidCompressionScheme,
    /// A port is not configured, or cannot be resolved for host access.
    ExposedPortUnavailable,
    /// A command ran to completion and exited non-zero.
    ExecNonzero,
    /// A command did not finish within its timeout.
    ExecTimeout,
    /// The transport carrying a command failed.
    ExecTransportError,
    /// A PTY session was addressed after it stopped existing.
    PtySessionNotFound,
    /// A patch named a path it may not touch.
    ApplyPatchInvalidPath,
    /// A patch could not be parsed or did not apply.
    ApplyPatchInvalidDiff,
    /// A patch updated a file that is not there.
    ApplyPatchFileNotFound,
    /// A patched file's bytes are not decodable as text.
    ApplyPatchDecodeError,
    /// A workspace read named a path that is not there.
    WorkspaceReadNotFound,
    /// An archive could not be read out of the workspace.
    WorkspaceArchiveReadError,
    /// An archive could not be written into the workspace.
    WorkspaceArchiveWriteError,
    /// A workspace write was handed a payload type it cannot store.
    WorkspaceWriteTypeError,
    /// Persisting the workspace failed while the session was stopping.
    WorkspaceStopError,
    /// Preparing the workspace failed while the session was starting.
    WorkspaceStartError,
    /// The workspace root does not exist.
    WorkspaceRootNotFound,
    /// A local file named by the manifest could not be read.
    LocalFileReadError,
    /// A local directory named by the manifest could not be read.
    LocalDirReadError,
    /// A local artifact did not match its declared checksum.
    LocalChecksumError,
    /// The image has no `git`, and the manifest needs one.
    GitMissingInImage,
    /// Cloning a repository named by the manifest failed.
    GitCloneError,
    /// A repository subpath named by the manifest is not usable.
    GitSubpathError,
    /// Copying a cloned repository into the workspace failed.
    GitCopyError,
    /// The tool a mount needs is not installed.
    MountMissingTool,
    /// A mount command failed.
    MountFailed,
    /// A mount's configuration is not valid.
    MountConfigInvalid,
    /// A skills configuration is not valid.
    SkillsConfigInvalid,
    /// A sandbox configuration is not valid.
    SandboxConfigInvalid,
    /// Persisting a snapshot failed.
    SnapshotPersistError,
    /// Restoring a snapshot failed.
    SnapshotRestoreError,
    /// A snapshot exists but cannot be restored from.
    SnapshotNotRestorable,
}

impl ErrorCode {
    /// The code's wire string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidManifestPath => "invalid_manifest_path",
            Self::InvalidCompressionScheme => "invalid_compression_scheme",
            Self::ExposedPortUnavailable => "exposed_port_unavailable",
            Self::ExecNonzero => "exec_nonzero",
            Self::ExecTimeout => "exec_timeout",
            Self::ExecTransportError => "exec_transport_error",
            Self::PtySessionNotFound => "pty_session_not_found",
            Self::ApplyPatchInvalidPath => "apply_patch_invalid_path",
            Self::ApplyPatchInvalidDiff => "apply_patch_invalid_diff",
            Self::ApplyPatchFileNotFound => "apply_patch_file_not_found",
            Self::ApplyPatchDecodeError => "apply_patch_decode_error",
            Self::WorkspaceReadNotFound => "workspace_read_not_found",
            Self::WorkspaceArchiveReadError => "workspace_archive_read_error",
            Self::WorkspaceArchiveWriteError => "workspace_archive_write_error",
            Self::WorkspaceWriteTypeError => "workspace_write_type_error",
            Self::WorkspaceStopError => "workspace_stop_error",
            Self::WorkspaceStartError => "workspace_start_error",
            Self::WorkspaceRootNotFound => "workspace_root_not_found",
            Self::LocalFileReadError => "local_file_read_error",
            Self::LocalDirReadError => "local_dir_read_error",
            Self::LocalChecksumError => "local_checksum_error",
            Self::GitMissingInImage => "git_missing_in_image",
            Self::GitCloneError => "git_clone_error",
            Self::GitSubpathError => "git_subpath_error",
            Self::GitCopyError => "git_copy_error",
            Self::MountMissingTool => "mount_missing_tool",
            Self::MountFailed => "mount_failed",
            Self::MountConfigInvalid => "mount_config_invalid",
            Self::SkillsConfigInvalid => "skills_config_invalid",
            Self::SandboxConfigInvalid => "sandbox_config_invalid",
            Self::SnapshotPersistError => "snapshot_persist_error",
            Self::SnapshotRestoreError => "snapshot_restore_error",
            Self::SnapshotNotRestorable => "snapshot_not_restorable",
        }
    }

    /// The coarse family this code belongs to.
    #[must_use]
    pub const fn category(self) -> ErrorCategory {
        match self {
            Self::InvalidManifestPath
            | Self::InvalidCompressionScheme
            | Self::ApplyPatchInvalidPath
            | Self::ApplyPatchInvalidDiff
            | Self::SkillsConfigInvalid
            | Self::SandboxConfigInvalid => ErrorCategory::Configuration,
            Self::ExposedPortUnavailable
            | Self::ExecNonzero
            | Self::ExecTimeout
            | Self::ExecTransportError
            | Self::PtySessionNotFound
            | Self::ApplyPatchFileNotFound
            | Self::ApplyPatchDecodeError
            | Self::WorkspaceReadNotFound
            | Self::WorkspaceArchiveReadError
            | Self::WorkspaceArchiveWriteError
            | Self::WorkspaceWriteTypeError
            | Self::WorkspaceStopError
            | Self::WorkspaceStartError
            | Self::WorkspaceRootNotFound => ErrorCategory::Runtime,
            Self::LocalFileReadError
            | Self::LocalDirReadError
            | Self::LocalChecksumError
            | Self::GitMissingInImage
            | Self::GitCloneError
            | Self::GitSubpathError
            | Self::GitCopyError
            | Self::MountMissingTool
            | Self::MountFailed
            | Self::MountConfigInvalid => ErrorCategory::Artifact,
            Self::SnapshotPersistError
            | Self::SnapshotRestoreError
            | Self::SnapshotNotRestorable => ErrorCategory::Snapshot,
        }
    }

    /// Whether retrying the same operation is expected to help, when the raiser said nothing.
    ///
    /// `None` means the failure cannot be classified from the code alone, which is a third answer
    /// and not a synonym for "no". A caller that treats unknown as non-retryable turns every
    /// transient clone failure into a permanent one; a caller that treats it as retryable spins on
    /// failures that will never clear. The raiser overrides this whenever it knows better.
    #[must_use]
    pub const fn default_retryable(self) -> Option<bool> {
        match self {
            // Deterministic: the same inputs fail the same way however many times they are tried.
            Self::InvalidManifestPath
            | Self::InvalidCompressionScheme
            | Self::ExecNonzero
            | Self::ExecTimeout
            | Self::PtySessionNotFound
            | Self::ApplyPatchInvalidPath
            | Self::ApplyPatchInvalidDiff
            | Self::ApplyPatchFileNotFound
            | Self::ApplyPatchDecodeError
            | Self::WorkspaceReadNotFound
            | Self::WorkspaceWriteTypeError
            | Self::WorkspaceRootNotFound
            | Self::LocalFileReadError
            | Self::LocalDirReadError
            | Self::LocalChecksumError
            | Self::GitMissingInImage
            | Self::GitSubpathError
            | Self::MountMissingTool
            | Self::MountConfigInvalid
            // A failing mount command is classified non-retryable rather than unknown: the
            // reference fixes this one where the neighbouring artifact failures stay open.
            | Self::MountFailed
            | Self::SkillsConfigInvalid
            | Self::SandboxConfigInvalid
            | Self::SnapshotNotRestorable => Some(false),
            // Broad enough to cover both a transient fault and a permanent one. The raiser is the
            // only party that can tell them apart, so an unqualified instance stays unclassified.
            Self::ExposedPortUnavailable
            | Self::ExecTransportError
            | Self::WorkspaceArchiveReadError
            | Self::WorkspaceArchiveWriteError
            | Self::WorkspaceStopError
            | Self::WorkspaceStartError
            | Self::GitCloneError
            | Self::GitCopyError
            | Self::SnapshotPersistError
            | Self::SnapshotRestoreError => None,
        }
    }

    /// Whether this is a failure to move bytes into or out of the workspace.
    ///
    /// The reference's `WorkspaceIOError` family, which sits inside the runtime category rather than
    /// beside it: a stop or start failure is a runtime failure too, but not one of these. Callers
    /// that record what an operation failed with distinguish the two.
    #[must_use]
    pub const fn is_workspace_io(self) -> bool {
        matches!(
            self,
            Self::ApplyPatchFileNotFound
                | Self::ApplyPatchDecodeError
                | Self::WorkspaceReadNotFound
                | Self::WorkspaceArchiveReadError
                | Self::WorkspaceArchiveWriteError
                | Self::WorkspaceWriteTypeError
        )
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// The coarse family a failure belongs to.
///
/// Derived from [`ErrorCode`] rather than stored, so a code cannot be filed under two families or
/// under none.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCategory {
    /// Caller-supplied configuration or input was rejected.
    Configuration,
    /// The sandbox itself failed: IO, transport, or the backend.
    Runtime,
    /// An input artifact could not be materialized.
    Artifact,
    /// A snapshot could not be persisted or restored.
    Snapshot,
}

/// The sandbox operation an error happened during.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpName {
    /// Session start.
    Start,
    /// Session stop, which persists rather than tears down.
    Stop,
    /// Running a command.
    Exec,
    /// Reading from the workspace.
    Read,
    /// Writing to the workspace.
    Write,
    /// Tearing the backend down.
    Shutdown,
    /// Asking whether the session is alive.
    Running,
    /// Streaming the workspace out.
    PersistWorkspace,
    /// Streaming a workspace in.
    HydrateWorkspace,
    /// Resolving a forwarded port.
    ResolveExposedPort,
    /// Materializing manifest entries.
    Materialize,
    /// Persisting a snapshot.
    SnapshotPersist,
    /// Restoring a snapshot.
    SnapshotRestore,
    /// Applying a patch.
    ApplyPatch,
}

impl OpName {
    /// The operation's wire string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Stop => "stop",
            Self::Exec => "exec",
            Self::Read => "read",
            Self::Write => "write",
            Self::Shutdown => "shutdown",
            Self::Running => "running",
            Self::PersistWorkspace => "persist_workspace",
            Self::HydrateWorkspace => "hydrate_workspace",
            Self::ResolveExposedPort => "resolve_exposed_port",
            Self::Materialize => "materialize",
            Self::SnapshotPersist => "snapshot_persist",
            Self::SnapshotRestore => "snapshot_restore",
            Self::ApplyPatch => "apply_patch",
        }
    }
}

impl fmt::Display for OpName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A structured sandbox failure.
///
/// Carries what a host needs in order to decide what to do: which failure it is, what was being
/// attempted, whether trying again is expected to help, and enough metadata to say why.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct SandboxError {
    message: String,
    error_code: ErrorCode,
    op: OpName,
    context: ErrorContext,
    #[source]
    cause: Option<Box<dyn std::error::Error + Send + Sync>>,
    retryable: Option<bool>,
    // Boxed to keep the error small. Every sandbox operation returns this in a `Result`, so the
    // whole call graph pays for its size on the success path too, and the payload is carried by a
    // handful of failures rather than by most of them.
    details: Option<Box<SandboxErrorDetails>>,
    redaction: Redaction,
}

/// Whether a failure's data may leave a mount boundary as it is.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Redaction {
    #[default]
    None,
    /// Its context or cause may hold credential-derived data.
    Redacted,
    /// As `Redacted`, but its message was written without any.
    RedactedSafeMessage,
}

impl SandboxError {
    /// Raises a failure, taking the code's documented retryability.
    #[must_use]
    pub fn new(error_code: ErrorCode, op: OpName, message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            error_code,
            op,
            context: ErrorContext::new(),
            cause: None,
            retryable: error_code.default_retryable(),
            details: None,
            redaction: Redaction::None,
        }
    }

    /// Marks the failure as carrying data that must not cross a mount lifecycle boundary as it is.
    ///
    /// The reference's `_mark_error_data_redacted`. Nothing is removed here; the boundary a failure
    /// crosses on its way out of a mount operation replaces a marked one with a copy that keeps
    /// only its code, operation and retryability.
    #[must_use]
    pub fn with_data_redacted(mut self) -> Self {
        if self.redaction == Redaction::None {
            self.redaction = Redaction::Redacted;
        }
        self
    }

    /// Marks the failure as [`Self::with_data_redacted`] does, and its message as safe to keep.
    ///
    /// For a validation failure whose message names only fields, never their values: the
    /// replacement a boundary makes keeps the message rather than a generic one.
    #[must_use]
    pub const fn with_safe_redacted_message(mut self) -> Self {
        self.redaction = Redaction::RedactedSafeMessage;
        self
    }

    /// Whether the failure was marked by [`Self::with_data_redacted`] or
    /// [`Self::with_safe_redacted_message`].
    #[must_use]
    pub const fn is_data_redacted(&self) -> bool {
        !matches!(self.redaction, Redaction::None)
    }

    /// Whether the failure's message was marked safe to keep through a replacement.
    #[must_use]
    pub const fn has_safe_redacted_message(&self) -> bool {
        matches!(self.redaction, Redaction::RedactedSafeMessage)
    }

    /// Overrides the code's documented retryability with what the raiser knows.
    #[must_use]
    pub const fn with_retryable(mut self, retryable: Option<bool>) -> Self {
        self.retryable = retryable;
        self
    }

    /// Attaches one piece of structured metadata.
    #[must_use]
    pub fn with_context(
        mut self,
        key: impl Into<String>,
        value: impl Into<serde_json::Value>,
    ) -> Self {
        self.context.insert(key.into(), value.into());
        self
    }

    /// Attaches the failure this one wraps.
    ///
    /// **A wrapping error inherits an unclassified retryability from what it wraps.** A stop that
    /// failed because an archive read failed is exactly as retryable as that read, and the outer
    /// layer is not in a position to know better. An outer error that was given its own answer
    /// keeps it — the inheritance fills a gap, it does not overwrite a decision.
    #[must_use]
    pub fn with_sandbox_cause(mut self, cause: Self) -> Self {
        self.cause = Some(Box::new(cause));
        self
    }

    /// Attaches an underlying failure that is not itself a sandbox error.
    ///
    /// Sandbox causes inherit retryability through the same path as `with_sandbox_cause`;
    /// other error types carry no classification to inherit.
    #[must_use]
    pub fn with_cause(mut self, cause: impl std::error::Error + Send + Sync + 'static) -> Self {
        self.cause = Some(Box::new(cause));
        self
    }

    /// The failure's stable identity.
    #[must_use]
    pub const fn error_code(&self) -> ErrorCode {
        self.error_code
    }

    /// The coarse family this failure belongs to.
    #[must_use]
    pub const fn category(&self) -> ErrorCategory {
        self.error_code.category()
    }

    /// What was being attempted.
    #[must_use]
    pub const fn op(&self) -> OpName {
        self.op
    }

    /// The human-readable message.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    /// The structured metadata attached to this failure.
    #[must_use]
    pub const fn context(&self) -> &ErrorContext {
        &self.context
    }

    /// Whether retrying is expected to help, or `None` when that cannot be said.
    #[must_use]
    pub fn retryable(&self) -> Option<bool> {
        self.retryable
            .or_else(|| self.cause.as_ref()?.downcast_ref::<Self>()?.retryable())
    }

    /// Typed failure data, preserved independently of diagnostic context overrides.
    #[must_use]
    pub fn details(&self) -> Option<&SandboxErrorDetails> {
        self.details.as_deref()
    }
}
