//! A Docker daemon stand-in for the Docker backend's interactive-process tests.
//!
//! The reference's PTY tests use three fakes of their own: `_FakePtyApi` answers `exec_create`,
//! `exec_start` and `exec_inspect` and holds the process's running flag and exit code,
//! `_FakePtySocket` queues output chunks and records what was sent to it and whether it was shut
//! down or closed, and `_FakePtyContainer` records the one-shot commands the session runs. This is
//! the three at the seam this port has, the `DockerApi` trait. As in the reference, sending input
//! ends the process: the input is echoed as its last output and the exit code becomes 0.

#![allow(dead_code)]

use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use ra_core::sandbox::{Manifest, SandboxSessionState, Snapshot};
use ra_sandbox::docker::{
    ContainerCreateSpec, DOCKER_BACKEND_ID, DockerApi, DockerApiError, DockerSandboxSession,
    DockerStateFields, ExecAttachment, ExecCreateRequest, ExecFrame, ExecInspect, ExecRunOutput,
    ExecRunRequest, ExecStreamKind,
};
use serde_json::{Value, json};
use tokio::io::AsyncWrite;
use tokio::sync::{Notify, mpsc};

/// The image the reference's tests use.
pub const IMAGE: &str = "python:3.14-slim";

/// The exec id every creation answers with, as `_FakePtyApi.exec_create` does.
pub const EXEC_ID: &str = "exec-123";

/// The wrapper that records the process's pid before running it.
pub const PTY_PID_WRAPPER_SCRIPT: &str =
    r#"mkdir -p "$1" && printf "%s" "$$" > "$2" && shift 2 && exec "$@""#;

/// The reference's `_PREPARE_USER_PTY_PID_SCRIPT`, as Python evaluates it.
pub const PREPARE_USER_PTY_PID_SCRIPT: &str = "pid_path=\"$1\"\npid_user=\"$2\"\npid_parent=\"$(dirname \"$pid_path\")\"\nmkdir -p \"$pid_parent\" && chmod 0711 \"$pid_parent\" && : > \"$pid_path\" && chown \"$pid_user\" \"$pid_path\" && chmod 0600 \"$pid_path\"\n";

/// The kill command the reference runs through `exec_run`, as Python evaluates it.
pub const KILL_PTY_PID_SCRIPT: &str = r#"if [ -f "$1" ]; then pid="$(cat "$1" 2>/dev/null || true)"; if [ -n "$pid" ]; then kill -KILL "$pid" >/dev/null 2>&1 || true; fi; fi"#;

/// Locks a mutex, ignoring poisoning from a panicked test thread.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One recorded one-shot command, as `_FakePtyContainer.exec_run` records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecCall {
    pub cmd: Vec<String>,
    pub workdir: Option<String>,
    pub user: Option<String>,
}

/// One recorded exec creation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateCall {
    pub container_id: String,
    pub request: ExecCreateRequest,
}

/// Which start-up call to hold back, to outlast a timeout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delayed {
    ExecCreate,
    ExecStart,
}

/// What the process's input does when it is written to.
#[derive(Debug, Clone, Copy)]
pub enum SendBehavior {
    /// Holds a write pending to simulate transport backpressure.
    BlockWrite,
    /// Accepts bytes but holds flushing pending.
    BlockFlush,
    /// Records the input, echoes it as the last output, and ends the process with 0.
    Ends,
    /// Fails with this operating-system error number.
    FailsWithErrno(i32),
    /// Fails with an error of this kind and no error number.
    FailsWithKind(std::io::ErrorKind),
}

/// Everything the fake and the attachments it hands out share.
struct Shared {
    running: AtomicBool,
    exit_code: Mutex<Option<i64>>,
    chunks: mpsc::UnboundedSender<Option<Vec<u8>>>,
    sent: Mutex<Vec<Vec<u8>>>,
    shutdowns: AtomicUsize,
    hold_shutdown: AtomicBool,
    blocked_writes: AtomicUsize,
    input_closed: AtomicBool,
    send_behavior: Mutex<SendBehavior>,
    /// Released by the first exit status query, when output is held back until then.
    inspected: Notify,
}

