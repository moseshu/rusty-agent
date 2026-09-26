//! `ra-sandbox::unix_local`: interactive processes, with a terminal and without one.
//!
//! Ported from the `TestUnixLocalPty` class in the reference's `tests/sandbox/test_unix_local.py`.
//! Every test runs a real process through a real session; none of them fakes the terminal.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ra_core::sandbox::{
    CreateRequest, ErrorCode, ExecRequest, Manifest, PtyExecUpdate, PtyProcessId, PtyStartRequest,
    PtyWriteRequest, SandboxClient, SandboxErrorDetails, SandboxSession, ShellInvocation,
};
use ra_sandbox::unix_local::UnixLocalSandboxClient;

/// A started session over a fresh workspace, and the directory holding it.
async fn session() -> (tempfile::TempDir, PathBuf, Box<dyn SandboxSession>) {
    let directory = tempfile::tempdir().expect("temp");
    let root = std::fs::canonicalize(directory.path())
        .expect("canonical")
        .join("workspace");
    let session = UnixLocalSandboxClient::new()
        .create(
            CreateRequest::new()
                .with_manifest(Manifest::new().with_root(root.to_string_lossy().into_owned())),
        )
        .await
        .expect("create");
    session.start().await.expect("start");
    (directory, root, session)
}

/// Starts `argv` with no shell of the session's own, as the reference's tests do with
/// `shell=False`.
fn direct(argv: &[&str]) -> PtyStartRequest {
    PtyStartRequest::new(argv.iter().map(|part| (*part).to_owned()))
        .with_shell(ShellInvocation::None)
}

fn text(update: &PtyExecUpdate) -> String {
    String::from_utf8_lossy(&update.output).into_owned()
}

async fn poll(
    session: &dyn SandboxSession,
    process_id: PtyProcessId,
    seconds: f64,
) -> PtyExecUpdate {
    session
        .pty_write(PtyWriteRequest::poll(process_id).with_yield_time_s(seconds))
        .await
        .expect("poll")
}

async fn assert_forgotten(session: &dyn SandboxSession, process_id: PtyProcessId) {
    let error = session
        .pty_write(PtyWriteRequest::poll(process_id))
        .await
        .expect_err("forgotten");
    assert_eq!(error.error_code(), ErrorCode::PtySessionNotFound);
    assert_eq!(
        error.details(),
        Some(&SandboxErrorDetails::PtySessionNotFound {
            session_id: process_id.0
        })
    );
}

#[tokio::test]
async fn the_local_backend_offers_a_terminal() {
    let (_directory, _root, session) = session().await;
    assert!(session.supports_pty());
    session.close().await.expect("close");
}

// `test_pty_exec_write_poll_and_unknown_session_errors`
#[tokio::test]
async fn a_terminal_process_takes_input_and_is_forgotten_once_it_exits() {
    let (_directory, _root, session) = session().await;

    let started = session
        .pty_start(
            direct(&["sh", "-c", "IFS= read -r line; printf '%s\\n' \"$line\""])
                .with_tty(true)
                .with_yield_time_s(0.05),
        )
        .await
        .expect("start");
    let process_id = started.process_id.expect("still running");
    assert_eq!(started.exit_code, None);

    let written = session
        .pty_write(PtyWriteRequest::new(process_id, "hello from pty\n").with_yield_time_s(0.25))
        .await
        .expect("write");
    assert_eq!(written.process_id, None);
    assert_eq!(written.exit_code, Some(0));
    assert!(
        text(&written).contains("hello from pty"),
        "{}",
        text(&written)
    );

    assert_forgotten(session.as_ref(), process_id).await;
    assert_forgotten(session.as_ref(), PtyProcessId(999_999)).await;

    session.close().await.expect("close");
}

// `test_pty_ctrl_c_interrupts_long_running_process`
#[tokio::test]
async fn ctrl_c_on_the_terminal_interrupts_a_long_running_process() {
    let (_directory, _root, session) = session().await;

    let started = session
        .pty_start(
            direct(&["sleep", "30"])
                .with_tty(true)
                .with_yield_time_s(0.05),
        )
        .await
        .expect("start");
    let process_id = started.process_id.expect("still running");
    assert_eq!(started.exit_code, None);

    let first = session
        .pty_write(PtyWriteRequest::new(process_id, "\u{3}").with_yield_time_s(0.25))
        .await
        .expect("interrupt");
    let interrupted = if first.process_id.is_none() {
        first
    } else {
        poll(session.as_ref(), process_id, 5.5).await
    };

    assert_eq!(interrupted.process_id, None);
    assert!(interrupted.exit_code.is_some());
    assert_forgotten(session.as_ref(), process_id).await;

    session.close().await.expect("close");
}

