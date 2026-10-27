// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
#![allow(
    clippy::needless_continue,
    reason = "Preserve source interrupted-I/O retry loops."
)]
#![allow(
    clippy::used_underscore_binding,
    reason = "The source uses underscore-prefixed owners to retain process cleanup guards."
)]
use core::fmt;
use std::io;
#[cfg(unix)]
use std::os::fd::RawFd;
use std::process::ExitStatus;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::AtomicBool;

use anyhow::anyhow;
use portable_pty::MasterPty;
use portable_pty::PtySize;
use portable_pty::SlavePty;
use tokio::sync::broadcast;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::sync::watch;
use tokio::task::AbortHandle;
use tokio::task::JoinHandle;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(
    dead_code,
    reason = "Retain this source process API for the unported execution backend."
)]
pub(crate) enum ProcessSignal {
    Interrupt,
}

#[allow(
    dead_code,
    reason = "Retain this source process API for the unported execution backend."
)]
pub(crate) fn unsupported_signal(signal: ProcessSignal) -> io::Error {
    match signal {
        ProcessSignal::Interrupt => io::Error::new(
            io::ErrorKind::Unsupported,
            "process interrupt is not supported by this process backend",
        ),
    }
}

pub(crate) fn exit_code_from_status(status: ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }

    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return 128 + signal;
        }
    }

    -1
}

pub(crate) trait ChildTerminator: Send + Sync {
    #[allow(
        dead_code,
        reason = "Retain this source process API for the unported execution backend."
    )]
    fn signal(&mut self, signal: ProcessSignal) -> io::Result<()>;

    fn kill(&mut self) -> io::Result<()>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TerminalSize {
    pub(crate) rows: u16,
    pub(crate) cols: u16,
}

impl Default for TerminalSize {
    fn default() -> Self {
        Self { rows: 24, cols: 80 }
    }
}

impl From<TerminalSize> for PtySize {
    fn from(value: TerminalSize) -> Self {
        Self {
            rows: value.rows,
            cols: value.cols,
            pixel_width: 0,
            pixel_height: 0,
        }
    }
}

#[cfg(unix)]
pub(crate) trait PtyHandleKeepAlive: Send {}

#[cfg(unix)]
impl<T: Send + ?Sized> PtyHandleKeepAlive for T {}

#[allow(
    dead_code,
    reason = "Retain this source process API for the unported execution backend."
)]
pub(crate) enum PtyMasterHandle {
    #[allow(
        dead_code,
        reason = "Retain this source process API for the unported execution backend."
    )]
    Resizable(Box<dyn MasterPty + Send>),
    #[cfg(unix)]
    Opaque {
        raw_fd: RawFd,
        _handle: Box<dyn PtyHandleKeepAlive>,
    },
}

pub(crate) struct PtyHandles {
    pub(crate) _slave: Option<Box<dyn SlavePty + Send>>,
    pub(crate) _master: PtyMasterHandle,
}

impl fmt::Debug for PtyHandles {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PtyHandles").finish()
    }
}

/// Callback used by driver-backed sessions to resize a PTY-like backend when
/// there is no local `PtyHandles` instance to resize directly.
type ResizeFn = Box<dyn FnMut(TerminalSize) -> anyhow::Result<()> + Send>;

/// Handle for driving an interactive process (PTY or pipe).
pub(crate) struct ProcessHandle {
    #[allow(
        dead_code,
        reason = "Retain this source process API for the unported execution backend."
    )]
    writer_tx: StdMutex<Option<mpsc::Sender<Vec<u8>>>>,
    killer: StdMutex<Option<Box<dyn ChildTerminator>>>,
    reader_handle: StdMutex<Option<JoinHandle<()>>>,
    reader_abort_handles: StdMutex<Vec<AbortHandle>>,
    writer_handle: StdMutex<Option<JoinHandle<()>>>,
    wait_handle: StdMutex<Option<JoinHandle<()>>>,
    #[allow(
        dead_code,
        reason = "Retain this source process API for the unported execution backend."
    )]
    exit_status: Arc<AtomicBool>,
    #[allow(
        dead_code,
        reason = "Retain this source process API for the unported execution backend."
    )]
    exit_code: Arc<StdMutex<Option<i32>>>,
    // PtyHandles must be preserved because the process will receive Control+C if the
    // slave is closed
    _pty_handles: StdMutex<Option<PtyHandles>>,
    // Optional resize hook for driver-backed sessions that proxy PTY control to
    // another backend instead of owning local PTY handles.
    #[allow(
        dead_code,
        reason = "Retain this source process API for the unported execution backend."
    )]
    resizer: StdMutex<Option<ResizeFn>>,
}

