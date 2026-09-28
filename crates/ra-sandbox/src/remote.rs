//! File access for a session whose filesystem is reachable only through commands run inside it.
//!
//! A container's workspace is not a directory on this machine. Whether a path escapes the workspace
//! through a symlink can only be answered by resolving it where it lives, so every file operation
//! first runs a resolver script inside the sandbox, and listing, creating and removing are commands
//! too. This is the reference base session's remote half — `_validate_remote_path_access`,
//! `_raise_read_error_from_exec`, and its `ls`, `rm` and `mkdir` — shared by every backend of that
//! shape rather than restated in each.
//!
//! # The resolved path decides containment; the requested path is what gets used
//!
//! The resolver prints where a path really leads, and that answer is only used to refuse. The
//! operation itself still acts on the normalized path the caller asked for, so removing a symlink
//! that stays inside the workspace removes the link rather than what it names. The local backend
//! makes the opposite choice for a reason of its own; see its `normalize_path`.

use ra_core::sandbox::{
    AsUser, ErrorCode, ExecRequest, ExecResult, FileEntry, OpName, PosixPath, SandboxError,
    SandboxResult, SandboxSession, SessionPath, ShellInvocation,
};

use crate::listing::{try_parse_ls_la, unreadable_listing};
use crate::runtime_helpers::{ensure_installed, resolve_workspace_path_helper};
use crate::session_scripts::{READ_PATH_PROBE_SCRIPT, READ_PATH_PROBE_TIMEOUT_S, diagnostic_text};
use crate::shell;

/// The resolver's exit status for a path that escapes every allowed root.
const RESOLVE_EXIT_ESCAPE: i32 = 111;
/// The resolver's exit status for a grant that resolves to the filesystem root.
const RESOLVE_EXIT_ROOT_GRANT: i32 = 113;
/// The resolver's exit status for a write under a read-only grant.
const RESOLVE_EXIT_READ_ONLY: i32 = 114;

/// Shapes a request into the argument vector the protocol runs.
///
/// The reference base session's `_prepare_exec_command`: a login shell in front unless the caller
/// asked for none or named its own, a single argument taken as a command line already and several
/// quoted back into one, and `sudo -u <user> --` in front of everything when an account is named.
/// A backend that can switch accounts itself shapes the request without the account and passes the
/// account to its runtime instead.
#[must_use]
pub fn prepare_exec_command(request: &ExecRequest) -> Vec<String> {
    let prefix: Option<Vec<String>> = match &request.shell {
        ShellInvocation::None => None,
        ShellInvocation::Login => Some(vec!["sh".to_owned(), "-lc".to_owned()]),
        // An empty prefix is falsy in the reference, which is no shell at all.
        ShellInvocation::Prefix(prefix) if prefix.is_empty() => None,
        ShellInvocation::Prefix(prefix) => Some(prefix.clone()),
    };

    let mut command = match prefix {
        None => request.command.clone(),
        Some(mut prefix) => {
            let joined = if request.command.len() == 1 {
                request.command[0].clone()
            } else {
                shell::join(request.command.iter().map(String::as_str))
            };
            prefix.push(joined);
            prefix
        }
    };

    if let Some(user) = &request.user {
        let mut elevated = vec![
            "sudo".to_owned(),
            "-u".to_owned(),
            user.name.clone(),
            "--".to_owned(),
        ];
        elevated.append(&mut command);
        command = elevated;
    }
    command
}

