//! Constructors preserve the reference error payloads independently of their rendered messages.

use super::{ErrorCode, OpName, SandboxError};
use crate::sandbox::ExecResult;

/// Failure-specific data that cannot be recovered from a diagnostic message.
#[derive(Debug, Clone, PartialEq)]
pub enum SandboxErrorDetails {
    /// A completed command and its original, possibly non-UTF-8 output.
    ExecNonZero {
        /// Argument vector supplied by the caller.
        command: Vec<String>,
        /// Original streams and exit status.
        result: ExecResult,
    },
    /// A command that exceeded its deadline.
    ExecTimeout {
        /// Argument vector supplied by the caller.
        command: Vec<String>,
        /// Requested timeout in seconds, if present.
        timeout_s: Option<f64>,
    },
    /// A command whose transport failed.
    ExecTransport {
        /// Argument vector supplied by the caller.
        command: Vec<String>,
    },
    /// A missing interactive process.
    PtySessionNotFound {
        /// Requested process identifier.
        session_id: i64,
    },
    /// Input sent to an interactive process that was started without a terminal.
    PtyStdinUnavailable {
        /// The process addressed.
        session_id: i64,
    },
}

/// What a session says when input is sent to a process that has no terminal to receive it.
///
/// The reference's wording, kept exactly: its shell tool recognises this failure by its text, and
/// a caller porting that recognition should find the same text here.
pub const PTY_STDIN_UNAVAILABLE_MESSAGE: &str = "stdin is not available for this process";

impl SandboxError {
    /// Constructs a non-zero exit error, retaining bytes before lossy message decoding.
    #[must_use]
    pub fn exec_nonzero(result: ExecResult, command: Vec<String>) -> Self {
        let stdout = String::from_utf8_lossy(&result.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&result.stderr).into_owned();
        let message = match (stdout.is_empty(), stderr.is_empty()) {
            (false, false) => format!("stdout: {stdout}\nstderr: {stderr}"),
            (false, true) => stdout.clone(),
            (true, false) => stderr.clone(),
            (true, true) => format!("command exited with code {}", result.exit_code),
        };
        let mut error = Self::new(ErrorCode::ExecNonzero, OpName::Exec, message)
            .with_command_context(&command)
            .with_context("exit_code", result.exit_code)
            .with_context("stdout", stdout)
            .with_context("stderr", stderr);
        error.details = Some(Box::new(SandboxErrorDetails::ExecNonZero {
            command,
            result,
        }));
        error
    }

    /// Constructs a timeout error with the original command and requested deadline.
    #[must_use]
    pub fn exec_timeout(command: Vec<String>, timeout_s: Option<f64>) -> Self {
        let mut error = Self::new(ErrorCode::ExecTimeout, OpName::Exec, "command timed out")
            .with_command_context(&command)
            .with_context("timeout_s", timeout_s);
        error.details = Some(Box::new(SandboxErrorDetails::ExecTimeout {
            command,
            timeout_s,
        }));
        error
    }

    /// Constructs a transport error, whose retryability is initially unknown.
    #[must_use]
    pub fn exec_transport(command: Vec<String>, message: Option<&str>) -> Self {
        let mut error = Self::new(
            ErrorCode::ExecTransportError,
            OpName::Exec,
            message
                .filter(|text| !text.is_empty())
                .unwrap_or("exec transport error"),
        )
        .with_command_context(&command);
        error.details = Some(Box::new(SandboxErrorDetails::ExecTransport { command }));
        error
    }

    /// Constructs an error for a missing interactive process.
    #[must_use]
    pub fn pty_session_not_found(session_id: i64) -> Self {
        let mut error = Self::new(
            ErrorCode::PtySessionNotFound,
            OpName::Exec,
            format!("PTY session not found: {session_id}"),
        )
        .with_context("session_id", session_id);
        error.details = Some(Box::new(SandboxErrorDetails::PtySessionNotFound {
            session_id,
        }));
        error
    }

    /// Constructs the refusal to write to a process that was started without a terminal.
    ///
    /// **Not a sandbox error in the reference**, which raises a bare runtime error outside its
    /// taxonomy. Here a session answers only in [`SandboxError`], so the failure travels as a
    /// non-retryable transport error — the process exists, but the channel to its input does not —
    /// and carries [`SandboxErrorDetails::PtyStdinUnavailable`], which is what a caller should
    /// match rather than the code.
    #[must_use]
    pub fn pty_stdin_unavailable(session_id: i64) -> Self {
        let mut error = Self::new(
            ErrorCode::ExecTransportError,
            OpName::Exec,
            PTY_STDIN_UNAVAILABLE_MESSAGE,
        )
        .with_context("session_id", session_id)
        .with_retryable(Some(false));
        error.details = Some(Box::new(SandboxErrorDetails::PtyStdinUnavailable {
            session_id,
        }));
        error
    }

