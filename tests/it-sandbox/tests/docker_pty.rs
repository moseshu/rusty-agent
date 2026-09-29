//! `ra-sandbox::docker`: interactive processes inside the container.
//!
//! Ports the PTY tests of the reference's `tests/sandbox/test_docker.py`. The daemon is the fake in
//! `support/docker_pty_fake.rs`, the three PTY fakes of the reference folded into one `DockerApi`.

#[path = "support/docker_pty_fake.rs"]
mod docker_pty_fake;

use std::sync::Arc;
use std::sync::atomic::Ordering;

use docker_pty_fake::{
    Delayed, EXEC_ID, ExecCall, FakePtyDocker, KILL_PTY_PID_SCRIPT, PREPARE_USER_PTY_PID_SCRIPT,
    PTY_PID_WRAPPER_SCRIPT, SendBehavior, ready_session,
};
use ra_core::sandbox::{
    ErrorCode, PtyStartRequest, PtyWriteRequest, SandboxErrorDetails, SandboxSession,
    ShellInvocation, User,
};
use ra_sandbox::docker::DockerApiError;
use rstest::rstest;

/// Starts `argv` with no shell of the session's own, as the reference's tests do with
/// `shell=False`.
fn direct(argv: &[&str]) -> PtyStartRequest {
    PtyStartRequest::new(argv.iter().map(|part| (*part).to_owned()))
        .with_shell(ShellInvocation::None)
}

/// `_assert_pty_exec_create_call`: the wrapped command, its streams, the terminal flag and the
/// workspace root as the working directory.
fn assert_pty_exec_create_call(fake: &FakePtyDocker, command_suffix: &[&str], tty: bool) {
    let calls = fake.create_calls();
    assert_eq!(calls.len(), 1);
    let call = &calls[0];
    assert_eq!(call.container_id, "container");
    assert!(call.request.stdin());
    assert!(call.request.stdout());
    assert!(call.request.stderr());
    assert_eq!(call.request.tty(), tty);
    assert_eq!(call.request.workdir(), Some("/workspace"));
    let cmd = call.request.cmd();
    assert_eq!(cmd[..3], ["sh", "-lc", PTY_PID_WRAPPER_SCRIPT]);
    assert_eq!(cmd[3], "sh");
    assert_eq!(cmd[cmd.len() - command_suffix.len()..], *command_suffix);
}

/// The pid file the wrapper was told to write.
fn pid_path(fake: &FakePtyDocker) -> String {
    fake.create_calls()[0].request.cmd()[5].clone()
}

/// `_assert_pty_kill_call`: the kill script, run without a working directory.
fn assert_pty_kill_call(call: &ExecCall) {
    assert_eq!(call.workdir, None);
    assert_eq!(call.user, None);
    assert_eq!(call.cmd[..3], ["sh", "-lc", KILL_PTY_PID_SCRIPT]);
    assert_eq!(call.cmd[3], "sh");
}

/// The removal of a pid file through the session's own `exec`, in the workspace root.
fn rm_call(path: &str) -> ExecCall {
    ExecCall {
        cmd: vec!["rm".into(), "-rf".into(), "--".into(), path.to_owned()],
        workdir: Some("/workspace".to_owned()),
        user: None,
    }
}

#[tokio::test]
async fn the_docker_backend_offers_a_terminal() {
    let fake = Arc::new(FakePtyDocker::new(&[]));
    assert!(ready_session(&fake).supports_pty());
}

// `test_docker_pty_exec_write_and_poll`
#[tokio::test]
async fn a_terminal_process_is_written_to_and_forgotten_once_it_exits() {
    let fake = Arc::new(FakePtyDocker::new(&[b"ready\n"]));
    let session = ready_session(&fake);

    let started = session
        .pty_start(direct(&["python3"]).with_tty(true).with_yield_time_s(0.25))
        .await
        .expect("start");

    let process_id = started.process_id.expect("still running");
    assert_eq!(started.exit_code, None);
    assert_eq!(started.output, b"ready\n");
    assert_pty_exec_create_call(&fake, &["python3"], true);
    assert_eq!(*fake.starts.lock().unwrap(), [(EXEC_ID.to_owned(), true)]);

    let updated = session
        .pty_write(PtyWriteRequest::new(process_id, "hello\n").with_yield_time_s(0.25))
        .await
        .expect("write");

    assert_eq!(updated.process_id, None);
    assert_eq!(updated.exit_code, Some(0));
    assert_eq!(updated.output, b"hello\n");
    assert_eq!(fake.sent(), [b"hello\n".to_vec()]);

    let error = session
        .pty_write(PtyWriteRequest::poll(process_id))
        .await
        .expect_err("forgotten");
    assert_eq!(error.error_code(), ErrorCode::PtySessionNotFound);
}

