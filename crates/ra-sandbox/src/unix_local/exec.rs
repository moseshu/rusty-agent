//! Turning a request into an argument vector, and running it on this host.
//!
//! Four steps, and each one is a decision the reference made:
//!
//! 1. a shell is put in front, or not — and for this backend it is `sh -c`, not the protocol's
//!    `sh -lc`, so a local command does not pick up whatever a login profile sets;
//! 2. arguments that name something inside the workspace are rewritten relative to it, so a command
//!    line never carries the host path a provider happened to pick;
//! 3. a `sh -c` command is given a `cd` into the workspace and the process is started from `/`,
//!    which is what lets the macOS fence deny the tree the workspace lives in;
//! 4. the whole thing is wrapped in the host fence, where there is one.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use ra_core::sandbox::{ExecRequest, ExecResult, SandboxError, SandboxPathGrant, ShellInvocation};

use crate::host_paths::resolve_without_strictness;
use crate::shell;

use super::confine::confined_exec_command;

/// Shapes a caller's request into the argument vector that will be run.
///
/// The shell prefix is this backend's: the protocol's default is a login shell, and a local session
/// deliberately does not use one — a command run here would otherwise inherit the developer's own
/// profile, which is neither reproducible nor what the caller asked for.
///
/// Public because it is the answer to "what will this session actually run", which a host wants for
/// logging and approval, and because running it is the only other way to find out — and the answer
/// for a request naming another account cannot be found that way without a second account.
#[must_use]
pub fn prepare_exec_command(request: &ExecRequest) -> Vec<String> {
    let prefix: Option<Vec<String>> = match &request.shell {
        ShellInvocation::None => None,
        ShellInvocation::Login => Some(vec!["sh".to_owned(), "-c".to_owned()]),
        // An empty prefix is no shell at all, which is how the reference reads an empty list.
        ShellInvocation::Prefix(prefix) if prefix.is_empty() => None,
        ShellInvocation::Prefix(prefix) => Some(prefix.clone()),
    };

    let mut command = match prefix {
        None => request.command.clone(),
        Some(mut prefix) => {
            // A single argument is already a command line; several are quoted back into one.
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

/// Rewrites arguments that point inside the workspace so they are relative to it.
///
/// The program itself is never rewritten — it is resolved against `PATH`, not against the
/// workspace. Everything else that is an absolute path under the workspace root becomes the
/// relative form, and the root itself becomes `.`. Anything else is passed through: an absolute
/// path elsewhere is the caller's business, and a relative one is already relative.
pub(crate) fn workspace_relative_command_parts(
    command: &[String],
    workspace_root: &Path,
) -> Vec<String> {
    let Some((program, arguments)) = command.split_first() else {
        return Vec::new();
    };
    let mut rewritten = vec![program.clone()];
    for argument in arguments {
        let path = Path::new(argument);
        let rewrite = path
            .is_absolute()
            .then(|| path.strip_prefix(workspace_root).ok())
            .flatten();
        match rewrite {
            Some(relative) if relative.as_os_str().is_empty() => rewritten.push(".".to_owned()),
            Some(relative) => rewritten.push(relative.to_string_lossy().into_owned()),
            None => rewritten.push(argument.clone()),
        }
    }
    rewritten
}

/// Decides where the process starts, and whether the shell has to walk into the workspace itself.
///
/// A `sh -c` command is started from `/` with a `cd` prepended. The reason is the fence: the
/// workspace usually lives under a tree the profile denies, and a process whose working directory
/// is inside a denied tree cannot start at all. Moving the `cd` inside the script means the shell
/// arrives after the profile has already allowed the workspace back.
pub(crate) fn shell_workspace_process_context(
    command_parts: Vec<String>,
    workspace_root: &Path,
    cwd: &Path,
) -> (PathBuf, Vec<String>) {
    if command_parts.len() < 3 || command_parts[0] != "sh" || command_parts[1] != "-c" {
        return (cwd.to_owned(), command_parts);
    }
    let mut rewritten = command_parts;
    rewritten[2] = format!(
        "cd {} && {}",
        shell::quote(&workspace_root.to_string_lossy()),
        rewritten[2]
    );
    (PathBuf::from("/"), rewritten)
}

/// Runs a prepared command to completion.
///
/// `stdin` is the payload to feed the command, when there is one. Without it the command inherits
/// this process's standard input, which is what the reference does: a command that reads from a
/// terminal keeps reading from the terminal the host is attached to.
///
/// # Errors
///
/// Returns [`ra_core::sandbox::ErrorCode::ExecTimeout`] when the deadline passes — after the whole
/// process group has been killed, because killing only the process that was started leaves whatever
/// it started behind — and [`ra_core::sandbox::ErrorCode::ExecTransportError`] when the command
/// could not be started or its output could not be collected. A command that ran and exited
/// non-zero is a result, not an error.
pub(crate) async fn run(
    command: &[String],
    timeout_s: Option<f64>,
    env: &BTreeMap<String, String>,
    cwd: &Path,
    extra_path_grants: &[SandboxPathGrant],
    stdin: Option<Vec<u8>>,
) -> Result<ExecResult, SandboxError> {
    let workspace_root = resolve_without_strictness(cwd).map_err(|error| {
        SandboxError::exec_transport(command.to_vec(), Some(&error.to_string()))
            .with_sandbox_cause(error)
    })?;
    let parts = workspace_relative_command_parts(command, &workspace_root);
    let (process_cwd, parts) = shell_workspace_process_context(parts, &workspace_root, cwd);
    let exec_command = confined_exec_command(parts, &workspace_root, env, extra_path_grants)?;

    let Some((program, arguments)) = exec_command.split_first() else {
        return Err(SandboxError::exec_transport(
            command.to_vec(),
            Some("no command to run"),
        ));
    };

    let mut process = tokio::process::Command::new(program);
    process
        .args(arguments)
        .current_dir(&process_cwd)
        .env_clear()
        .envs(env)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        // Its own group, so a timeout can reach everything the command started rather than only the
        // command itself.
        .process_group(0);
    if stdin.is_some() {
        process.stdin(Stdio::piped());
    }

    let mut child = process
        .spawn()
        .map_err(|error| transport_failure(command, &error))?;
    let pid = child.id();
    let mut group_guard = ProcessGroupGuard(pid);
    let handle = child.stdin.take();
    let feed = async move {
        if let (Some(bytes), Some(mut handle)) = (stdin, handle) {
            use tokio::io::AsyncWriteExt;

            // Best effort: a command that exits before reading its input closes the pipe, and that
            // is the command's answer rather than a failure to deliver.
            let _ = handle.write_all(&bytes).await;
            let _ = handle.shutdown().await;
        }
    };
    let finish = async move {
        let ((), output) = tokio::join!(feed, child.wait_with_output());
        output
    };

    let output = match timeout_s {
        Some(seconds) => {
            match tokio::time::timeout(Duration::from_secs_f64(seconds.max(0.0)), finish).await {
                Ok(output) => output,
                Err(_elapsed) => {
                    return Err(SandboxError::exec_timeout(command.to_vec(), timeout_s));
                }
            }
        }
        None => finish.await,
    };
    let output = output.map_err(|error| transport_failure(command, &error))?;
    group_guard.0 = None;

    Ok(ExecResult::new(
        output.stdout,
        output.stderr,
        exit_code(output.status),
    ))
}

/// Reports a command that never produced a result.
fn transport_failure(command: &[String], error: &std::io::Error) -> SandboxError {
    SandboxError::exec_transport(command.to_vec(), Some(&error.to_string()))
        .with_context("os_error", error.to_string())
}

/// The status a caller sees, including for a command a signal ended.
///
/// A signalled command reports the negated signal number, which is what the reference's process
/// object carries and what a caller reading "was this interrupted" looks for. Without it a killed
/// command would be indistinguishable from one that exited cleanly.
fn exit_code(status: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;

    status
        .code()
        .unwrap_or_else(|| status.signal().map_or(0, |signal| -signal))
}

/// Stops the whole command tree when its future is dropped, before checkout cleanup can run.
struct ProcessGroupGuard(Option<u32>);

impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        kill_process_group(self.0);
    }
}

/// Kills everything the command started, by group.
fn kill_process_group(pid: Option<u32>) {
    use rustix::process::{Pid, Signal, kill_process_group};

    let group = pid
        .and_then(|pid| i32::try_from(pid).ok())
        .and_then(Pid::from_raw);
    if let Some(group) = group {
        // Best effort by design: the group may already be gone, and a failure here must not replace
        // the timeout the caller is about to be told about.
        let _ = kill_process_group(group, Signal::KILL);
    }
}
