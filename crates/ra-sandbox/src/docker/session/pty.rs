//! Interactive processes inside the container.
//!
//! A port of the reference Docker session's `pty_exec_start`, `pty_write_stdin` and
//! `pty_terminate_all`. A command is set up with its three streams attached, on a terminal when one
//! is asked for, and started through an attached exec; one task copies its output into a buffer and
//! another asks the daemon every 50 ms whether it has exited. The command is registered under a
//! random id and the caller gets what it printed up to a deadline; a later write sends it input,
//! waits again, and returns what it printed since. Once a call sees an exit code, the process is
//! forgotten and the next call naming it is told it does not exist.
//!
//! # Ending a process inside a container
//!
//! The daemon has no call that signals an exec, so the command is started behind a wrapper that
//! writes the wrapper's own pid to a file and then `exec`s the command, which keeps that pid. Ending
//! the process reads the file and sends `SIGKILL` to that pid. The file sits in the session's
//! staging directory; when the command runs as another account, the file is created for that account
//! first, since it has to be able to write it.
//!
//! # Terminal or not
//!
//! With `tty` the command's output arrives as the terminal delivers it, one stream, and input written
//! to it reaches the terminal. Without `tty` its two output streams arrive demultiplexed and are
//! appended in the order the frames come, and its input is closed as soon as it starts: a write with
//! input is refused, and a write without input only waits.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use futures::StreamExt;
use ra_core::sandbox::{
    ExecRequest, PTY_PROCESSES_MAX, PTY_PROCESSES_WARNING, PtyExecUpdate, PtyProcessId,
    PtyProcessMeta, PtyStartRequest, PtyWriteRequest, SandboxError, SandboxResult,
    allocate_pty_process_id, clamp_pty_yield_time_ms, process_id_to_prune_from_meta,
    resolve_pty_write_yield_time_ms,
};
use tokio::io::AsyncWriteExt;
use tokio::sync::{Mutex as AsyncMutex, mpsc, oneshot};
use tokio::task::JoinHandle;

use super::{ArchiveDirection, DockerSandboxSession};
use crate::docker::api::{DockerApi, DockerApiError, ExecCreateRequest, ExecInput, ExecRunRequest};
use crate::pty_output::{PtyOutputBuffer, collect_pty_output, seconds_to_millis};
use crate::remote;

/// Starts the command after writing the shell's pid to `$2`, which `exec` hands to the command.
const PTY_PID_WRAPPER_SCRIPT: &str =
    r#"mkdir -p "$1" && printf "%s" "$$" > "$2" && shift 2 && exec "$@""#;

/// Creates the pid file for an account other than the one the session writes as: the staging
/// directory made traversable, the file empty, owned by that account and private to it. The
/// reference's `_PREPARE_USER_PTY_PID_SCRIPT`, byte for byte.
const PREPARE_USER_PTY_PID_SCRIPT: &str = concat!(
    "pid_path=\"$1\"\n",
    "pid_user=\"$2\"\n",
    "pid_parent=\"$(dirname \"$pid_path\")\"\n",
    "mkdir -p \"$pid_parent\" && ",
    "chmod 0711 \"$pid_parent\" && ",
    ": > \"$pid_path\" && ",
    "chown \"$pid_user\" \"$pid_path\" && ",
    "chmod 0600 \"$pid_path\"\n",
);

/// Sends `SIGKILL` to the pid a pid file names, if it names one, and never fails.
const KILL_PTY_PID_SCRIPT: &str = concat!(
    r#"if [ -f "$1" ]; then "#,
    r#"pid="$(cat "$1" 2>/dev/null || true)"; "#,
    r#"if [ -n "$pid" ]; then "#,
    r#"kill -KILL "$pid" >/dev/null 2>&1 || true; "#,
    "fi; ",
    "fi",
);