// `test_docker_pty_exec_uses_native_docker_user_without_sudo`
#[tokio::test]
async fn another_user_is_handed_to_docker_and_gets_a_pid_file_it_can_write() {
    let fake = Arc::new(FakePtyDocker::new(&[]));
    let session = ready_session(&fake);

    let started = session
        .pty_start(
            direct(&["whoami"])
                .as_user(User::new("sandbox-user"))
                .with_yield_time_s(0.0),
        )
        .await
        .expect("start");

    assert!(started.process_id.is_some());
    assert_pty_exec_create_call(&fake, &["whoami"], false);
    let create = &fake.create_calls()[0].request;
    assert_eq!(create.user(), Some("sandbox-user"));
    assert!(
        !create.cmd().iter().any(|part| part == "sudo"),
        "{:?}",
        create.cmd()
    );
    let pid_path = pid_path(&fake);
    assert_eq!(
        fake.exec_calls(),
        [ExecCall {
            cmd: vec![
                "sh".into(),
                "-lc".into(),
                PREPARE_USER_PTY_PID_SCRIPT.into(),
                "sh".into(),
                pid_path,
                "sandbox-user".into(),
            ],
            workdir: Some("/workspace".to_owned()),
            user: None,
        }]
    );
    session.pty_terminate_all().await.expect("terminate");
}

// `test_docker_pty_write_stdin_ignores_closed_socket_errors_and_returns_exit`, with the two
// failures the reference passes and the other two it ignores.
#[rstest]
#[case::broken_pipe(SendBehavior::FailsWithKind(std::io::ErrorKind::BrokenPipe))]
#[case::epipe(SendBehavior::FailsWithErrno(32))]
#[case::ebadf(SendBehavior::FailsWithErrno(9))]
#[case::connection_reset(SendBehavior::FailsWithKind(std::io::ErrorKind::ConnectionReset))]
#[tokio::test]
async fn a_write_to_a_closing_connection_is_ignored_and_the_exit_is_reported(
    #[case] failure: SendBehavior,
) {
    let fake = Arc::new(FakePtyDocker::new(&[b"ready\n"]));
    let session = ready_session(&fake);
    let started = session
        .pty_start(direct(&["python3"]).with_tty(true).with_yield_time_s(0.25))
        .await
        .expect("start");
    let process_id = started.process_id.expect("still running");

    fake.finish(0);
    fake.push_chunk(b"tail\n");
    fake.close_output();
    fake.on_send(failure);

    let updated = session
        .pty_write(PtyWriteRequest::new(process_id, "hello\n").with_yield_time_s(0.25))
        .await
        .expect("write");

    assert_eq!(updated.process_id, None);
    assert_eq!(updated.exit_code, Some(0));
    assert_eq!(updated.output, b"tail\n");
}

/// Any other write failure is the caller's to see, as the reference lets it propagate.
#[tokio::test]
async fn any_other_write_failure_is_reported() {
    let fake = Arc::new(FakePtyDocker::new(&[b"ready\n"]));
    let session = ready_session(&fake);
    let started = session
        .pty_start(direct(&["python3"]).with_tty(true).with_yield_time_s(0.25))
        .await
        .expect("start");
    let process_id = started.process_id.expect("still running");
    fake.on_send(SendBehavior::FailsWithKind(
        std::io::ErrorKind::PermissionDenied,
    ));

    let error = session
        .pty_write(PtyWriteRequest::new(process_id, "hello\n"))
        .await
        .expect_err("a failed write");

    assert_eq!(error.error_code(), ErrorCode::ExecTransportError);
    assert_eq!(error.context()["session_id"], process_id.get());
    session.pty_terminate_all().await.expect("terminate");
}