impl fmt::Debug for ProcessHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProcessHandle").finish()
    }
}

impl ProcessHandle {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        writer_tx: mpsc::Sender<Vec<u8>>,
        killer: Box<dyn ChildTerminator>,
        reader_handle: JoinHandle<()>,
        reader_abort_handles: Vec<AbortHandle>,
        writer_handle: JoinHandle<()>,
        wait_handle: JoinHandle<()>,
        exit_status: Arc<AtomicBool>,
        exit_code: Arc<StdMutex<Option<i32>>>,
        pty_handles: Option<PtyHandles>,
        resizer: Option<ResizeFn>,
    ) -> Self {
        Self {
            writer_tx: StdMutex::new(Some(writer_tx)),
            killer: StdMutex::new(Some(killer)),
            reader_handle: StdMutex::new(Some(reader_handle)),
            reader_abort_handles: StdMutex::new(reader_abort_handles),
            writer_handle: StdMutex::new(Some(writer_handle)),
            wait_handle: StdMutex::new(Some(wait_handle)),
            exit_status,
            exit_code,
            _pty_handles: StdMutex::new(pty_handles),
            resizer: StdMutex::new(resizer),
        }
    }

    /// Returns a channel sender for writing raw bytes to the child stdin.
    #[allow(
        dead_code,
        reason = "Retain this source process API for the unported execution backend."
    )]
    pub(crate) fn writer_sender(&self) -> mpsc::Sender<Vec<u8>> {
        if let Ok(writer_tx) = self.writer_tx.lock()
            && let Some(writer_tx) = writer_tx.as_ref()
        {
            return writer_tx.clone();
        }

        let (writer_tx, writer_rx) = mpsc::channel(1);
        drop(writer_rx);
        writer_tx
    }

    /// True if the child process has exited.
    #[allow(
        dead_code,
        reason = "Retain this source process API for the unported execution backend."
    )]
    pub(crate) fn has_exited(&self) -> bool {
        self.exit_status.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Returns the exit code if known.
    #[allow(
        dead_code,
        reason = "Retain this source process API for the unported execution backend."
    )]
    pub(crate) fn exit_code(&self) -> Option<i32> {
        self.exit_code.lock().ok().and_then(|guard| *guard)
    }

    /// Resize the PTY in character cells.
    #[allow(
        dead_code,
        reason = "Retain this source process API for the unported execution backend."
    )]
    pub(crate) fn resize(&self, size: TerminalSize) -> anyhow::Result<()> {
        {
            let handles = self
                ._pty_handles
                .lock()
                .map_err(|_| anyhow!("failed to lock PTY handles"))?;
            if let Some(handles) = handles.as_ref() {
                return match &handles._master {
                    PtyMasterHandle::Resizable(master) => master.resize(size.into()),
                    #[cfg(unix)]
                    PtyMasterHandle::Opaque { raw_fd, .. } => resize_raw_pty(*raw_fd, size),
                };
            }
        }

        let mut resizer = self
            .resizer
            .lock()
            .map_err(|_| anyhow!("failed to lock PTY resizer"))?;
        if let Some(resizer) = resizer.as_mut() {
            resizer(size)
        } else {
            Err(anyhow!("process is not attached to a PTY"))
        }
    }

    /// Close the child's stdin channel.
    #[allow(
        dead_code,
        reason = "Retain this source process API for the unported execution backend."
    )]
    pub(crate) fn close_stdin(&self) {
        if let Ok(mut writer_tx) = self.writer_tx.lock() {
            writer_tx.take();
        }
    }

    /// Attempts to kill the child while leaving the reader/writer tasks alive
    /// so callers can still drain output until EOF.
    pub(crate) fn request_terminate(&self) {
        if let Ok(mut killer_opt) = self.killer.lock()
            && let Some(mut killer) = killer_opt.take()
        {
            let _ = killer.kill();
        }
    }

    #[allow(
        dead_code,
        reason = "Retain this source process API for the unported execution backend."
    )]
    pub(crate) fn signal(&self, signal: ProcessSignal) -> io::Result<()> {
        let Ok(mut killer_opt) = self.killer.lock() else {
            return Ok(());
        };
        let Some(killer) = killer_opt.as_mut() else {
            return Ok(());
        };

        let result = killer.signal(signal);
        #[cfg(windows)]
        if result.is_ok() {
            killer_opt.take();
        }
        result
    }

    /// Attempts to kill the child and abort I/O helper tasks.
    pub(crate) fn terminate(&self) {
        self.request_terminate();

        if let Ok(mut h) = self.reader_handle.lock()
            && let Some(handle) = h.take()
        {
            handle.abort();
        }
        if let Ok(mut handles) = self.reader_abort_handles.lock() {
            for handle in handles.drain(..) {
                handle.abort();
            }
        }
        if let Ok(mut h) = self.writer_handle.lock()
            && let Some(handle) = h.take()
        {
            handle.abort();
        }
        if let Ok(mut h) = self.wait_handle.lock()
            && let Some(handle) = h.take()
        {
            // Even a queued PTY waiter must run to reap the terminated child.
            drop(handle);
        }
    }
}