/// How long a start waits for output when the caller does not say.
const DEFAULT_START_YIELD_TIME_MS: u64 = 10_000;
/// How long a write waits for output when the caller does not say.
const DEFAULT_WRITE_YIELD_TIME_MS: u64 = 250;
/// How long a write lets the process react before it starts collecting output.
const WRITE_SETTLE: Duration = Duration::from_millis(100);
/// How often the exit watcher asks the daemon whether the process is still running.
const EXIT_POLL_INTERVAL: Duration = Duration::from_millis(50);
/// How long ending a process waits for its output reader to stop.
const READER_STOP_GRACE: Duration = Duration::from_secs(1);

/// The interactive processes one session started.
#[derive(Default)]
pub(super) struct DockerPtyProcesses {
    table: AsyncMutex<PtyTable>,
}

/// The registered processes, and every id currently spoken for.
#[derive(Default)]
struct PtyTable {
    processes: HashMap<PtyProcessId, Arc<DockerPtyEntry>>,
    reserved: HashSet<PtyProcessId>,
}

/// One interactive process.
struct DockerPtyEntry {
    exec_id: String,
    /// The file the wrapper wrote the process's pid to.
    pid_path: String,
    tty: bool,
    last_used: Mutex<Instant>,
    output: PtyOutputBuffer,
    /// Set once the output stream has ended: nothing more will arrive.
    output_closed: AtomicBool,
    /// Set once the daemon has reported an exit status.
    exit_code: Mutex<Option<i32>>,
    /// Requests to the task owning stdin, so termination never needs a blocked writer's lock.
    input: mpsc::UnboundedSender<InputRequest>,
    /// The task holding the input half of the attachment, abortable independently of callers.
    writer: Mutex<Option<JoinHandle<()>>>,
    /// The task copying output into the buffer; it owns the output half of the attachment.
    reader: Mutex<Option<JoinHandle<()>>>,
    /// The task asking the daemon whether the process has exited.
    watcher: Mutex<Option<JoinHandle<()>>>,
}

impl DockerPtyEntry {
    fn new(exec_id: String, pid_path: String, tty: bool, input: ExecInput) -> Self {
        let (sender, receiver) = mpsc::unbounded_channel();
        let writer = tokio::spawn(pump_pty_input(input, receiver));
        Self {
            exec_id,
            pid_path,
            tty,
            last_used: Mutex::new(Instant::now()),
            output: PtyOutputBuffer::new(),
            output_closed: AtomicBool::new(false),
            exit_code: Mutex::new(None),
            input: sender,
            writer: Mutex::new(Some(writer)),
            reader: Mutex::new(None),
            watcher: Mutex::new(None),
        }
    }