// `test_docker_pty_non_tty_rejects_stdin_and_stop_cleans_up`
#[tokio::test]
async fn a_process_without_a_terminal_refuses_input_and_stop_ends_it() {
    let fake = Arc::new(FakePtyDocker::new(&[b"stdout\n", b"stderr\n"]));
    let session = ready_session(&fake);

    let started = session
        .pty_start(
            direct(&["sh", "-c", "sleep 30"])
                .with_tty(false)
                .with_yield_time_s(0.25),
        )
        .await
        .expect("start");

    let process_id = started.process_id.expect("still running");
    assert_eq!(started.exit_code, None);
    assert_eq!(started.output, b"stdout\nstderr\n");
    assert_eq!(fake.shutdown_calls(), 1);

    let error = session
        .pty_write(PtyWriteRequest::new(process_id, "hello"))
        .await
        .expect_err("no input");
    assert!(
        matches!(
            error.details(),
            Some(SandboxErrorDetails::PtyStdinUnavailable { .. })
        ),
        "{error:?}"
    );
    assert_eq!(error.message(), "stdin is not available for this process");

    session.stop().await.expect("stop");

    assert!(fake.input_closed());
    let calls = fake.exec_calls();
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert_pty_kill_call(&calls[0]);
    assert_eq!(calls[0].cmd[4], pid_path(&fake));
    assert_eq!(calls[1], rm_call(&pid_path(&fake)));

    let error = session
        .pty_write(PtyWriteRequest::poll(process_id))
        .await
        .expect_err("forgotten");
    assert_eq!(error.error_code(), ErrorCode::PtySessionNotFound);
}

// `test_docker_pty_exec_start_times_out_blocking_docker_startup`
#[rstest]
#[case::exec_create(Delayed::ExecCreate)]
#[case::exec_start(Delayed::ExecStart)]
#[tokio::test]
async fn a_start_that_outlasts_its_timeout_kills_through_the_pid_file(#[case] delayed: Delayed) {
    let fake = Arc::new(FakePtyDocker::new(&[]));
    *fake.delayed.lock().unwrap() = Some(delayed);
    let session = ready_session(&fake);

    let request = direct(&["python3"])
        .with_tty(true)
        .with_yield_time_s(0.01)
        .with_timeout_s(0.01);
    let error = session.pty_start(request).await.expect_err("timed out");

    assert_eq!(error.error_code(), ErrorCode::ExecTimeout);
    assert!(
        matches!(
            error.details(),
            Some(SandboxErrorDetails::ExecTimeout { command, timeout_s: Some(_) })
                if command == &["python3".to_owned()]
        ),
        "{error:?}"
    );
    let calls = fake.exec_calls();
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert_pty_kill_call(&calls[0]);
    assert_eq!(calls[1], rm_call(&calls[0].cmd[4]));
}

// `test_docker_pty_exec_returns_exit_code_for_fast_exit`
#[tokio::test]
async fn a_process_that_exits_at_once_is_reported_finished_by_the_start() {
    let fake = Arc::new(FakePtyDocker::new(&[b"done\n"]));
    fake.finish(0);
    fake.close_output();
    let session = ready_session(&fake);

    let started = session
        .pty_start(
            direct(&["sh", "-c", "printf done"])
                .with_tty(false)
                .with_yield_time_s(0.25),
        )
        .await
        .expect("start");

    assert_eq!(started.process_id, None);
    assert_eq!(started.exit_code, Some(0));
    assert_eq!(started.output, b"done\n");
    assert_eq!(fake.exec_calls(), [rm_call(&pid_path(&fake))]);
}