/// Validates a path against the sandbox's own filesystem before it is used.
///
/// Normalizes the path with the manifest's lexical policy, installs the resolver and runs it with
/// the workspace root, the normalized path and every grant, then maps what it said. On success the
/// normalized path is returned — not the resolved one — for the reason in the module comment.
///
/// # Errors
///
/// Returns the lexical policy's refusal first. Then, from the resolver: [`ErrorCode::InvalidManifestPath`]
/// for a path that resolves outside everything, with the resolved path in `resolved_path`;
/// [`ErrorCode::WorkspaceArchiveWriteError`] with reason `read_only_extra_path_grant` for a write
/// that resolves under a read-only grant; [`ErrorCode::SandboxConfigInvalid`] for a grant that
/// resolves to `/`; [`ErrorCode::ExecTransportError`] for a success that printed nothing; and
/// [`ErrorCode::ExecNonzero`] for any other failure, including one to install the resolver.
pub async fn validate_remote_path_access(
    session: &dyn SandboxSession,
    path: SessionPath<'_>,
    for_write: bool,
) -> SandboxResult<PosixPath> {
    let policy = session.workspace_path_policy()?;
    let root = policy.sandbox_root().as_str().to_owned();
    let workspace_path = policy.normalize_sandbox_path(path, for_write)?;
    let original_path = path.to_posix();
    let helper = resolve_workspace_path_helper();
    ensure_installed(session, &helper).await?;

    let grant_args: Vec<String> = policy
        .extra_path_grant_rules()
        .map_err(|error| {
            SandboxError::new(
                ErrorCode::SandboxConfigInvalid,
                OpName::Materialize,
                error.to_string(),
            )
        })?
        .into_iter()
        .flat_map(|(grant_root, read_only)| {
            [
                grant_root.as_str().to_owned(),
                if read_only { "1" } else { "0" }.to_owned(),
            ]
        })
        .collect();
    let write_flag = if for_write { "1" } else { "0" }.to_owned();
    let mut command = vec![
        helper.install_path().to_owned(),
        root.clone(),
        workspace_path.as_str().to_owned(),
        write_flag.clone(),
    ];
    command.extend(grant_args.iter().cloned());

    let result = session
        .exec(ExecRequest::new(command).with_shell(ShellInvocation::None))
        .await?;
    // What an error reports as the command: the helper's name rather than its installed path, as
    // the reference reports it.
    let reported_command = || {
        let mut reported = vec![
            "resolve_workspace_path".to_owned(),
            root.clone(),
            workspace_path.as_str().to_owned(),
            write_flag.clone(),
        ];
        reported.extend(grant_args.iter().cloned());
        reported
    };

    if result.ok() {
        let resolved = String::from_utf8_lossy(&result.stdout);
        if !resolved.trim().is_empty() {
            return Ok(workspace_path);
        }
        return Err(SandboxError::exec_transport(reported_command(), None)
            .with_context("reason", "empty_stdout")
            .with_context("exit_code", result.exit_code)
            .with_context("stdout", "")
            .with_context(
                "stderr",
                String::from_utf8_lossy(&result.stderr).into_owned(),
            ));
    }

    let stderr = String::from_utf8_lossy(&result.stderr).into_owned();
    match result.exit_code {
        RESOLVE_EXIT_ESCAPE => Err(invalid_manifest_path(&original_path)
            .with_context("resolved_path", stderr.trim().to_owned())),
        RESOLVE_EXIT_ROOT_GRANT => Err(SandboxError::new(
            ErrorCode::SandboxConfigInvalid,
            OpName::Materialize,
            stderr.trim().to_owned(),
        )),
        RESOLVE_EXIT_READ_ONLY => {
            let mut error = SandboxError::workspace_archive_write(workspace_path.as_str())
                .with_context("reason", "read_only_extra_path_grant");
            for line in stderr.lines() {
                if let Some(grant) = line.strip_prefix("read-only extra path grant: ") {
                    error = error.with_context("grant_path", grant.to_owned());
                } else if let Some(resolved) = line.strip_prefix("resolved path: ") {
                    error = error.with_context("resolved_path", resolved.to_owned());
                }
            }
            Err(error)
        }
        _ => Err(SandboxError::exec_nonzero(result, reported_command())),
    }
}

/// The refusal of a path that resolves outside the workspace, worded as the policy words it.
fn invalid_manifest_path(path: &PosixPath) -> SandboxError {
    let (reason, message) = if path.is_absolute() {
        (
            "absolute",
            format!("manifest path must be relative: {path}"),
        )
    } else {
        (
            "escape_root",
            format!("manifest path must not escape root: {path}"),
        )
    };
    SandboxError::new(ErrorCode::InvalidManifestPath, OpName::Materialize, message)
        .with_context("rel", path.as_str())
        .with_context("reason", reason)
}