impl Shared {
    fn finish(&self, code: i64) {
        self.running.store(false, Ordering::SeqCst);
        *lock(&self.exit_code) = Some(code);
    }
}

/// The fake daemon.
pub struct FakePtyDocker {
    shared: Arc<Shared>,
    output: Mutex<Option<mpsc::UnboundedReceiver<Option<Vec<u8>>>>>,
    /// Output waits for the first exit status query before it is sent.
    pub hold_output_until_inspected: AtomicBool,
    /// Exec creations asked for.
    pub creates: Mutex<Vec<CreateCall>>,
    /// Exec starts asked for, with whether a terminal was asked for.
    pub starts: Mutex<Vec<(String, bool)>>,
    /// Exit status queries asked for.
    pub inspects: Mutex<Vec<String>>,
    /// One-shot commands run.
    pub execs: Mutex<Vec<ExecCall>>,
    /// Containers stopped, and one-shot commands, in the order they happened.
    pub events: Mutex<Vec<String>>,
    /// A start-up call held back for longer than the tests' timeouts.
    pub delayed: Mutex<Option<Delayed>>,
    /// A failure every exec creation returns.
    pub create_error: Mutex<Option<DockerApiError>>,
    /// The exit status every one-shot command reports.
    pub exec_exit_code: Mutex<i64>,
    /// How many of the next exit status queries fail.
    pub inspect_failures: AtomicUsize,
}

impl FakePtyDocker {
    /// A daemon whose process is running and has produced `initial_chunks` so far.
    pub fn new(initial_chunks: &[&[u8]]) -> Self {
        let (sender, receiver) = mpsc::unbounded_channel();
        for chunk in initial_chunks {
            sender.send(Some(chunk.to_vec())).expect("queue a chunk");
        }
        Self {
            shared: Arc::new(Shared {
                running: AtomicBool::new(true),
                exit_code: Mutex::new(None),
                chunks: sender,
                sent: Mutex::new(Vec::new()),
                shutdowns: AtomicUsize::new(0),
                hold_shutdown: AtomicBool::new(false),
                blocked_writes: AtomicUsize::new(0),
                input_closed: AtomicBool::new(false),
                send_behavior: Mutex::new(SendBehavior::Ends),
                inspected: Notify::new(),
            }),
            output: Mutex::new(Some(receiver)),
            hold_output_until_inspected: AtomicBool::new(false),
            creates: Mutex::new(Vec::new()),
            starts: Mutex::new(Vec::new()),
            inspects: Mutex::new(Vec::new()),
            execs: Mutex::new(Vec::new()),
            events: Mutex::new(Vec::new()),
            delayed: Mutex::new(None),
            create_error: Mutex::new(None),
            exec_exit_code: Mutex::new(0),
            inspect_failures: AtomicUsize::new(0),
        }
    }

    /// Queues more output.
    pub fn push_chunk(&self, chunk: &[u8]) {
        self.shared
            .chunks
            .send(Some(chunk.to_vec()))
            .expect("queue a chunk");
    }

    /// Ends the output stream once what is queued has been read.
    pub fn close_output(&self) {
        self.shared.chunks.send(None).expect("queue the end");
    }

    /// Marks the process as exited with `code`.
    pub fn finish(&self, code: i64) {
        self.shared.finish(code);
    }

    /// Makes writes to the process's input behave this way.
    pub fn on_send(&self, behavior: SendBehavior) {
        *lock(&self.shared.send_behavior) = behavior;
    }

    /// What was written to the process's input, one entry per write.
    pub fn sent(&self) -> Vec<Vec<u8>> {
        lock(&self.shared.sent).clone()
    }