// `test_docker_pty_exec_waits_for_socket_drain_after_process_exit`
#[tokio::test]
async fn output_sent_after_the_exit_is_known_is_still_collected() {
    let fake = Arc::new(FakePtyDocker::new(&[b"done\n"]));
    fake.finish(0);
    fake.close_output();
    fake.hold_output_until_inspected
        .store(true, Ordering::SeqCst);
    let session = ready_session(&fake);

    let started = session
        .pty_start(
            direct(&["sh", "-c", "printf done"])
                .with_tty(false)
                .with_yield_time_s(0.25),
        )
        .await
        .expect("start");

    assert_eq!(started.process_id, None);
    assert_eq!(started.exit_code, Some(0));
    assert_eq!(started.output, b"done\n");
    assert_eq!(fake.exec_calls(), [rm_call(&pid_path(&fake))]);
}

/// A daemon that refuses to set the command up leaves a transport error the caller may retry,
/// naming the command as the caller gave it.
#[tokio::test]
async fn a_daemon_refusal_to_start_is_a_retry_safe_transport_error() {
    let fake = Arc::new(FakePtyDocker::new(&[]));
    *fake.create_error.lock().unwrap() = Some(DockerApiError::api(500, "daemon down"));
    let session = ready_session(&fake);

    let error = session
        .pty_start(direct(&["python3"]).with_tty(true))
        .await
        .expect_err("refused");

    assert_eq!(error.error_code(), ErrorCode::ExecTransportError);
    assert_eq!(error.context()["retry_safe"], true);
    assert!(
        matches!(
            error.details(),
            Some(SandboxErrorDetails::ExecTransport { command }) if command == &["python3".to_owned()]
        ),
        "{error:?}"
    );
    let cause = std::error::Error::source(&error).expect("a cause");
    assert!(cause.to_string().contains("daemon down"), "{cause}");
    assert!(fake.exec_calls().is_empty());
}

/// A pid file that cannot be made for the account fails the start the same way, with the write
/// failure as the cause, and nothing is started.
#[tokio::test]
async fn a_pid_file_that_cannot_be_prepared_fails_the_start() {
    let fake = Arc::new(FakePtyDocker::new(&[]));
    *fake.exec_exit_code.lock().unwrap() = 1;
    let session = ready_session(&fake);

    let error = session
        .pty_start(direct(&["whoami"]).as_user(User::new("sandbox-user")))
        .await
        .expect_err("refused");

    assert_eq!(error.error_code(), ErrorCode::ExecTransportError);
    assert_eq!(error.context()["retry_safe"], true);
    let cause = std::error::Error::source(&error)
        .and_then(|cause| cause.downcast_ref::<ra_core::sandbox::SandboxError>())
        .expect("a sandbox error as the cause");
    assert_eq!(cause.error_code(), ErrorCode::WorkspaceArchiveWriteError);
    assert!(fake.create_calls().is_empty());
}

/// Shutdown ends interactive processes before it stops the container under them.
#[tokio::test]
async fn shutdown_ends_interactive_processes_before_stopping_the_container() {
    let fake = Arc::new(FakePtyDocker::new(&[]));
    let session = ready_session(&fake);
    session
        .pty_start(
            direct(&["sleep", "30"])
                .with_tty(true)
                .with_yield_time_s(0.0),
        )
        .await
        .expect("start");

    session.shutdown().await.expect("shutdown");

    assert!(fake.input_closed());
    let events = fake.events.lock().unwrap().clone();
    let kill = events
        .iter()
        .position(|event| event.contains("kill -KILL"))
        .expect("a kill");
    let stop = events
        .iter()
        .position(|event| event == "stop:container")
        .expect("a stop");
    assert!(kill < stop, "{events:?}");
}

/// An exit the watcher never saw — its question to the daemon failed — is asked for once more when
/// the output ends, so the call still reports the process finished.
#[tokio::test]
async fn an_exit_the_watcher_missed_is_read_once_the_output_ends() {
    let fake = Arc::new(FakePtyDocker::new(&[b"done\n"]));
    fake.inspect_failures.store(1, Ordering::SeqCst);
    fake.hold_output_until_inspected
        .store(true, Ordering::SeqCst);
    fake.finish(0);
    fake.close_output();
    let session = ready_session(&fake);

    let started = session
        .pty_start(
            direct(&["sh", "-c", "printf done"])
                .with_tty(false)
                .with_yield_time_s(0.25),
        )
        .await
        .expect("start");

    assert_eq!(started.process_id, None);
    assert_eq!(started.exit_code, Some(0));
    assert_eq!(started.output, b"done\n");
}