impl Drop for ProcessHandle {
    fn drop(&mut self) {
        self.terminate();
    }
}

/// Adapts a closure into a `ChildTerminator` implementation.
#[allow(
    dead_code,
    reason = "Retain this source process API for the unported execution backend."
)]
struct ClosureTerminator {
    inner: Option<Box<dyn FnMut() + Send + Sync>>,
    #[cfg(windows)]
    interrupt_terminates: bool,
}

impl ChildTerminator for ClosureTerminator {
    fn signal(&mut self, signal: ProcessSignal) -> io::Result<()> {
        #[cfg(windows)]
        if self.interrupt_terminates {
            return self.kill();
        }

        Err(unsupported_signal(signal))
    }

    fn kill(&mut self) -> io::Result<()> {
        if let Some(inner) = self.inner.as_mut() {
            (inner)();
        }
        Ok(())
    }
}

#[cfg(unix)]
#[allow(
    dead_code,
    reason = "Retain this source process API for the unported execution backend."
)]
fn resize_raw_pty(raw_fd: RawFd, size: TerminalSize) -> anyhow::Result<()> {
    let mut winsize = libc::winsize {
        ws_row: size.rows,
        ws_col: size.cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let result = unsafe { libc::ioctl(raw_fd, libc::TIOCSWINSZ, &mut winsize) };
    if result == -1 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

/// Combine split stdout/stderr receivers into a single broadcast receiver.
#[allow(
    dead_code,
    reason = "Retain this source process API for the unported execution backend."
)]
pub(crate) fn combine_output_receivers(
    mut stdout_rx: mpsc::Receiver<Vec<u8>>,
    mut stderr_rx: mpsc::Receiver<Vec<u8>>,
) -> broadcast::Receiver<Vec<u8>> {
    let (combined_tx, combined_rx) = broadcast::channel(256);
    tokio::spawn(async move {
        let mut stdout_open = true;
        let mut stderr_open = true;

        loop {
            tokio::select! {
                stdout = stdout_rx.recv(), if stdout_open => match stdout {
                    Some(chunk) => {
                        let _ = combined_tx.send(chunk);
                    }
                    None => {
                        stdout_open = false;
                    }
                },
                stderr = stderr_rx.recv(), if stderr_open => match stderr {
                    Some(chunk) => {
                        let _ = combined_tx.send(chunk);
                    }
                    None => {
                        stderr_open = false;
                    }
                },
                else => break,
            }
        }
    });
    combined_rx
}

/// Return value from PTY or pipe spawn helpers.
#[derive(Debug)]
pub(crate) struct SpawnedProcess {
    #[allow(
        dead_code,
        reason = "Retain this source process API for the unported execution backend."
    )]
    pub(crate) session: ProcessHandle,
    #[allow(
        dead_code,
        reason = "Retain this source process API for the unported execution backend."
    )]
    pub(crate) stdout_rx: mpsc::Receiver<Vec<u8>>,
    #[allow(
        dead_code,
        reason = "Retain this source process API for the unported execution backend."
    )]
    pub(crate) stderr_rx: mpsc::Receiver<Vec<u8>>,
    #[allow(
        dead_code,
        reason = "Retain this source process API for the unported execution backend."
    )]
    pub(crate) exit_rx: oneshot::Receiver<i32>,
}