    /// Holds the input half-close pending after attachment has succeeded.
    pub fn hold_shutdown(&self) {
        self.shared.hold_shutdown.store(true, Ordering::SeqCst);
    }

    /// Whether the input task has reached a deliberately blocked write or flush.
    pub fn write_blocked(&self) -> bool {
        self.shared.blocked_writes.load(Ordering::SeqCst) > 0
    }

    /// How many times the process's input was shut down.
    pub fn shutdown_calls(&self) -> usize {
        self.shared.shutdowns.load(Ordering::SeqCst)
    }

    /// Whether the process's input has been dropped, which is how an attachment is closed.
    pub fn input_closed(&self) -> bool {
        self.shared.input_closed.load(Ordering::SeqCst)
    }

    /// The one-shot commands run so far.
    pub fn exec_calls(&self) -> Vec<ExecCall> {
        lock(&self.execs).clone()
    }

    /// The exec creations asked for so far.
    pub fn create_calls(&self) -> Vec<CreateCall> {
        lock(&self.creates).clone()
    }

    async fn hold_back(&self, operation: Delayed) {
        if *lock(&self.delayed) == Some(operation) {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
}

/// The process's input: records writes, and ends the process as `_FakePtySocket.sendall` does.
struct FakeInput {
    shared: Arc<Shared>,
}

impl AsyncWrite for FakeInput {
    fn poll_write(
        self: Pin<&mut Self>,
        _context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let behavior = *lock(&self.shared.send_behavior);
        match behavior {
            SendBehavior::BlockWrite => {
                self.shared.blocked_writes.fetch_add(1, Ordering::SeqCst);
                Poll::Pending
            }
            SendBehavior::BlockFlush => {
                lock(&self.shared.sent).push(buffer.to_vec());
                Poll::Ready(Ok(buffer.len()))
            }
            SendBehavior::FailsWithErrno(errno) => {
                Poll::Ready(Err(std::io::Error::from_raw_os_error(errno)))
            }
            SendBehavior::FailsWithKind(kind) => Poll::Ready(Err(std::io::Error::from(kind))),
            SendBehavior::Ends => {
                lock(&self.shared.sent).push(buffer.to_vec());
                self.shared.finish(0);
                let _ = self.shared.chunks.send(Some(buffer.to_vec()));
                let _ = self.shared.chunks.send(None);
                Poll::Ready(Ok(buffer.len()))
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        if matches!(*lock(&self.shared.send_behavior), SendBehavior::BlockFlush) {
            self.shared.blocked_writes.fetch_add(1, Ordering::SeqCst);
            return Poll::Pending;
        }
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        _context: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        self.shared.shutdowns.fetch_add(1, Ordering::SeqCst);
        if self.shared.hold_shutdown.load(Ordering::SeqCst) {
            return Poll::Pending;
        }
        Poll::Ready(Ok(()))
    }
}

impl Drop for FakeInput {
    fn drop(&mut self) {
        self.shared.input_closed.store(true, Ordering::SeqCst);
    }
}

#[async_trait]
impl DockerApi for FakePtyDocker {
    async fn inspect_image(&self, _image: &str) -> Result<(), DockerApiError> {
        Ok(())
    }

    async fn pull_image(
        &self,
        _repository: &str,
        _tag: Option<&str>,
    ) -> Result<(), DockerApiError> {
        Ok(())
    }

    async fn create_container(
        &self,
        _spec: &ContainerCreateSpec,
    ) -> Result<String, DockerApiError> {
        Err(DockerApiError::api(500, "not used by these tests"))
    }

    async fn start_container(&self, _id: &str) -> Result<(), DockerApiError> {
        Ok(())
    }

    async fn stop_container(&self, id: &str) -> Result<(), DockerApiError> {
        lock(&self.events).push(format!("stop:{id}"));
        Ok(())
    }

    async fn remove_container(&self, _id: &str, _force: bool) -> Result<(), DockerApiError> {
        Ok(())
    }

    async fn inspect_container(&self, _id: &str) -> Result<Value, DockerApiError> {
        Ok(json!({"State": {"Status": "running"}}))
    }

    async fn inspect_volume(&self, name: &str) -> Result<(), DockerApiError> {
        Err(DockerApiError::not_found(format!("no such volume: {name}")))
    }

    async fn remove_volume(&self, name: &str) -> Result<(), DockerApiError> {
        Err(DockerApiError::not_found(format!("no such volume: {name}")))
    }

    async fn exec_create(
        &self,
        container_id: &str,
        request: &ExecCreateRequest,
    ) -> Result<String, DockerApiError> {
        lock(&self.creates).push(CreateCall {
            container_id: container_id.to_owned(),
            request: request.clone(),
        });
        self.hold_back(Delayed::ExecCreate).await;
        if let Some(error) = lock(&self.create_error).clone() {
            return Err(error);
        }
        Ok(EXEC_ID.to_owned())
    }

    async fn exec_start(&self, exec_id: &str, tty: bool) -> Result<ExecAttachment, DockerApiError> {
        lock(&self.starts).push((exec_id.to_owned(), tty));
        self.hold_back(Delayed::ExecStart).await;
        let receiver = lock(&self.output).take().expect("one attachment per test");
        let hold = self.hold_output_until_inspected.load(Ordering::SeqCst);
        let shared = Arc::clone(&self.shared);
        let output = futures::stream::unfold(
            (receiver, hold, shared),
            |(mut receiver, hold, shared)| async move {
                if hold {
                    shared.inspected.notified().await;
                }
                match receiver.recv().await {
                    Some(Some(chunk)) => Some((
                        Ok(ExecFrame::new(ExecStreamKind::Stdout, chunk)),
                        (receiver, false, shared),
                    )),
                    _ => None,
                }
            },
        )
        .boxed();
        let input = FakeInput {
            shared: Arc::clone(&self.shared),
        };
        Ok(ExecAttachment::new(output, Box::pin(input)))
    }

    async fn exec_inspect(&self, exec_id: &str) -> Result<ExecInspect, DockerApiError> {
        lock(&self.inspects).push(exec_id.to_owned());
        self.shared.inspected.notify_one();
        let failing = self
            .inspect_failures
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                left.checked_sub(1)
            })
            .is_ok();
        if failing {
            return Err(DockerApiError::transport("connection reset"));
        }
        Ok(ExecInspect::new(
            self.shared.running.load(Ordering::SeqCst),
            *lock(&self.shared.exit_code),
        ))
    }

    async fn exec_run(
        &self,
        _container_id: &str,
        request: &ExecRunRequest,
    ) -> Result<ExecRunOutput, DockerApiError> {
        lock(&self.execs).push(ExecCall {
            cmd: request.cmd().to_vec(),
            workdir: request.workdir().map(str::to_owned),
            user: request.user().map(str::to_owned),
        });
        lock(&self.events).push(format!("exec:{}", request.cmd().join(" ")));
        Ok(ExecRunOutput::new(
            Vec::new(),
            Vec::new(),
            Some(*lock(&self.exec_exit_code)),
        ))
    }

    async fn get_archive(
        &self,
        _container_id: &str,
        _path: &str,
    ) -> Result<Vec<u8>, DockerApiError> {
        Err(DockerApiError::api(500, "not used by these tests"))
    }
}

/// A session over `fake` whose workspace root is ready, as the reference's PTY tests build one.
pub fn ready_session(fake: &Arc<FakePtyDocker>) -> DockerSandboxSession {
    let state = DockerStateFields::new(IMAGE, "container")
        .apply(SandboxSessionState::new(
            DOCKER_BACKEND_ID,
            Snapshot::noop(),
            Manifest::new().with_root("/workspace"),
        ))
        .with_workspace_root_ready(true);
    DockerSandboxSession::new(fake.clone(), state).expect("a docker state")
}
