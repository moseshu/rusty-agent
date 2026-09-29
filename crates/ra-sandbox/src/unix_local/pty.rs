//! Interactive processes on this host: a terminal when one is asked for, plain pipes otherwise.
//!
//! A port of the process table the reference's local session keeps for `pty_exec_start` and
//! `pty_write_stdin`. A process is started, registered under a random id, and waited on for output
//! up to a deadline; a later write sends it input, waits again, and returns what it printed since.
//! Once a call sees the process has exited, that call returns the exit code without an id and the
//! process is forgotten — so the next call naming it is told it does not exist.
//!
//! # Terminal or pipes
//!
//! With `tty` the process gets a pseudo-terminal as its standard streams, is made the leader of a
//! new session with that terminal as its controlling one, and has SIGINT and SIGQUIT restored to
//! their defaults — so Ctrl-C written to the terminal interrupts it even when this process ignores
//! the signal. Standard output and error arrive interleaved, as a terminal delivers them.
//!
//! Without `tty` the process gets pipes for output, keeps this process's standard input, as a
//! one-shot command does, and cannot be written to: a write with input is refused, and a write
//! without input only waits. Its two output streams are read separately and arrive in the order the
//! reads complete. It is started in its own process group rather than its own session — the local
//! backend's one-shot commands make the same substitution, because a new session can only be made
//! between fork and exec, and that step is not available here.
//!
//! # What the terminal crate is for
//!
//! Making a session leader and a controlling terminal has to happen in the child before it runs
//! anything, which in Rust means code that runs between fork and exec. This workspace forbids
//! `unsafe`, so that step is `portable-pty`'s — the crate codex's own terminal support uses — whose
//! child setup is the reference's `_preexec` plus a little more: it also resets SIGCHLD, SIGHUP,
//! SIGTERM and SIGALRM, clears the signal mask, and closes stray descriptors. One thing it does
//! that the reference does not is visible to a command: `SHELL` is always set in the environment.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{Read, Write};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use ra_core::sandbox::{
    PTY_PROCESSES_MAX, PTY_PROCESSES_WARNING, PtyExecUpdate, PtyProcessId, PtyProcessMeta,
    PtyWriteRequest, SandboxError, SandboxResult, allocate_pty_process_id, clamp_pty_yield_time_ms,
    process_id_to_prune_from_meta, resolve_pty_write_yield_time_ms,
};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::sync::oneshot;
use tokio::task::{AbortHandle, JoinHandle};

use crate::pty_output::{PtyOutputBuffer, collect_pty_output, seconds_to_millis};

use super::exec::{self, HostCommand};

/// How much one read takes from a terminal or a pipe.
const PTY_READ_CHUNK_BYTES: usize = 16_384;
/// How long stop and shutdown wait for terminals still being closed.
const PTY_FD_CLOSE_GRACE: Duration = Duration::from_millis(100);
/// How long a start waits for output when the caller does not say.
const DEFAULT_START_YIELD_TIME_MS: u64 = 10_000;
/// How long a write waits for output when the caller does not say.
const DEFAULT_WRITE_YIELD_TIME_MS: u64 = 250;
/// How long a write lets the process react before it starts collecting output.
const WRITE_SETTLE: Duration = Duration::from_millis(100);

/// The interactive processes one session started, and the terminals still being closed.
#[derive(Default)]
pub(crate) struct PtyProcesses {
    table: tokio::sync::Mutex<PtyTable>,
    /// One receiver per terminal handed to a closing thread; it completes when the close has.
    fd_closes: Mutex<Vec<oneshot::Receiver<()>>>,
}

/// The registered processes, and every id currently spoken for.
#[derive(Default)]
struct PtyTable {
    processes: HashMap<PtyProcessId, Arc<PtyEntry>>,
    reserved: HashSet<PtyProcessId>,
}

/// One interactive process.
struct PtyEntry {
    /// Whether it was given a terminal.
    tty: bool,
    /// Its process id, which is also its process group's.
    pid: Option<u32>,
    last_used: Mutex<Instant>,
    output: PtyOutputBuffer,
    /// Set once every reader has stopped: nothing more will arrive.
    output_closed: AtomicBool,
    /// Set once the process has been reaped.
    exit_code: Mutex<Option<i32>>,
    /// The terminal's controlling end, until the process is ended. Shared with the thread a write
    /// runs on, because a write to a terminal can block.
    terminal: Arc<Mutex<Option<Terminal>>>,
    /// The pipe readers.
    readers: Mutex<Vec<AbortHandle>>,
    /// The task waiting for a piped process to exit.
    waiter: Mutex<Option<JoinHandle<()>>>,
}

/// The controlling end of a terminal: the handle that keeps it open, and the one input goes to.
struct Terminal {
    _master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
}