/// Set by the test below for the copy of this binary it runs under an ignored signal.
const IGNORED_SIGNAL_ENV: &str = "RA_TEST_PTY_IGNORED_SIGNAL";

// `test_pty_terminal_signals_interrupt_even_if_parent_ignores_signal`, both parameters.
//
// The reference ignores the signal in its own process for the duration of the test. Doing that
// here needs `unsafe`, so the test runs this binary again under a shell that ignores the signal
// before `exec`, which is how an ignored disposition reaches a child in the first place.
#[tokio::test]
async fn terminal_signals_interrupt_even_when_the_host_ignores_them() {
    let binary = std::env::current_exe().expect("test binary");
    for signal in ["INT", "QUIT"] {
        let status = std::process::Command::new("sh")
            .args([
                "-c",
                &format!("trap '' {signal}; exec \"$0\" \"$@\""),
                &binary.to_string_lossy(),
                "--exact",
                "under_an_ignored_signal",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(IGNORED_SIGNAL_ENV, signal)
            .status()
            .expect("run under an ignored signal");
        assert!(status.success(), "SIG{signal}: {status}");
    }
}

#[tokio::test]
#[ignore = "run by `terminal_signals_interrupt_even_when_the_host_ignores_them`"]
async fn under_an_ignored_signal() {
    let Ok(signal) = std::env::var(IGNORED_SIGNAL_ENV) else {
        return;
    };
    let (chars, signum) = match signal.as_str() {
        "INT" => ("\u{3}", 2),
        "QUIT" => ("\u{1c}", 3),
        other => panic!("unexpected signal {other}"),
    };
    let (_directory, _root, session) = session().await;

    // First show the ignore really reached this process: an ordinary child inherits it and
    // survives the signal it sends itself.
    let survived = session
        .exec(
            ExecRequest::new([
                "sh".to_owned(),
                "-c".to_owned(),
                format!("kill -{signal} $$; printf survived"),
            ])
            .with_shell(ShellInvocation::None),
        )
        .await
        .expect("exec");
    assert_eq!(String::from_utf8_lossy(&survived.stdout), "survived");

    let started = session
        .pty_start(
            direct(&["sleep", "30"])
                .with_tty(true)
                .with_yield_time_s(0.05),
        )
        .await
        .expect("start");
    let process_id = started.process_id.expect("still running");

    let interrupted = session
        .pty_write(PtyWriteRequest::new(process_id, chars).with_yield_time_s(5.5))
        .await
        .expect("interrupt");
    assert_eq!(interrupted.process_id, None);
    assert_eq!(interrupted.exit_code, Some(-signum));

    session.close().await.expect("close");
}

// `test_non_tty_pty_session_rejects_stdin_and_can_still_be_polled`
#[tokio::test]
async fn a_process_without_a_terminal_refuses_input_and_can_still_be_polled() {
    let (_directory, _root, session) = session().await;

    let started = session
        .pty_start(
            direct(&[
                "sh",
                "-c",
                "printf 'stdout\\n'; printf 'stderr\\n' >&2; sleep 1",
            ])
            .with_yield_time_s(0.05),
        )
        .await
        .expect("start");
    let process_id = started.process_id.expect("still running");
    assert_eq!(started.exit_code, None);
    // The reference's 50 ms is below the 250 ms floor, so the start waits long enough for both.
    assert!(text(&started).contains("stdout"), "{}", text(&started));
    assert!(text(&started).contains("stderr"), "{}", text(&started));

    let error = session
        .pty_write(PtyWriteRequest::new(process_id, "hello"))
        .await
        .expect_err("no terminal");
    assert_eq!(error.message(), "stdin is not available for this process");
    assert_eq!(
        error.details(),
        Some(&SandboxErrorDetails::PtyStdinUnavailable {
            session_id: process_id.0
        })
    );

    let finished = poll(session.as_ref(), process_id, 5.5).await;
    assert_eq!(finished.process_id, None);
    assert_eq!(finished.exit_code, Some(0));
    assert_eq!(text(&finished), "");

    assert_forgotten(session.as_ref(), process_id).await;
    session.close().await.expect("close");
}

// `test_stop_terminates_active_pty_sessions`
#[tokio::test]
async fn stop_ends_every_interactive_process() {
    let (_directory, _root, session) = session().await;

    let started = session
        .pty_start(
            direct(&["sh", "-c", "printf 'ready\\n'; sleep 30"])
                .with_tty(true)
                .with_yield_time_s(0.25),
        )
        .await
        .expect("start");
    let process_id = started.process_id.expect("still running");
    assert!(text(&started).contains("ready"), "{}", text(&started));

    let stopping = Instant::now();
    session.stop().await.expect("stop");
    assert!(stopping.elapsed() < Duration::from_secs(5));

    assert_forgotten(session.as_ref(), process_id).await;
    session.shutdown().await.expect("shutdown");
}

// `test_tty_fd_close_is_owned_without_blocking_termination` replaces `asyncio.to_thread` to hold a
// terminal's close open and checks the session's bookkeeping of it. The Rust close runs on a thread
// the session starts itself, with no seam to hold it open from outside; what that test protects —
// ending a terminal process never waits on the close — is what `stop_ends_every_interactive_process`
// bounds above.

// Beyond the upstream class.

/// The request's default is the login shell, which this backend runs as `sh -c` — and a shell
/// command starts in the workspace, as a one-shot command does.
#[tokio::test]
async fn an_interactive_command_starts_in_the_workspace() {
    let (_directory, root, session) = session().await;

    let finished = session
        .pty_start(PtyStartRequest::new(["pwd".to_owned()]).with_yield_time_s(5.0))
        .await
        .expect("start");

    assert_eq!(finished.process_id, None);
    assert_eq!(finished.exit_code, Some(0));
    assert_eq!(text(&finished).trim_end(), root.to_string_lossy());
    session.close().await.expect("close");
}

/// On a terminal both streams arrive as one, and the exit code is the command's own.
#[tokio::test]
async fn a_terminal_merges_both_streams_and_reports_the_exit_code() {
    let (_directory, _root, session) = session().await;

    let finished = session
        .pty_start(
            direct(&["sh", "-c", "printf out; printf err >&2; exit 3"])
                .with_tty(true)
                .with_yield_time_s(5.0),
        )
        .await
        .expect("start");

    assert_eq!(finished.process_id, None);
    assert_eq!(finished.exit_code, Some(3));
    assert!(text(&finished).contains("out"), "{}", text(&finished));
    assert!(text(&finished).contains("err"), "{}", text(&finished));
    session.close().await.expect("close");
}

#[tokio::test]
async fn returned_output_is_cut_to_the_token_budget() {
    let (_directory, _root, session) = session().await;

    let finished = session
        .pty_start(
            direct(&["sh", "-c", "printf '%0100d' 0"])
                .with_yield_time_s(5.0)
                .with_max_output_tokens(4),
        )
        .await
        .expect("start");

    assert_eq!(finished.exit_code, Some(0));
    assert_eq!(finished.original_token_count, Some(25));
    assert!(finished.output.len() <= 16, "{}", text(&finished));
    session.close().await.expect("close");
}

/// A full table ends its least recently used process to make room for the next one.
#[tokio::test]
async fn a_full_session_ends_its_least_recently_used_process() {
    let (_directory, _root, session) = session().await;
    let session: Arc<dyn SandboxSession> = Arc::from(session);
    let sleeper = || direct(&["sleep", "30"]).with_yield_time_s(0.0);

    let oldest = session
        .pty_start(sleeper())
        .await
        .expect("start")
        .process_id
        .expect("still running");

    let mut starts = tokio::task::JoinSet::new();
    for _ in 1..ra_core::sandbox::PTY_PROCESSES_MAX {
        let session = Arc::clone(&session);
        starts.spawn(async move { session.pty_start(sleeper()).await });
    }
    let mut running = Vec::new();
    while let Some(started) = starts.join_next().await {
        running.push(
            started
                .expect("join")
                .expect("start")
                .process_id
                .expect("still running"),
        );
    }
    assert!(!running.contains(&oldest));

    // The table is full; one more start ends the process nobody has touched for longest.
    let overflow = session.pty_start(sleeper()).await.expect("start");
    assert!(overflow.process_id.is_some());
    assert_forgotten(session.as_ref(), oldest).await;

    session.close().await.expect("close");
}
