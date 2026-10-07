//! Stdio shutdown under blocked output and runtime teardown, adapted from Codex's
//! `app-server/tests/suite/v2/connection_handling_stdio_tests.rs`.
//!
//! A blocked Tokio task stands in for the upstream's blocked filesystem write: it keeps runtime
//! teardown waiting without requiring the app-server filesystem API. The production 45-second
//! watchdog runs unchanged in a subprocess. Each child drops its runtime before exiting directly,
//! so the test harness cannot hang trying to print its result through the blocked stdout lock.

#![cfg(unix)]

use std::{
    io::{BufRead, BufReader, Read},
    process::{Child, ChildStdout, Command, ExitStatus, Stdio},
    sync::mpsc as std_mpsc,
    time::{Duration, Instant},
};

use ra_protocol::{
    frame::JsonRpcNotification,
    transport::{QueuedOutgoingMessage, TransportEvent, stdio::start_stdio_connection},
};
use serde_json::json;
use tokio::{runtime::Builder, sync::oneshot, time::timeout};

const CHILD_MODE: &str = "RA_STDIO_SHUTDOWN_CHILD_MODE";
const BLOCKED_STDOUT: &str = "blocked-stdout";
const BLOCKED_RUNTIME: &str = "blocked-runtime";
const PATIENCE: Duration = Duration::from_secs(20);
const SHUTDOWN_DEADLINE: Duration = Duration::from_secs(45);
const WATCHDOG_PATIENCE: Duration = Duration::from_secs(55);

/// Runs the real transport with either a full stdout pipe or an uncancellable blocking task.
#[test]
#[ignore = "runs only as the child process of the stdio shutdown tests"]
fn stdio_shutdown_child() {
    let mode = std::env::var(CHILD_MODE).expect("set by the parent test");
    assert!(matches!(mode.as_str(), BLOCKED_STDOUT | BLOCKED_RUNTIME));
    let runtime = Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let blocker = runtime.block_on(async {
        let (tx, mut events) = tokio::sync::mpsc::channel(128);
        let connection = start_stdio_connection(tx, true).await.unwrap();
        let Some(TransportEvent::ConnectionOpened { writer, .. }) = events.recv().await else {
            panic!("the connection opens first");
        };

        let mut pending_write = None;
        let mut blocker = None;
        if mode == BLOCKED_STDOUT {
            let (written_tx, mut written_rx) = oneshot::channel();
            writer
                .send(
                    QueuedOutgoingMessage::new(
                        JsonRpcNotification::new("large")
                            .with_params(json!("x".repeat(8 * 1024 * 1024))),
                    )
                    .with_write_complete(written_tx),
                )
                .await
                .unwrap();
            assert!(
                timeout(Duration::from_millis(250), &mut written_rx)
                    .await
                    .is_err(),
                "the unread stdout pipe must not accept the whole message"
            );
            pending_write = Some(written_rx);
        } else {
            let (release_tx, release_rx) = std_mpsc::channel::<()>();
            let (started_tx, started_rx) = oneshot::channel();
            tokio::task::spawn_blocking(move || {
                started_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            });
            started_rx.await.unwrap();
            blocker = Some(release_tx);
        }
        eprintln!("ready");

        match events.recv().await {
            Some(TransportEvent::ConnectionClosed { .. }) => {}
            other => panic!("expected the connection to close, got {other:?}"),
        }
        drop(writer);
        timeout(PATIENCE, connection).await.unwrap().unwrap();
        if let Some(written_rx) = pending_write {
            assert!(
                written_rx.await.is_err(),
                "SIGTERM abandons the blocked write without acknowledging it"
            );
        }
        eprintln!("transport finished");
        blocker
    });

    // Keep the blocking task's release sender alive throughout runtime teardown. Only the
    // process watchdog can end teardown in this mode.
    drop(runtime);
    drop(blocker);
    std::process::exit(0);
}

/// Owns a child and keeps its stdout pipe unread except for the write-start probe.
struct ShutdownChild {
    process: Child,
    markers: std_mpsc::Receiver<String>,
}