impl PtyEntry {
    fn new(tty: bool, pid: Option<u32>, terminal: Option<Terminal>) -> Self {
        Self {
            tty,
            pid,
            last_used: Mutex::new(Instant::now()),
            output: PtyOutputBuffer::new(),
            output_closed: AtomicBool::new(false),
            exit_code: Mutex::new(None),
            terminal: Arc::new(Mutex::new(terminal)),
            readers: Mutex::new(Vec::new()),
            waiter: Mutex::new(None),
        }
    }

    fn exit_code(&self) -> Option<i32> {
        *self
            .exit_code
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn record_exit(&self, code: i32) {
        *self
            .exit_code
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(code);
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
}

/// Ends a process that was started but never registered, if the call starting it is dropped.
struct Unregistered(Option<Arc<PtyEntry>>);

impl Drop for Unregistered {
    fn drop(&mut self) {
        if let Some(entry) = self.0.take() {
            exec::kill_process_group(entry.pid);
        }
    }
}

impl PtyProcesses {
    /// Starts `host`, registers it, and waits for its first output.
    ///
    /// `command` is the request as prepared, before the host shaped it, and is what a failure to
    /// start names.
    pub(crate) async fn start(
        &self,
        command: &[String],
        host: HostCommand,
        env: &BTreeMap<String, String>,
        tty: bool,
        yield_time_s: Option<f64>,
        max_output_tokens: Option<u64>,
    ) -> SandboxResult<PtyExecUpdate> {
        let entry = if tty {
            spawn_terminal(command, host, env)?
        } else {
            spawn_piped(command, &host, env)?
        };
        let mut unregistered = Unregistered(Some(Arc::clone(&entry)));

        let (process_id, pruned, process_count) = {
            let mut table = self.table.lock().await;
            let process_id = allocate_pty_process_id(&table.reserved);
            table.reserved.insert(process_id);
            let pruned = prune_if_needed(&mut table);
            table.processes.insert(process_id, Arc::clone(&entry));
            (process_id, pruned, table.processes.len())
        };
        unregistered.0 = None;

        if let Some(pruned) = pruned {
            self.terminate(pruned).await;
        }
        if process_count >= PTY_PROCESSES_WARNING {
            tracing::warn!(
                "PTY process count reached warning threshold: {process_count} active sessions"
            );
        }

        let wait_ms = seconds_to_millis(yield_time_s, DEFAULT_START_YIELD_TIME_MS);
        let (output, original_token_count) = collect_pty_output(
            &entry.output,
            || entry.output_closed.load(Ordering::SeqCst),
            clamp_pty_yield_time_ms(wait_ms),
            max_output_tokens,
        )
        .await;
        Ok(self
            .finalize(process_id, &entry, output, original_token_count)
            .await)
    }

    /// Sends input to a registered process, or only waits when there is none, and collects output.
    pub(crate) async fn write(&self, request: PtyWriteRequest) -> SandboxResult<PtyExecUpdate> {
        let process_id = request.process_id();
        let entry = self
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
            write_to_terminal(&entry, process_id, request.chars().as_bytes().to_vec()).await?;
            tokio::time::sleep(WRITE_SETTLE).await;
        }

        let wait_ms = seconds_to_millis(request.yield_time_s(), DEFAULT_WRITE_YIELD_TIME_MS);
        let (output, original_token_count) = collect_pty_output(
            &entry.output,
            || entry.output_closed.load(Ordering::SeqCst),
            resolve_pty_write_yield_time_ms(wait_ms, request.chars().is_empty()),
            request.max_output_tokens(),
        )
        .await;
        entry.touch();
        Ok(self
            .finalize(process_id, &entry, output, original_token_count)
            .await)
    }

    /// Ends every registered process and forgets every id.
    pub(crate) async fn terminate_all(&self) {
        let entries: Vec<Arc<PtyEntry>> = {
            let mut table = self.table.lock().await;
            table.reserved.clear();
            table.processes.drain().map(|(_, entry)| entry).collect()
        };
        for entry in entries {
            self.terminate(entry).await;
        }
    }

    /// Waits a short grace period for terminals still being closed.
    ///
    /// Bounded on purpose, as the reference's is: closing a terminal has been seen to block on
    /// macOS while a reader is still inside a read, and stop must not wait on that indefinitely.
    /// Closes still running after the grace period stay tracked for the next call.
    pub(crate) async fn wait_for_fd_closes(&self) {
        let pending: Vec<oneshot::Receiver<()>> = std::mem::take(
            &mut *self
                .fd_closes
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        );
        if pending.is_empty() {
            return;
        }
        let deadline = tokio::time::Instant::now() + PTY_FD_CLOSE_GRACE;
        let mut unfinished = Vec::new();
        for mut close in pending {
            if tokio::time::timeout_at(deadline, &mut close).await.is_err() {
                unfinished.push(close);
            }
        }
        self.fd_closes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend(unfinished);
    }

    /// Reports what a call collected, forgetting the process if it has exited.
    async fn finalize(
        &self,
        process_id: PtyProcessId,
        entry: &PtyEntry,
        output: Vec<u8>,
        original_token_count: Option<u64>,
    ) -> PtyExecUpdate {
        let exit_code = entry.exit_code();
        let mut live_process_id = Some(process_id);
        if exit_code.is_some() {
            let removed = {
                let mut table = self.table.lock().await;
                table.reserved.remove(&process_id);
                table.processes.remove(&process_id)
            };
            if let Some(removed) = removed {
                self.terminate(removed).await;
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
    /// A process still running is killed with its whole group. A terminal's controlling end is
    /// closed on a thread of its own rather than here, and the call does not wait for it — see
    /// [`Self::wait_for_fd_closes`].
    async fn terminate(&self, entry: Arc<PtyEntry>) {
        if entry.exit_code().is_none() {
            exec::kill_process_group(entry.pid);
        }
        for reader in entry
            .readers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .drain(..)
        {
            reader.abort();
        }
        let waiter = entry
            .waiter
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(waiter) = &waiter {
            waiter.abort();
        }

        if entry.tty {
            let terminal = entry
                .terminal
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take();
            if let Some(terminal) = terminal {
                self.schedule_close(terminal);
            }
            entry.close_output();
            return;
        }

        if let Some(waiter) = waiter {
            // Aborted just above; awaiting only lets it finish unwinding.
            let _ = waiter.await;
        }
    }

    /// Closes a terminal on a thread of its own, and tracks when it has.
    fn schedule_close(&self, terminal: Terminal) {
        let (done, close) = oneshot::channel();
        let spawned = std::thread::Builder::new()
            .name("sandbox-pty-close".to_owned())
            .spawn(move || {
                drop(terminal);
                let _ = done.send(());
            });
        if spawned.is_ok() {
            self.fd_closes
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(close);
        }
    }
}

/// Ends the least useful process when the table is full, and returns it to be terminated.
fn prune_if_needed(table: &mut PtyTable) -> Option<Arc<PtyEntry>> {
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

/// Starts a command on a new terminal.
fn spawn_terminal(
    command: &[String],
    host: HostCommand,
    env: &BTreeMap<String, String>,
) -> SandboxResult<Arc<PtyEntry>> {
    let failure = |error: &dyn std::fmt::Display| {
        SandboxError::exec_transport(command.to_vec(), Some(&error.to_string()))
            .with_context("tty", true)
    };

    // No window size, as the reference's `os.openpty()` sets none.
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 0,
            cols: 0,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|error| failure(&error))?;

    let mut builder = CommandBuilder::from_argv(host.argv.into_iter().map(Into::into).collect());
    builder.env_clear();
    for (name, value) in env {
        builder.env(name, value);
    }
    builder.cwd(host.process_cwd);

    let child = pair
        .slave
        .spawn_command(builder)
        .map_err(|error| failure(&error))?;
    // The child holds the terminal now. Keeping this end open here would keep the terminal alive
    // after the child exits, and the reader would never see the end of its output.
    drop(pair.slave);

    // The exit status portable-pty reports keeps a signal's name and loses its number; the
    // standard library's keeps both, and a process Ctrl-C ended has to read as `-SIGINT`.
    let child: Box<dyn portable_pty::Child> = child;
    let child = match child.downcast::<std::process::Child>() {
        Ok(child) => child,
        Err(mut child) => {
            let _ = child.kill();
            return Err(failure(&"unexpected terminal child handle"));
        }
    };
    let pid = child.id();

    let handles = pair
        .master
        .try_clone_reader()
        .and_then(|reader| Ok((reader, pair.master.take_writer()?)));
    let (reader, writer) = match handles {
        Ok(handles) => handles,
        Err(error) => {
            exec::kill_process_group(Some(pid));
            return Err(failure(&error));
        }
    };

    let entry = Arc::new(PtyEntry::new(
        true,
        Some(pid),
        Some(Terminal {
            _master: pair.master,
            writer,
        }),
    ));

    let reading = Arc::clone(&entry);
    let reader_thread = std::thread::Builder::new()
        .name("sandbox-pty-read".to_owned())
        .spawn(move || read_terminal(reader, &reading.output));
    let reader_thread = match reader_thread {
        Ok(thread) => thread,
        Err(error) => {
            exec::kill_process_group(Some(pid));
            return Err(failure(&error));
        }
    };

    let waiting = Arc::clone(&entry);
    let mut child = *child;
    let waiter = std::thread::Builder::new()
        .name("sandbox-pty-wait".to_owned())
        .spawn(move || {
            let code = child.wait().map_or(-1, exec::exit_code);
            waiting.record_exit(code);
            let _ = reader_thread.join();
            waiting.close_output();
        });
    if let Err(error) = waiter {
        exec::kill_process_group(Some(pid));
        return Err(failure(&error));
    }

    Ok(entry)
}

/// Copies a terminal's output into the buffer until the terminal closes.
///
/// A terminal whose last user has gone reports an error rather than end of file on some systems,
/// so any error other than an interrupted read ends it as end of file does.
fn read_terminal(mut reader: Box<dyn Read + Send>, output: &PtyOutputBuffer) {
    let mut buffer = vec![0_u8; PTY_READ_CHUNK_BYTES];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => output.push(buffer[..read].to_vec()),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
}

/// Starts a command with pipes for its output.
fn spawn_piped(
    command: &[String],
    host: &HostCommand,
    env: &BTreeMap<String, String>,
) -> SandboxResult<Arc<PtyEntry>> {
    let Some((program, arguments)) = host.argv.split_first() else {
        return Err(SandboxError::exec_transport(
            command.to_vec(),
            Some("no command to run"),
        ));
    };
    let mut child = tokio::process::Command::new(program)
        .args(arguments)
        .current_dir(&host.process_cwd)
        .env_clear()
        .envs(env)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Its own group, so ending it reaches everything it started.
        .process_group(0)
        .spawn()
        .map_err(|error| exec::transport_failure(command, &error))?;

    let entry = Arc::new(PtyEntry::new(false, child.id(), None));
    let stdout = tokio::spawn(read_pipe(child.stdout.take(), Arc::clone(&entry)));
    let stderr = tokio::spawn(read_pipe(child.stderr.take(), Arc::clone(&entry)));
    entry
        .readers
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .extend([stdout.abort_handle(), stderr.abort_handle()]);

    let waiting = Arc::clone(&entry);
    let waiter = tokio::spawn(async move {
        let code = child.wait().await.map_or(-1, exec::exit_code);
        waiting.record_exit(code);
        let _ = stdout.await;
        let _ = stderr.await;
        waiting.close_output();
    });
    *entry.waiter.lock().unwrap_or_else(PoisonError::into_inner) = Some(waiter);
    Ok(entry)
}

/// Copies one pipe into the buffer until it closes.
async fn read_pipe(stream: Option<impl AsyncRead + Unpin>, entry: Arc<PtyEntry>) {
    let Some(mut stream) = stream else {
        return;
    };
    let mut buffer = vec![0_u8; PTY_READ_CHUNK_BYTES];
    loop {
        match stream.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(read) => entry.output.push(buffer[..read].to_vec()),
        }
    }
}

/// Writes input to a process's terminal.
///
/// **Every byte is written, where the reference makes one `os.write` and ignores how much of it
/// the terminal took.** A partial write there drops the rest of the input without a word; the
/// inputs it expects are short enough that one write takes them whole, and for those the two
/// behave the same. The write runs on a blocking thread, since a terminal whose process is not
/// reading can hold it.
///
/// A write that fails because the terminal is going away is not an error — the process is ending,
/// and the call that collects output next will say so.
async fn write_to_terminal(
    entry: &PtyEntry,
    process_id: PtyProcessId,
    bytes: Vec<u8>,
) -> SandboxResult<()> {
    let terminal = Arc::clone(&entry.terminal);
    let written = tokio::task::spawn_blocking(move || {
        let mut terminal = terminal.lock().unwrap_or_else(PoisonError::into_inner);
        terminal.as_mut().map(|terminal| {
            terminal
                .writer
                .write_all(&bytes)
                .and_then(|()| terminal.writer.flush())
        })
    })
    .await
    .map_err(|error| {
        SandboxError::exec_transport(Vec::new(), Some(&error.to_string()))
            .with_context("session_id", process_id.get())
    })?;

    match written {
        None => Err(SandboxError::pty_stdin_unavailable(process_id.get())),
        Some(Ok(())) => Ok(()),
        Some(Err(error)) if terminal_is_going_away(&error) => Ok(()),
        Some(Err(error)) => Err(
            SandboxError::exec_transport(Vec::new(), Some(&error.to_string()))
                .with_context("session_id", process_id.get())
                .with_context("os_error", error.to_string()),
        ),
    }
}

/// Whether a write failed because the other end of the terminal is gone.
fn terminal_is_going_away(error: &std::io::Error) -> bool {
    use rustix::io::Errno;

    error.raw_os_error().is_some_and(|code| {
        [Errno::IO, Errno::BADF, Errno::PIPE, Errno::CONNRESET]
            .iter()
            .any(|errno| errno.raw_os_error() == code)
    })
}