    fn exit_code(&self) -> Option<i32> {
        *self
            .exit_code
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn record_exit(&self, code: i64) {
        *self
            .exit_code
            .lock()
            .unwrap_or_else(PoisonError::into_inner) =
            Some(i32::try_from(code).unwrap_or(i32::MAX));
    }

    fn output_closed(&self) -> bool {
        self.output_closed.load(Ordering::SeqCst)
    }

    fn close_output(&self) {
        self.output_closed.store(true, Ordering::SeqCst);
        self.output.wake();
    }

    fn last_used(&self) -> Instant {
        *self
            .last_used
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn touch(&self) {
        *self
            .last_used
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Instant::now();
    }

    fn take_task(slot: &Mutex<Option<JoinHandle<()>>>) -> Option<JoinHandle<()>> {
        slot.lock().unwrap_or_else(PoisonError::into_inner).take()
    }
}

/// Ends a process that was started but never registered, if the call starting it is dropped.
///
/// The reference ends such a process when the start is cancelled; here cancellation is the start's
/// future being dropped, and ending the process needs the daemon, so the end is spawned onto the
/// runtime the start was running on.
struct Unregistered {
    session: DockerSandboxSession,
    entry: Option<Arc<DockerPtyEntry>>,
}

impl Drop for Unregistered {
    fn drop(&mut self) {
        let Some(entry) = self.entry.take() else {
            return;
        };
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let session = self.session.clone();
            runtime.spawn(async move { session.terminate_pty_entry(entry).await });
        }
    }
}

/// How a start that did not get as far as a running process failed.
enum StartFailure {
    /// A daemon call outlived the start's timeout.
    TimedOut,
    /// The daemon refused or could not be reached.
    Daemon(DockerApiError),
    /// The pid file could not be prepared.
    Session(SandboxError),
}

impl From<DockerApiError> for StartFailure {
    fn from(error: DockerApiError) -> Self {
        Self::Daemon(error)
    }
}

/// Runs one daemon call within the start's timeout, which each call gets in full, as in the
/// reference's two separate `wait_for`s.
async fn within<T>(
    timeout_s: Option<f64>,
    call: impl Future<Output = Result<T, DockerApiError>>,
) -> Result<T, StartFailure> {
    match timeout_s {
        None => Ok(call.await?),
        Some(timeout_s) => {
            let limit = Duration::try_from_secs_f64(timeout_s).unwrap_or(Duration::ZERO);
            match tokio::time::timeout(limit, call).await {
                Ok(outcome) => Ok(outcome?),
                Err(_) => Err(StartFailure::TimedOut),
            }
        }
    }
}

impl DockerSandboxSession {
    /// Starts a command with its streams attached, registers it, and waits for its first output.
    ///
    /// The command is shaped as a one-shot command is, the account handed to the daemon rather than
    /// to `sudo`, and it runs in the workspace root once that exists. `timeout_s` bounds each of the
    /// two daemon calls that start it, not the process.
    pub(super) async fn pty_exec_start(
        &self,
        request: PtyStartRequest,
    ) -> SandboxResult<PtyExecUpdate> {
        let original = request.command().to_vec();
        let user = request.user().map(|user| user.name.clone());
        let command = remote::prepare_exec_command(
            &ExecRequest::new(request.command().iter().cloned())
                .with_shell(request.shell().clone()),
        );
        self.recover_workspace_root_ready(request.timeout_s()).await;
        let workdir = self
            .workspace_root_ready
            .load(Ordering::SeqCst)
            .then(|| self.manifest().root);

        let pid_path = Self::archive_stage_path("pty.pid");
        let mut unregistered = Unregistered {
            session: self.clone(),
            entry: None,
        };
        let started = self
            .start_attached(
                command,
                workdir,
                user,
                request.tty(),
                request.timeout_s(),
                &pid_path,
            )
            .await;
        let entry = match started {
            Ok(entry) => entry,
            Err(StartFailure::TimedOut) => {
                self.kill_pty_pid_path(&pid_path).await;
                return Err(SandboxError::exec_timeout(original, request.timeout_s()));
            }
            Err(failure) => {
                // Every other failure is a transport error the caller may retry, with what went
                // wrong as its cause, as the reference wraps any exception here.
                let error =
                    SandboxError::exec_transport(original, None).with_context("retry_safe", true);
                return Err(match failure {
                    StartFailure::Daemon(cause) => error.with_cause(cause),
                    StartFailure::Session(cause) => error.with_sandbox_cause(cause),
                    StartFailure::TimedOut => error,
                });
            }
        };
        unregistered.entry = Some(Arc::clone(&entry));
        if !request.tty() {
            // Unlike the reference's synchronous half-close, this can yield. The acquired process
            // must already be guarded so cancellation ends it before it has a public process id.
            let (reply, done) = oneshot::channel();
            if entry.input.send(InputRequest::Shutdown(reply)).is_ok() {
                let _ = done.await;
            }
        }

        let (process_id, pruned, process_count) = {
            let mut table = self.pty.table.lock().await;
            let process_id = allocate_pty_process_id(&table.reserved);
            table.reserved.insert(process_id);
            let pruned = prune_if_needed(&mut table);
            table.processes.insert(process_id, Arc::clone(&entry));
            (process_id, pruned, table.processes.len())
        };
        unregistered.entry = None;

        if let Some(pruned) = pruned {
            self.terminate_pty_entry(pruned).await;
        }
        if process_count >= PTY_PROCESSES_WARNING {
            tracing::warn!(
                "PTY process count reached warning threshold: {process_count} active sessions"
            );
        }

        let wait_ms = seconds_to_millis(request.yield_time_s(), DEFAULT_START_YIELD_TIME_MS);
        let (output, original_token_count) = collect_pty_output(
            &entry.output,
            || entry.output_closed(),
            clamp_pty_yield_time_ms(wait_ms),
            request.max_output_tokens(),
        )
        .await;
        Ok(self
            .finalize_pty_update(process_id, &entry, output, original_token_count)
            .await)
    }