/// Explains a failed read: missing, or present but not readable.
///
/// Only an exit of 1 from the read is a "no"; anything else is a failure to ask, reported as a read
/// failure without probing. After a "no", the existence probe runs as the same account, with no
/// shell and a ten-second limit, and decides which of the two it was.
///
/// The reference's `_raise_read_error_from_exec`. `command` is what the error reports as the read.
pub async fn read_error_from_exec(
    session: &dyn SandboxSession,
    path: &str,
    workspace_path: &str,
    command: Vec<String>,
    result: &ExecResult,
    user: AsUser,
) -> SandboxError {
    let context = |error: SandboxError| {
        error
            .with_context("command", command.clone())
            .with_context("stdout_bytes", result.stdout.len())
            .with_context("stderr", diagnostic_text(&result.stderr))
    };
    if result.exit_code != 1 {
        return context(SandboxError::workspace_archive_read(path));
    }

    let mut probe = ExecRequest::new([
        "sh".to_owned(),
        "-c".to_owned(),
        READ_PATH_PROBE_SCRIPT.to_owned(),
        "sh".to_owned(),
        workspace_path.to_owned(),
    ])
    .with_timeout_s(READ_PATH_PROBE_TIMEOUT_S)
    .with_shell(ShellInvocation::None);
    if let Some(user) = user {
        probe = probe.as_user(user);
    }
    let probe = match session.exec(probe).await {
        Ok(probe) => probe,
        Err(error) => {
            return context(SandboxError::workspace_archive_read(path)).with_sandbox_cause(error);
        }
    };
    let error = if probe.exit_code == 1 {
        SandboxError::workspace_read_not_found(path)
    } else {
        SandboxError::workspace_archive_read(path)
    };
    context(error)
        .with_context("existence_probe_exit_code", probe.exit_code)
        .with_context("existence_probe_stdout_bytes", probe.stdout.len())
        .with_context("existence_probe_stderr", diagnostic_text(&probe.stderr))
}

/// Lists a directory with `ls -la`, run inside the sandbox.
///
/// # Errors
///
/// Returns the path validation's refusal, or [`ErrorCode::ExecNonzero`] when `ls` fails.
pub async fn ls(
    session: &dyn SandboxSession,
    path: SessionPath<'_>,
    user: AsUser,
) -> SandboxResult<Vec<FileEntry>> {
    let path = String::from(session.validate_path_access(path, false).await?);
    let command = vec![
        "ls".to_owned(),
        "-la".to_owned(),
        "--".to_owned(),
        path.clone(),
    ];
    let result = session.exec(no_shell(command.clone(), user)).await?;
    if !result.ok() {
        return Err(SandboxError::exec_nonzero(result, command));
    }
    try_parse_ls_la(&String::from_utf8_lossy(&result.stdout), &path)
        .map_err(|error| unreadable_listing(error, &path))
}

/// Removes a path with `rm`, run inside the sandbox; `-rf` when recursive.
///
/// # Errors
///
/// Returns the path validation's refusal, or [`ErrorCode::ExecNonzero`] when `rm` fails.
pub async fn rm(
    session: &dyn SandboxSession,
    path: SessionPath<'_>,
    recursive: bool,
    user: AsUser,
) -> SandboxResult<()> {
    let path = String::from(session.validate_path_access(path, true).await?);
    let mut command = vec!["rm".to_owned()];
    if recursive {
        command.push("-rf".to_owned());
    }
    command.push("--".to_owned());
    command.push(path);
    let result = session.exec(no_shell(command.clone(), user)).await?;
    if !result.ok() {
        return Err(SandboxError::exec_nonzero(result, command));
    }
    Ok(())
}

/// Creates a directory with `mkdir`, run inside the sandbox; `-p` when parents may be created.
///
/// # Errors
///
/// Returns the path validation's refusal, or [`ErrorCode::ExecNonzero`] when `mkdir` fails.
pub async fn mkdir(
    session: &dyn SandboxSession,
    path: SessionPath<'_>,
    parents: bool,
    user: AsUser,
) -> SandboxResult<()> {
    let path = String::from(session.validate_path_access(path, true).await?);
    let mut command = vec!["mkdir".to_owned()];
    if parents {
        command.push("-p".to_owned());
    }
    command.push(path);
    let result = session.exec(no_shell(command.clone(), user)).await?;
    if !result.ok() {
        return Err(SandboxError::exec_nonzero(result, command));
    }
    Ok(())
}

/// A request for an argument vector run as written, as the account named.
fn no_shell(command: Vec<String>, user: AsUser) -> ExecRequest {
    let request = ExecRequest::new(command).with_shell(ShellInvocation::None);
    match user {
        Some(user) => request.as_user(user),
        None => request,
    }
}