/// Driver-backed process handles for non-standard spawn backends.
#[allow(
    dead_code,
    reason = "Retain this source process API for the unported execution backend."
)]
pub(crate) struct ProcessDriver {
    pub(crate) writer_tx: mpsc::Sender<Vec<u8>>,
    pub(crate) stdout_rx: broadcast::Receiver<Vec<u8>>,
    pub(crate) stderr_rx: Option<broadcast::Receiver<Vec<u8>>>,
    pub(crate) exit_rx: oneshot::Receiver<i32>,
    pub(crate) terminator: Option<Box<dyn FnMut() + Send + Sync>>,
    pub(crate) writer_handle: Option<JoinHandle<()>>,
    pub(crate) resizer: Option<ResizeFn>,
    /// Whether this Windows process is attached to a pseudo-console.
    #[cfg(windows)]
    pub(crate) tty: bool,
}

/// Build a `SpawnedProcess` from a driver that supplies stdin/output/exit channels.
#[allow(
    dead_code,
    reason = "Retain this source process API for the unported execution backend."
)]
pub(crate) fn spawn_from_driver(driver: ProcessDriver) -> SpawnedProcess {
    let ProcessDriver {
        writer_tx,
        stdout_rx: stdout_driver_rx,
        stderr_rx: mut stderr_driver_rx,
        exit_rx,
        terminator,
        writer_handle,
        resizer,
        #[cfg(windows)]
        tty,
    } = driver;

    #[cfg(windows)]
    let interrupt_terminates = terminator.is_some() && !tty;

    let (stdout_tx, stdout_rx) = mpsc::channel::<Vec<u8>>(256);
    let (stderr_tx, stderr_rx) = mpsc::channel::<Vec<u8>>(256);
    let (exit_seen_tx, exit_seen_rx) = watch::channel(false);
    let spawn_stream_reader =
        |mut output_rx: broadcast::Receiver<Vec<u8>>,
         output_tx: mpsc::Sender<Vec<u8>>,
         mut exit_seen_rx: watch::Receiver<bool>| {
            tokio::spawn(async move {
                loop {
                    let recv_result = if *exit_seen_rx.borrow() {
                        // Once exit has been observed, we no longer want a timer here. Some
                        // backends publish the exit code before their final stdout/stderr bytes
                        // have been forwarded through the broadcast channel, so a fixed grace
                        // period can still drop the tail of the stream under load.
                        //
                        // Instead, keep waiting until the driver closes the broadcast sender.
                        // That makes the shutdown contract explicit: the backend is responsible
                        // for dropping its sender when it has truly finished forwarding output.
                        output_rx.recv().await
                    } else {
                        tokio::select! {
                            _ = exit_seen_rx.changed() => {
                                continue;
                            }
                            result = output_rx.recv() => result,
                        }
                    };
                    match recv_result {
                        Ok(chunk) => {
                            if output_tx.send(chunk).await.is_err() {
                                break;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            })
        };
    let reader_handle = spawn_stream_reader(stdout_driver_rx, stdout_tx, exit_seen_rx.clone());
    let stderr_reader_handle = stderr_driver_rx
        .take()
        .map(|rx| spawn_stream_reader(rx, stderr_tx, exit_seen_rx));

    let writer_handle = writer_handle.unwrap_or_else(|| tokio::spawn(async {}));

    let (exit_tx, exit_rx_out) = oneshot::channel::<i32>();
    let exit_status = Arc::new(AtomicBool::new(false));
    let wait_exit_status = Arc::clone(&exit_status);
    let exit_code = Arc::new(StdMutex::new(None));
    let wait_exit_code = Arc::clone(&exit_code);
    let wait_handle = tokio::spawn(async move {
        let code = exit_rx.await.unwrap_or(-1);
        wait_exit_status.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Ok(mut guard) = wait_exit_code.lock() {
            *guard = Some(code);
        }
        let _ = exit_seen_tx.send(true);
        let _ = exit_tx.send(code);
    });

    let handle = ProcessHandle::new(
        writer_tx,
        Box::new(ClosureTerminator {
            inner: terminator,
            #[cfg(windows)]
            interrupt_terminates,
        }),
        reader_handle,
        stderr_reader_handle
            .map(|handle| handle.abort_handle())
            .into_iter()
            .collect(),
        writer_handle,
        wait_handle,
        exit_status,
        exit_code,
        /*pty_handles*/ None,
        resizer,
    );

    SpawnedProcess {
        session: handle,
        stdout_rx,
        stderr_rx,
        exit_rx: exit_rx_out,
    }
}