    /// Prepares the pid file, sets the command up, starts it, and begins reading and watching it.
    async fn start_attached(
        &self,
        command: Vec<String>,
        workdir: Option<String>,
        user: Option<String>,
        tty: bool,
        timeout_s: Option<f64>,
        pid_path: &str,
    ) -> Result<Arc<DockerPtyEntry>, StartFailure> {
        self.prepare_user_pty_pid_path(pid_path, user.as_deref())
            .await
            .map_err(StartFailure::Session)?;
        let pid_parent = pid_path
            .rsplit_once('/')
            .map_or("/", |(parent, _)| parent)
            .to_owned();
        let mut wrapped = vec![
            "sh".to_owned(),
            "-lc".to_owned(),
            PTY_PID_WRAPPER_SCRIPT.to_owned(),
            "sh".to_owned(),
            pid_parent,
            pid_path.to_owned(),
        ];
        wrapped.extend(command);

        let container_id = self.container_id();
        let create = ExecCreateRequest::new(wrapped)
            .with_stdin(true)
            .with_tty(tty)
            .in_dir(workdir)
            .as_user(user);
        let exec_id = within(timeout_s, self.api.exec_create(&container_id, &create)).await?;
        let attachment = within(timeout_s, self.api.exec_start(&exec_id, tty)).await?;
        let (mut output, input) = attachment.into_parts();

        let entry = Arc::new(DockerPtyEntry::new(
            exec_id,
            pid_path.to_owned(),
            tty,
            input,
        ));
        let reading = Arc::clone(&entry);
        let reader = tokio::spawn(async move {
            // Which stream a frame came from is not kept: the reference appends both as they come.
            while let Some(Ok(frame)) = output.next().await {
                reading.output.push(frame.data().to_vec());
            }
            reading.close_output();
        });
        *entry.reader.lock().unwrap_or_else(PoisonError::into_inner) = Some(reader);
        let watcher = tokio::spawn(watch_pty_exit(Arc::clone(&self.api), Arc::clone(&entry)));
        *entry.watcher.lock().unwrap_or_else(PoisonError::into_inner) = Some(watcher);
        Ok(entry)
    }

    /// Sends input to a registered process, or only waits when there is none, and collects output.
    pub(super) async fn pty_write_stdin(
        &self,
        request: PtyWriteRequest,
    ) -> SandboxResult<PtyExecUpdate> {
        let process_id = request.process_id();
        let entry = self
            .pty
            .table
            .lock()
            .await
            .processes
            .get(&process_id)
            .cloned()
            .ok_or_else(|| SandboxError::pty_session_not_found(process_id.get()))?;

        if !request.chars().is_empty() {
            if !entry.tty {
                return Err(SandboxError::pty_stdin_unavailable(process_id.get()));
            }
            write_to_process(&entry, process_id, request.chars().as_bytes()).await?;
            tokio::time::sleep(WRITE_SETTLE).await;
        }

        let wait_ms = seconds_to_millis(request.yield_time_s(), DEFAULT_WRITE_YIELD_TIME_MS);
        let (output, original_token_count) = collect_pty_output(
            &entry.output,
            || entry.output_closed(),
            resolve_pty_write_yield_time_ms(wait_ms, request.chars().is_empty()),
            request.max_output_tokens(),
        )
        .await;
        entry.touch();
        Ok(self
            .finalize_pty_update(process_id, &entry, output, original_token_count)
            .await)
    }

    /// Ends every registered process and forgets every id.
    pub(super) async fn pty_terminate_all_processes(&self) {
        let entries: Vec<Arc<DockerPtyEntry>> = {
            let mut table = self.pty.table.lock().await;
            table.reserved.clear();
            table.processes.drain().map(|(_, entry)| entry).collect()
        };
        for entry in entries {
            self.terminate_pty_entry(entry).await;
        }
    }