    /// Constructs a port error; an unconfigured port is always non-retryable.
    #[must_use]
    pub fn exposed_port_unavailable(port: u16, exposed_ports: &[u16], reason: &str) -> Self {
        let message = if reason == "not_configured" {
            format!("port {port} is not configured for host exposure")
        } else {
            format!("port {port} could not be resolved for host exposure")
        };
        let mut error = Self::new(
            ErrorCode::ExposedPortUnavailable,
            OpName::ResolveExposedPort,
            message,
        )
        .with_context("port", port)
        .with_context("exposed_ports", exposed_ports.to_vec())
        .with_context("reason", reason);
        if reason == "not_configured" {
            error.retryable = Some(false);
        }
        error
    }

    fn with_command_context(self, command: &[String]) -> Self {
        self.with_context("command", command.to_vec())
            .with_context("command_str", command.join(" "))
    }

    /// Constructs a missing workspace file error.
    #[must_use]
    pub fn workspace_read_not_found(path: &str) -> Self {
        Self::new(
            ErrorCode::WorkspaceReadNotFound,
            OpName::Read,
            format!("file not found: {path}"),
        )
        .with_context("path", path)
    }

    /// Constructs an archive read failure without assuming whether it is transient.
    #[must_use]
    pub fn workspace_archive_read(path: &str) -> Self {
        Self::new(
            ErrorCode::WorkspaceArchiveReadError,
            OpName::Read,
            format!("failed to read archive for path: {path}"),
        )
        .with_context("path", path)
    }

    /// Constructs an archive write failure without assuming whether it is transient.
    #[must_use]
    pub fn workspace_archive_write(path: &str) -> Self {
        Self::new(
            ErrorCode::WorkspaceArchiveWriteError,
            OpName::Write,
            format!("failed to write archive for path: {path}"),
        )
        .with_context("path", path)
    }

    /// Constructs a workspace payload type failure.
    #[must_use]
    pub fn workspace_write_type(path: &str, actual_type: &str) -> Self {
        Self::new(
            ErrorCode::WorkspaceWriteTypeError,
            OpName::Write,
            "write() expects a binary file-like object",
        )
        .with_context("path", path)
        .with_context("actual_type", actual_type)
    }

    /// Constructs a session persistence failure.
    #[must_use]
    pub fn workspace_stop(path: &str) -> Self {
        Self::new(
            ErrorCode::WorkspaceStopError,
            OpName::Stop,
            "failed to stop session",
        )
        .with_context("path", path)
    }

    /// Constructs a failure to write a snapshot's bytes to its storage.
    #[must_use]
    pub fn snapshot_persist(snapshot_id: &str, path: &str) -> Self {
        Self::new(
            ErrorCode::SnapshotPersistError,
            OpName::SnapshotPersist,
            "failed to persist snapshot",
        )
        .with_context("snapshot_id", snapshot_id)
        .with_context("path", path)
    }

    /// Constructs a failure to read a snapshot's bytes back from its storage.
    #[must_use]
    pub fn snapshot_restore(snapshot_id: &str, path: &str) -> Self {
        Self::new(
            ErrorCode::SnapshotRestoreError,
            OpName::SnapshotRestore,
            "failed to restore snapshot",
        )
        .with_context("snapshot_id", snapshot_id)
        .with_context("path", path)
    }

    /// Constructs a refusal to restore from storage that holds nothing to restore.
    ///
    /// Filed under restore rather than a family of its own, as the reference files it: the caller
    /// asked to restore, and what it needs to know is that it cannot.
    #[must_use]
    pub fn snapshot_not_restorable(snapshot_id: &str, path: &str) -> Self {
        Self::new(
            ErrorCode::SnapshotNotRestorable,
            OpName::SnapshotRestore,
            "snapshot is not restorable",
        )
        .with_context("snapshot_id", snapshot_id)
        .with_context("path", path)
    }

    /// Constructs a workspace preparation failure.
    #[must_use]
    pub fn workspace_start(path: &str, message: Option<&str>) -> Self {
        Self::new(
            ErrorCode::WorkspaceStartError,
            OpName::Start,
            message
                .filter(|text| !text.is_empty())
                .unwrap_or("failed to start session"),
        )
        .with_context("path", path)
    }