/// Ending a process closes its attachment at once, even while a poll still holds the process,
/// and the poll returns rather than waiting out its deadline.
#[tokio::test]
async fn ending_a_process_closes_its_attachment_even_while_a_poll_holds_it() {
    let fake = Arc::new(FakePtyDocker::new(&[]));
    let session = Arc::new(ready_session(&fake));
    let started = session
        .pty_start(
            direct(&["sleep", "30"])
                .with_tty(true)
                .with_yield_time_s(0.0),
        )
        .await
        .expect("start");
    let process_id = started.process_id.expect("still running");

    let polling = Arc::clone(&session);
    let poll = tokio::spawn(async move {
        polling
            .pty_write(PtyWriteRequest::poll(process_id).with_yield_time_s(10.0))
            .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let began = std::time::Instant::now();
    session.pty_terminate_all().await.expect("terminate");

    assert!(fake.input_closed());
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), poll)
        .await
        .expect("the poll returns once the process is ended");
    assert!(began.elapsed() < std::time::Duration::from_secs(2));
}

/// A shutdown must close the attachment even if a caller's stdin write or flush cannot progress.
#[rstest]
#[case::terminate_blocked_write(SendBehavior::BlockWrite, false)]
#[case::terminate_blocked_flush(SendBehavior::BlockFlush, false)]
#[case::shutdown_blocked_write(SendBehavior::BlockWrite, true)]
#[case::shutdown_blocked_flush(SendBehavior::BlockFlush, true)]
#[tokio::test]
async fn ending_a_process_interrupts_blocked_input(
    #[case] behavior: SendBehavior,
    #[case] shutdown: bool,
) {
    let fake = Arc::new(FakePtyDocker::new(&[]));
    let session = Arc::new(ready_session(&fake));
    let process_id = session
        .pty_start(
            direct(&["sleep", "30"])
                .with_tty(true)
                .with_yield_time_s(0.0),
        )
        .await
        .expect("start")
        .process_id
        .expect("running");
    fake.on_send(behavior);
    let writing_session = Arc::clone(&session);
    let writing = tokio::spawn(async move {
        writing_session
            .pty_write(PtyWriteRequest::new(process_id, "payload"))
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while !fake.write_blocked() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("writer reached backpressure");
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        if shutdown {
            session.shutdown().await
        } else {
            session.pty_terminate_all().await
        }
    })
    .await
    .expect("termination does not wait for stdin")
    .expect("terminate");
    assert!(fake.input_closed());
    tokio::time::timeout(std::time::Duration::from_secs(2), writing)
        .await
        .expect("writer released")
        .expect("writer task")
        .expect("closed input is ignored");
    assert_pty_kill_call(&fake.exec_calls()[0]);
    assert_eq!(
        session
            .pty_write(PtyWriteRequest::poll(process_id))
            .await
            .expect_err("forgotten")
            .error_code(),
        ErrorCode::PtySessionNotFound
    );
}

/// Half-closing a non-terminal attachment can yield; cancellation there must still end the exec.
#[tokio::test]
async fn cancelling_during_input_half_close_ends_the_unregistered_process() {
    let fake = Arc::new(FakePtyDocker::new(&[]));
    fake.hold_shutdown();
    let session = Arc::new(ready_session(&fake));
    let starting_session = Arc::clone(&session);
    let starting = tokio::spawn(async move {
        starting_session
            .pty_start(direct(&["sleep", "30"]).with_tty(false))
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while fake.shutdown_calls() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("attached and half-closing");
    starting.abort();
    assert!(starting.await.expect_err("cancelled").is_cancelled());
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while !fake.input_closed() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("guard closed the acquired attachment");
    let calls = fake.exec_calls();
    assert_eq!(calls.len(), 2);
    assert_pty_kill_call(&calls[0]);
    assert_eq!(calls[0].cmd[4], pid_path(&fake));
    assert_eq!(calls[1], rm_call(&pid_path(&fake)));
    session
        .pty_terminate_all()
        .await
        .expect("nothing left registered");
    assert_eq!(fake.exec_calls(), calls);
}