    /// Reports what a call collected, forgetting the process if it has exited.
    ///
    /// Output that has ended without an exit status yet gets one more look at the daemon, so a
    /// process that finished between two polls is reported finished now.
    async fn finalize_pty_update(
        &self,
        process_id: PtyProcessId,
        entry: &DockerPtyEntry,
        output: Vec<u8>,
        original_token_count: Option<u64>,
    ) -> PtyExecUpdate {
        if entry.output_closed() && entry.exit_code().is_none() {
            refresh_exit_code(self.api.as_ref(), entry).await;
        }

        let exit_code = entry.exit_code();
        let mut live_process_id = Some(process_id);
        if exit_code.is_some() {
            let removed = {
                let mut table = self.pty.table.lock().await;
                table.reserved.remove(&process_id);
                table.processes.remove(&process_id)
            };
            if let Some(removed) = removed {
                self.terminate_pty_entry(removed).await;
            }
            live_process_id = None;
        }
        PtyExecUpdate {
            process_id: live_process_id,
            output,
            exit_code,
            original_token_count,
        }
    }

    /// Ends one process and releases what it held.
    ///
    /// A process with no exit status is killed through its pid file; either way the pid file is
    /// removed. Then the attachment is closed — its input dropped and its reader stopped — and the
    /// watcher is let go.
    async fn terminate_pty_entry(&self, entry: Arc<DockerPtyEntry>) {
        let watcher = DockerPtyEntry::take_task(&entry.watcher);
        if let Some(watcher) = &watcher {
            watcher.abort();
        }

        refresh_exit_code(self.api.as_ref(), &entry).await;
        if entry.exit_code().is_none() {
            self.kill_pty_pid_path(&entry.pid_path).await;
        } else {
            self.rm_best_effort(&entry.pid_path).await;
        }

        if let Some(writer) = DockerPtyEntry::take_task(&entry.writer) {
            writer.abort();
            let _ = writer.await;
        }
        if let Some(reader) = DockerPtyEntry::take_task(&entry.reader) {
            reader.abort();
            let _ = tokio::time::timeout(READER_STOP_GRACE, reader).await;
        }
        entry.close_output();

        if let Some(watcher) = watcher {
            // Aborted just above; awaiting only lets it finish unwinding.
            let _ = watcher.await;
        }
    }

    /// Kills whatever pid a pid file names, then removes the file; says nothing if either fails.
    ///
    /// Run as the container's default account and without a working directory, as the reference's
    /// direct `exec_run` is.
    async fn kill_pty_pid_path(&self, pid_path: &str) {
        let _ = self
            .api
            .exec_run(
                &self.container_id(),
                &ExecRunRequest::new(vec![
                    "sh".to_owned(),
                    "-lc".to_owned(),
                    KILL_PTY_PID_SCRIPT.to_owned(),
                    "sh".to_owned(),
                    pid_path.to_owned(),
                ]),
            )
            .await;
        self.rm_best_effort(pid_path).await;
    }