    /// Constructs an invalid or incomplete mount configuration failure.
    ///
    /// Raised while a manifest is being checked, before anything is materialized, which is why
    /// the operation is `materialize` whatever the caller was doing when it asked.
    #[must_use]
    pub fn mount_config(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::MountConfigInvalid, OpName::Materialize, message)
            .with_retryable(Some(false))
    }

    /// Constructs an invalid skills configuration failure.
    ///
    /// The reference's `SkillsConfigError`: raised while the skills capability checks what it was
    /// given, edits a manifest, renders its index or loads a lazy skill, and always recorded against
    /// `materialize`, as the reference records it.
    #[must_use]
    pub fn skills_config(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::SkillsConfigInvalid, OpName::Materialize, message)
            .with_retryable(Some(false))
    }

    /// Constructs a failure for a mount whose tool is not installed in the sandbox.
    #[must_use]
    pub fn mount_tool_missing(tool: &str) -> Self {
        Self::new(
            ErrorCode::MountMissingTool,
            OpName::Materialize,
            format!("required mount tool missing: {tool}"),
        )
        .with_retryable(Some(false))
        .with_context("tool", tool)
    }

    /// Constructs a failure for a mount command that ran and did not succeed.
    ///
    /// `command` and `stderr` are recorded as given; a caller whose command line or output may
    /// carry credentials redacts them first.
    #[must_use]
    pub fn mount_command(command: &str, stderr: &str) -> Self {
        Self::new(
            ErrorCode::MountFailed,
            OpName::Materialize,
            "mount command failed",
        )
        .with_retryable(Some(false))
        .with_context("command", command)
        .with_context("stderr", stderr)
    }

    /// Constructs a refusal of a patch path.
    #[must_use]
    pub fn apply_patch_invalid_path(path: &str, reason: ApplyPatchPathReason) -> Self {
        let message = match reason {
            ApplyPatchPathReason::Absolute => format!("apply_patch path must be relative: {path}"),
            ApplyPatchPathReason::EscapeRoot => {
                format!("apply_patch path must not escape root: {path}")
            }
            ApplyPatchPathReason::Empty => "apply_patch path must be non-empty".to_owned(),
        };
        Self::new(
            ErrorCode::ApplyPatchInvalidPath,
            OpName::ApplyPatch,
            message,
        )
        .with_retryable(Some(false))
        .with_context("path", path)
        .with_context("reason", reason.as_str())
    }

    /// Constructs a failure for a diff that is malformed or does not apply.
    ///
    /// `path` is the file it was for, when there is one.
    #[must_use]
    pub fn apply_patch_invalid_diff(message: impl Into<String>, path: Option<&str>) -> Self {
        let error = Self::new(
            ErrorCode::ApplyPatchInvalidDiff,
            OpName::ApplyPatch,
            message,
        )
        .with_retryable(Some(false));
        match path {
            Some(path) => error.with_context("path", path),
            None => error,
        }
    }

    /// Constructs a failure for a patch that names a file that is not there.
    #[must_use]
    pub fn apply_patch_file_not_found(path: &str) -> Self {
        Self::new(
            ErrorCode::ApplyPatchFileNotFound,
            OpName::ApplyPatch,
            format!("apply_patch missing file: {path}"),
        )
        .with_retryable(Some(false))
        .with_context("path", path)
    }

    /// Constructs a failure for a patched file that is not UTF-8.
    #[must_use]
    pub fn apply_patch_decode_error(path: &str) -> Self {
        Self::new(
            ErrorCode::ApplyPatchDecodeError,
            OpName::ApplyPatch,
            format!("apply_patch could not decode file: {path}"),
        )
        .with_retryable(Some(false))
        .with_context("path", path)
    }

    /// Constructs the failure of an event sink that was configured to raise, during `op`.
    ///
    /// `message` is what the caller is told, which is the reference's own wording: its
    /// synchronous delivery reports `sandbox event sink failed: <sink> while handling event <id>`,
    /// while a sink awaited inline in `async` mode lets its own failure out as it is. What the sink
    /// failed with travels as the cause either way.
    #[must_use]
    pub fn event_sink_failed(op: OpName, message: impl Into<String>) -> Self {
        Self::new(ErrorCode::EventSinkFailed, op, message)
    }
}

/// Why a patch path was refused.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyPatchPathReason {
    /// It was absolute where a relative path was required.
    Absolute,
    /// It leaves the workspace.
    EscapeRoot,
    /// It was blank.
    Empty,
}

impl ApplyPatchPathReason {
    /// The reference's name for it, recorded in the failure's context.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Absolute => "absolute",
            Self::EscapeRoot => "escape_root",
            Self::Empty => "empty",
        }
    }
}