impl ShutdownChild {
    fn start(mode: &str) -> Self {
        let mut process = Command::new(std::env::current_exe().unwrap())
            .args([
                "stdio_shutdown_child",
                "--exact",
                "--ignored",
                "--nocapture",
                "--quiet",
            ])
            .env(CHILD_MODE, mode)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stderr = process.stderr.take().unwrap();
        let (marker_tx, markers) = std_mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines() {
                let Ok(line) = line else { break };
                if marker_tx.send(line).is_err() {
                    break;
                }
            }
        });
        let child = Self { process, markers };
        child.expect_marker("ready");
        child
    }

    fn expect_marker(&self, expected: &str) {
        assert_eq!(
            self.markers
                .recv_timeout(PATIENCE)
                .expect("the child reaches the expected shutdown stage"),
            expected
        );
    }

    fn send_sigterm(&self) {
        let status = Command::new("kill")
            .args(["-TERM", &self.process.id().to_string()])
            .status()
            .unwrap();
        assert!(status.success(), "failed to signal the child: {status}");
    }

    fn wait(&mut self, patience: Duration) -> ExitStatus {
        let deadline = Instant::now() + patience;
        loop {
            if let Some(status) = self.process.try_wait().unwrap() {
                return status;
            }
            assert!(Instant::now() < deadline, "the child did not exit");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn confirm_stdout_write_started(&mut self) {
        let mut stdout = self.process.stdout.take().unwrap();
        let (started_tx, started_rx) = std_mpsc::channel::<ChildStdout>();
        std::thread::spawn(move || {
            // Skip the test harness's preamble, then stop at the first byte of the JSON message.
            // Return the pipe without draining the large response, leaving its writer blocked.
            let mut byte = [0];
            loop {
                stdout.read_exact(&mut byte).unwrap();
                if byte[0] == b'{' {
                    let _ = started_tx.send(stdout);
                    break;
                }
            }
        });
        self.process.stdout = Some(
            started_rx
                .recv_timeout(PATIENCE)
                .expect("the transport starts writing the large message"),
        );
    }
}

impl Drop for ShutdownChild {
    fn drop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

/// A full stdout pipe does not prevent clean SIGTERM shutdown while stdin remains open.
#[test]
fn stdio_sigterm_exits_with_blocked_stdout() {
    let mut child = ShutdownChild::start(BLOCKED_STDOUT);
    child.confirm_stdout_write_started();
    child.send_sigterm();
    child.expect_marker("transport finished");
    let status = child.wait(PATIENCE);
    assert!(status.success(), "the child did not exit cleanly: {status}");
    assert!(child.process.stdin.is_some(), "stdin stays open throughout");
    assert!(
        child.process.stdout.is_some(),
        "stdout stays open and unread"
    );
}

/// EOF bounds runtime teardown even when an uncancellable blocking task cannot finish.
#[test]
fn stdio_eof_watchdog_bounds_blocked_runtime_teardown() {
    assert_watchdog_exit(false);
}

/// SIGTERM bounds runtime teardown even when stdin and stdout remain open.
#[test]
fn stdio_sigterm_watchdog_bounds_blocked_runtime_teardown() {
    assert_watchdog_exit(true);
}

fn assert_watchdog_exit(sigterm: bool) {
    let mut child = ShutdownChild::start(BLOCKED_RUNTIME);
    let shutdown_at = Instant::now();
    if sigterm {
        child.send_sigterm();
    } else {
        drop(child.process.stdin.take());
    }
    child.expect_marker("transport finished");
    let remaining = WATCHDOG_PATIENCE.saturating_sub(shutdown_at.elapsed());
    let status = child.wait(remaining);
    assert_eq!(status.code(), Some(1), "the watchdog ends the process");
    assert!(
        shutdown_at.elapsed() >= SHUTDOWN_DEADLINE,
        "the process must leave the full graceful-shutdown interval"
    );
    assert_eq!(child.process.stdin.is_some(), sigterm);
    assert!(
        child.process.stdout.is_some(),
        "stdout stays open throughout"
    );
}