    /// Creates the pid file for `user`, when the command runs as someone else.
    async fn prepare_user_pty_pid_path(
        &self,
        pid_path: &str,
        user: Option<&str>,
    ) -> SandboxResult<()> {
        let Some(user) = user else {
            return Ok(());
        };
        self.exec_checked(
            vec![
                "sh".to_owned(),
                "-lc".to_owned(),
                PREPARE_USER_PTY_PID_SCRIPT.to_owned(),
                "sh".to_owned(),
                pid_path.to_owned(),
                user.to_owned(),
            ],
            ArchiveDirection::Write,
            pid_path,
        )
        .await
        .map(|_| ())
    }
}

/// Asks the daemon every [`EXIT_POLL_INTERVAL`] until the process is no longer running, records its
/// exit status if the daemon has one, and wakes whoever is waiting for output.
///
/// A failed question ends the watch without a status, as in the reference.
async fn watch_pty_exit(api: Arc<dyn DockerApi>, entry: Arc<DockerPtyEntry>) {
    loop {
        let Ok(inspect) = api.exec_inspect(&entry.exec_id).await else {
            break;
        };
        if !inspect.running() {
            if let Some(code) = inspect.exit_code() {
                entry.record_exit(code);
            }
            break;
        }
        tokio::time::sleep(EXIT_POLL_INTERVAL).await;
    }
    entry.output.wake();
}

/// Records the exit status once, if the daemon says the process has one.
async fn refresh_exit_code(api: &dyn DockerApi, entry: &DockerPtyEntry) {
    if entry.exit_code().is_some() {
        return;
    }
    let Ok(inspect) = api.exec_inspect(&entry.exec_id).await else {
        return;
    };
    if inspect.running() {
        return;
    }
    if let Some(code) = inspect.exit_code() {
        entry.record_exit(code);
    }
}

/// Ends the least useful process when the table is full, and returns it to be terminated.
fn prune_if_needed(table: &mut PtyTable) -> Option<Arc<DockerPtyEntry>> {
    if table.processes.len() < PTY_PROCESSES_MAX {
        return None;
    }
    let meta: Vec<PtyProcessMeta<Instant>> = table
        .processes
        .iter()
        .map(|(process_id, entry)| {
            PtyProcessMeta::new(*process_id, entry.last_used(), entry.exit_code().is_some())
        })
        .collect();
    let process_id = process_id_to_prune_from_meta(&meta)?;
    table.reserved.remove(&process_id);
    table.processes.remove(&process_id)
}

/// Operations serialized by the task that owns the attached input.
enum InputRequest {
    Write(Vec<u8>, oneshot::Sender<std::io::Result<()>>),
    Shutdown(oneshot::Sender<std::io::Result<()>>),
}

/// Owns stdin independently of the callers awaiting writes. As in Codex's PTY I/O tasks,
/// terminating the task drops the connection even if a write or flush never becomes ready.
async fn pump_pty_input(mut input: ExecInput, mut requests: mpsc::UnboundedReceiver<InputRequest>) {
    while let Some(request) = requests.recv().await {
        match request {
            InputRequest::Write(bytes, reply) => {
                let outcome = match input.write_all(&bytes).await {
                    Ok(()) => input.flush().await,
                    Err(error) => Err(error),
                };
                let _ = reply.send(outcome);
            }
            InputRequest::Shutdown(reply) => {
                let _ = reply.send(input.shutdown().await);
            }
        }
    }
}

/// Writes input to a process's terminal.
///
/// A write that fails because the connection is going away is not an error — the process is ending,
/// and the call that collects output next will say so. The reference ignores the same failures of
/// its `sendall`: a broken pipe, a closed descriptor, a reset connection. An input already closed by
/// an ending is the closed-descriptor case.
async fn write_to_process(
    entry: &DockerPtyEntry,
    process_id: PtyProcessId,
    bytes: &[u8],
) -> SandboxResult<()> {
    let (reply, done) = oneshot::channel();
    if entry
        .input
        .send(InputRequest::Write(bytes.to_vec(), reply))
        .is_err()
    {
        return Ok(());
    }
    // Aborting the input task closes the reply channel, just as closing the reference socket
    // makes an in-flight send fail with a closed connection.
    let written = done
        .await
        .unwrap_or_else(|_| Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe)));
    match written {
        Ok(()) => Ok(()),
        Err(error) if connection_is_going_away(&error) => Ok(()),
        Err(error) => Err(SandboxError::exec_transport(Vec::new(), None)
            .with_context("session_id", process_id.get())
            .with_context("os_error", error.to_string())
            .with_cause(error)),
    }
}

/// Whether a write failed because the other end of the connection is gone.
fn connection_is_going_away(error: &std::io::Error) -> bool {
    use std::io::ErrorKind;

    #[cfg(unix)]
    let closed_descriptor = error.raw_os_error() == Some(rustix::io::Errno::BADF.raw_os_error());
    #[cfg(not(unix))]
    let closed_descriptor = false;

    closed_descriptor
        || matches!(
            error.kind(),
            ErrorKind::BrokenPipe | ErrorKind::ConnectionReset
        )
}
