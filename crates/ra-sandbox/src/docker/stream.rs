//! Feeding bytes to a command inside the container, framed by their length.
//!
//! Writes and workspace restores go into the container as the standard input of `cat` or `tar`.
//! The command cannot be told "that was all of it" by closing its input: over a daemon reached
//! through TLS the half-close of an attached exec is swallowed, and a `tar -x` waiting for the end
//! of its input waits forever. So the reference frames the payload by its length instead: the
//! command is run behind `head -c <n>`, which stops after exactly the bytes that were sent whatever
//! the transport does with the close.

use ra_core::sandbox::{SandboxError, SandboxResult};
use tokio::io::AsyncWriteExt;

use futures::StreamExt;

use super::api::{DockerApi, DockerApiError, ExecCreateRequest};

/// The POSIX shell that pipes exactly `$1` bytes of its input into the command after it.
///
/// A bare `head -c "$n" | "$@"` reports only the consumer's status, so a `head` that is missing, or
/// a POSIX-only one without `-c`, would leave `cat` an empty pipe and the write would "succeed" with
/// an empty file. The script therefore first checks that `head -c` produces the byte it should, and
/// exits 98 when it does not, turning that silent loss into an error. The check needs no writable
/// path — a predictable file in `/tmp` could be pre-seeded as a symlink by code in the container —
/// and no `pipefail`, which `dash` lacks. The reference's script, byte for byte.
pub const LENGTH_FRAMED_STDIN_SCRIPT: &str = r#"n=$1; shift; [ "$(printf ab | head -c 1 2>/dev/null)" = a ] || exit 98; head -c "$n" | "$@""#;

/// How much is written to the command's input at a time.
const WRITE_CHUNK_SIZE: usize = 1024 * 1024;

/// Runs `command` inside the container with `payload` as its standard input.
///
/// The command is wrapped in [`LENGTH_FRAMED_STDIN_SCRIPT`] with the payload's length, started with
/// input attached and no working directory, fed exactly that many bytes, and its input then shut
/// down — which a transport that carries half-closes needs and one that does not ignores. Output is
/// read to the end and discarded, and the exit status decides the outcome; a status the daemon does
/// not report is taken as success, as the reference takes it.
///
/// # Errors
///
/// Returns [`ra_core::sandbox::ErrorCode::WorkspaceArchiveWriteError`] for `error_path`: with the
/// daemon's failure as the cause when the command could not be run or fed, and with `command` and
/// `exit_code` in the context when it ran and failed.
pub(crate) async fn stream_into_exec(
    api: &dyn DockerApi,
    container_id: &str,
    command: &[String],
    payload: &[u8],
    error_path: &str,
    user: Option<&str>,
) -> SandboxResult<()> {
    let exit_code = run_framed(api, container_id, command, payload, user)
        .await
        .map_err(|error| SandboxError::workspace_archive_write(error_path).with_cause(error))?;
    match exit_code {
        None | Some(0) => Ok(()),
        Some(code) => Err(SandboxError::workspace_archive_write(error_path)
            .with_context("command", command.to_vec())
            .with_context("exit_code", code.to_string())),
    }
}

/// Creates, starts, feeds and drains the framed command, and returns its exit status.
async fn run_framed(
    api: &dyn DockerApi,
    container_id: &str,
    command: &[String],
    payload: &[u8],
    user: Option<&str>,
) -> Result<Option<i64>, DockerApiError> {
    let mut framed = vec![
        "sh".to_owned(),
        "-c".to_owned(),
        LENGTH_FRAMED_STDIN_SCRIPT.to_owned(),
        "sh".to_owned(),
        payload.len().to_string(),
    ];
    framed.extend_from_slice(command);
    let exec_id = api
        .exec_create(
            container_id,
            &ExecCreateRequest::new(framed)
                .with_stdin(true)
                .as_user(user.map(str::to_owned)),
        )
        .await?;
    let (mut output, mut input) = api.exec_start(&exec_id, false).await?.into_parts();
    for chunk in payload.chunks(WRITE_CHUNK_SIZE) {
        input
            .write_all(chunk)
            .await
            .map_err(|error| DockerApiError::transport(error.to_string()))?;
    }
    // Best effort: over a transport that swallows the half-close this does nothing, and the length
    // framing is what ends the command's input instead.
    let _ = input.shutdown().await;
    while let Some(frame) = output.next().await {
        if frame.is_err() {
            break;
        }
    }
    drop((output, input));
    Ok(api.exec_inspect(&exec_id).await?.exit_code())
}
