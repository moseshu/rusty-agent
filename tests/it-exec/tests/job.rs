//! Behaviour of background-job lifecycle inspection and waiting.

use std::{sync::Arc, time::Duration};

use ra_core::event::exec::ExecStreamKind;
use ra_exec::{
    command::{ExecLimits, ExecRequest},
    job::{BackgroundJob, JobError, JobWaitResult, JobWaitUntil},
    session::{ExecExecutionResult, ExecSessionState, ProcessManager},
};

fn quick_yield() -> ExecLimits {
    ExecLimits::new().with_initial_yield_timeout(Duration::from_millis(100))
}

async fn start_job(manager: Arc<ProcessManager>, command: &str) -> BackgroundJob {
    start_job_with(manager, command, quick_yield()).await
}

async fn start_job_with(
    manager: Arc<ProcessManager>,
    command: &str,
    limits: ExecLimits,
) -> BackgroundJob {
    let request = ExecRequest::new(command).with_limits(limits);
    let result = manager
        .execute(request, None)
        .await
        .expect("command starts");
    assert!(matches!(result, ExecExecutionResult::Yielded { .. }));
    BackgroundJob::from_execution_result(manager, &result).expect("yielded result has a job")
}

#[tokio::test]
async fn test_job_wait_done_observes_terminal_output() {
    let manager = Arc::new(ProcessManager::default());
    let job = start_job(Arc::clone(&manager), "sleep 0.3; printf done").await;

    let result = job.wait(JobWaitUntil::Done).await.expect("job is retained");
    let JobWaitResult::Done(snapshot) = result else {
        panic!("a completed job must report done: {result:?}");
    };
    assert!(snapshot.is_terminal());
    assert!(snapshot.is_closed());
    assert_eq!(
        snapshot.state(),
        &ExecSessionState::Exited { exit_code: Some(0) }
    );
    assert_eq!(snapshot.output().stdout(), "done");
}

#[tokio::test]
async fn test_job_timeout_returns_an_active_snapshot() {
    let manager = Arc::new(ProcessManager::default());
    let job = start_job(Arc::clone(&manager), "sleep 30").await;

    let result = job
        .wait(JobWaitUntil::Timeout(Duration::from_millis(20)))
        .await
        .expect("job is retained");
    let JobWaitResult::TimedOut(snapshot) = result else {
        panic!("an active job must report timeout: {result:?}");
    };
    assert!(snapshot.state().is_active());

    job.cancel().await.expect("job is still retained");
}

#[tokio::test]
async fn test_job_wait_match_identifies_the_output_stream() {
    let manager = Arc::new(ProcessManager::default());
    let job = start_job(
        Arc::clone(&manager),
        "sleep 0.3; printf 'ready on stderr' >&2; sleep 30",
    )
    .await;

    let result = job
        .wait(JobWaitUntil::Match {
            text: "ready on stderr".to_owned(),
            timeout: Duration::from_secs(2),
        })
        .await
        .expect("job is retained");
    let JobWaitResult::Matched {
        snapshot,
        stream,
        excerpt,
    } = result
    else {
        panic!("matching output must report a match: {result:?}");
    };
    assert_eq!(stream, ExecStreamKind::Stderr);
    assert!(
        excerpt.contains("ready on stderr"),
        "the excerpt must show what matched: {excerpt}"
    );
    assert!(snapshot.output().stderr().contains("ready on stderr"));
    assert!(snapshot.state().is_active());

    job.cancel().await.expect("job is still retained");
}

#[tokio::test]
async fn test_job_match_rejects_an_empty_literal() {
    let manager = Arc::new(ProcessManager::default());
    let job = start_job(Arc::clone(&manager), "sleep 30").await;

    assert!(matches!(
        job.wait(JobWaitUntil::Match {
            text: String::new(),
            timeout: Duration::from_secs(1),
        })
        .await,
        Err(JobError::EmptyMatch)
    ));
    job.cancel().await.expect("job is still retained");
}

#[tokio::test]
async fn test_job_reports_an_unknown_session() {
    let job = BackgroundJob::new(
        Arc::new(ProcessManager::default()),
        "exec-no-longer-retained",
    );

    assert!(matches!(
        job.snapshot().await,
        Err(JobError::UnknownJob { .. })
    ));
    assert!(matches!(
        job.cancel().await,
        Err(JobError::UnknownJob { .. })
    ));
}

#[tokio::test]
async fn test_job_match_ignores_the_truncation_marker() {
    let manager = Arc::new(ProcessManager::default());
    let limits = quick_yield().with_max_capture_bytes(64);
    // Far more than the ceiling, so the retained head and tail are separated by dropped bytes and
    // the rendered summary stands a marker in their place.
    let job = start_job_with(
        Arc::clone(&manager),
        "sleep 0.3; head -c 4096 /dev/zero | tr '\\0' a; sleep 30",
        limits,
    )
    .await;

    // `omitted` appears only in that marker, never in the command's own output.
    let result = job
        .wait(JobWaitUntil::Match {
            text: "omitted".to_owned(),
            timeout: Duration::from_millis(600),
        })
        .await
        .expect("job is retained");
    let JobWaitResult::TimedOut(snapshot) = result else {
        panic!("a search must not match the marker describing a gap: {result:?}");
    };
    assert!(snapshot.output().is_truncated());
    assert!(snapshot.output().stdout().contains("omitted"));

    job.cancel().await.expect("job is still retained");
}

#[tokio::test]
async fn test_job_wait_result_carries_the_observed_snapshot() {
    let manager = Arc::new(ProcessManager::default());
    let job = start_job(Arc::clone(&manager), "sleep 30").await;

    let result = job
        .wait(JobWaitUntil::Timeout(Duration::from_millis(20)))
        .await
        .expect("job is retained");
    assert_eq!(result.snapshot().session_id(), job.session_id());
    assert!(!result.snapshot().is_closed());

    job.cancel().await.expect("job is still retained");
}

#[tokio::test]
async fn test_job_match_on_stdout_wins_when_the_process_closes() {
    let manager = Arc::new(ProcessManager::default());
    let job = start_job(Arc::clone(&manager), "sleep 0.3; printf ready").await;

    let result = job
        .wait(JobWaitUntil::Match {
            text: "ready".to_owned(),
            timeout: Duration::from_secs(2),
        })
        .await
        .expect("job is retained");
    let JobWaitResult::Matched {
        snapshot, stream, ..
    } = result
    else {
        panic!("matching final stdout must win over closeout: {result:?}");
    };
    assert_eq!(stream, ExecStreamKind::Stdout);
    assert_eq!(snapshot.output().stdout(), "ready");
}

#[tokio::test]
async fn test_job_match_spans_output_reads() {
    let manager = Arc::new(ProcessManager::default());
    let job = start_job(
        Arc::clone(&manager),
        "sleep 0.3; printf rea; sleep 0.3; printf dy; sleep 30",
    )
    .await;

    let result = job
        .wait(JobWaitUntil::Match {
            text: "ready".to_owned(),
            timeout: Duration::from_secs(2),
        })
        .await
        .expect("job is retained");
    assert!(matches!(
        result,
        JobWaitResult::Matched {
            stream: ExecStreamKind::Stdout,
            ..
        }
    ));

    job.cancel().await.expect("job is still retained");
}

#[tokio::test]
async fn test_job_match_has_a_deadline() {
    let manager = Arc::new(ProcessManager::default());
    let job = start_job(Arc::clone(&manager), "sleep 30").await;

    let result = job
        .wait(JobWaitUntil::Match {
            text: "never emitted".to_owned(),
            timeout: Duration::from_millis(20),
        })
        .await
        .expect("job is retained");
    assert!(matches!(result, JobWaitResult::TimedOut(snapshot) if snapshot.state().is_active()));

    job.cancel().await.expect("job is still retained");
}

#[tokio::test]
async fn test_job_done_waits_for_cancelled_process_closeout() {
    let manager = Arc::new(ProcessManager::default());
    let job = start_job(
        Arc::clone(&manager),
        "trap '' TERM; while :; do sleep 1; done",
    )
    .await;

    job.cancel().await.expect("job is retained");
    let result = tokio::time::timeout(Duration::from_secs(5), job.wait(JobWaitUntil::Done))
        .await
        .expect("closeout finishes within the drain contract")
        .expect("job stays retained through its own closeout");
    let JobWaitResult::Done(snapshot) = result else {
        panic!("cancelled job must report done after closeout: {result:?}");
    };
    assert_eq!(snapshot.state(), &ExecSessionState::Cancelled);
    assert!(snapshot.is_closed());
    assert!(!manager.active_sessions().await.contains(job.session_id()));
}

/// An output wait is about output the caller has not taken, not about a stream that has ever moved.
#[tokio::test]
async fn test_job_output_wait_starts_from_what_the_caller_already_took() {
    let manager = Arc::new(ProcessManager::default());
    let job = start_job(
        Arc::clone(&manager),
        "printf 'first\\n'; sleep 0.3; printf 'second\\n'; sleep 30",
    )
    .await;

    // The first line is already retained, so a wait that started from zero would return at once
    // and report output the caller had seen as if it were new.
    let produced = job
        .snapshot()
        .await
        .expect("job is retained")
        .output()
        .stdout_bytes() as u64;
    let result = job
        .wait(JobWaitUntil::Output {
            delivered_stdout: produced,
            delivered_stderr: 0,
            timeout: Duration::from_secs(2),
        })
        .await
        .expect("job is retained");
    let JobWaitResult::OutputReady(snapshot) = result else {
        panic!("new output must report itself: {result:?}");
    };
    assert!(snapshot.output().stdout().contains("second"));
    assert!(snapshot.state().is_active());

    job.cancel().await.expect("job is still retained");
}

/// Closeout outranks undelivered output, because only one of the two says to stop asking.
///
/// This is the shape of a poll that arrives after the command is already over: there are bytes
/// nobody has taken *and* nothing more will ever arrive. Reporting the output alone would leave the
/// caller to poll a finished session again for a tail that does not exist.
#[tokio::test]
async fn test_job_output_wait_reports_closeout_over_undelivered_output() {
    let manager = Arc::new(ProcessManager::default());
    let job = run_to_completion(Arc::clone(&manager), "printf last").await;

    let result = job
        .wait(JobWaitUntil::Output {
            delivered_stdout: 0,
            delivered_stderr: 0,
            timeout: Duration::from_secs(2),
        })
        .await
        .expect("job is retained");
    let JobWaitResult::Done(snapshot) = result else {
        panic!("a finished job must report done: {result:?}");
    };
    assert!(snapshot.is_closed());
    assert!(snapshot.output().stdout().contains("last"));
}

/// A silent job someone is waiting on is not an idle job.
///
/// The sweep exists to reclaim processes nobody is watching. Firing it into a wait kills the very
/// process that wait is about and hands the caller an expiry in place of the answer it asked for —
/// and a command that prints nothing until it succeeds is exactly the command worth waiting for.
#[tokio::test]
async fn test_a_watched_job_outlives_the_idle_sweep() {
    let manager = Arc::new(ProcessManager::new(
        ExecLimits::new().with_idle_timeout(Some(Duration::from_millis(150))),
    ));
    let job = start_job_with(
        Arc::clone(&manager),
        "sleep 30",
        quick_yield().with_idle_timeout(Some(Duration::from_millis(150))),
    )
    .await;

    let result = job
        .wait(JobWaitUntil::Timeout(Duration::from_millis(900)))
        .await
        .expect("job is retained");
    let JobWaitResult::TimedOut(snapshot) = result else {
        panic!("a watched silent job must outlive six idle windows: {result:?}");
    };
    assert!(
        snapshot.state().is_active(),
        "the idle sweep took a session that was being waited on: {:?}",
        snapshot.state()
    );

    job.cancel().await.expect("job is still retained");
}

/// The exemption is the wait, not a disabled sweep: nobody watching means idle still means idle.
#[tokio::test]
async fn test_an_unwatched_silent_job_is_still_swept() {
    let manager = Arc::new(ProcessManager::new(
        ExecLimits::new().with_idle_timeout(Some(Duration::from_millis(150))),
    ));
    let job = start_job_with(
        Arc::clone(&manager),
        "sleep 30",
        quick_yield().with_idle_timeout(Some(Duration::from_millis(150))),
    )
    .await;

    // Polled to a deadline rather than asserted at a fixed moment: the sweep is prompt, but this
    // test shares a machine with every other command these tests start.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let snapshot = job.snapshot().await.expect("job is retained");
        if snapshot.state().is_terminal() {
            assert_eq!(
                snapshot.state(),
                &ExecSessionState::Expired {
                    reason: ra_core::event::exec::ExecEvictionReason::IdleTimeout
                }
            );
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "an unwatched idle session was never swept"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// A finished job a caller is still working with keeps its place in the retention window.
///
/// The window is where a caller collects what it waited for. Between a wait returning and its
/// output being read, other sessions can finish and push the oldest one out; doing that to the
/// session a caller is holding turns a result it had already earned into "no such job".
#[tokio::test]
async fn test_retention_evicts_around_a_watched_job() {
    // Two finished sessions stay readable, so the third retirement drops one of the first two.
    let manager = Arc::new(ProcessManager::new(ExecLimits::new().with_max_sessions(2)));
    let first = run_to_completion(Arc::clone(&manager), "printf first").await;
    let second = run_to_completion(Arc::clone(&manager), "printf second").await;

    let watch = first.watch();
    let third = run_to_completion(Arc::clone(&manager), "printf third").await;

    assert!(
        first.snapshot().await.is_ok(),
        "a watched job must survive another session's retirement"
    );
    assert!(
        matches!(second.snapshot().await, Err(JobError::UnknownJob { .. })),
        "the oldest unwatched job is the one that makes room"
    );
    assert!(third.snapshot().await.is_ok());

    // And the exemption ends with the watch: the next retirement takes the job it was protecting.
    drop(watch);
    let fourth = run_to_completion(Arc::clone(&manager), "printf fourth").await;
    assert!(matches!(
        first.snapshot().await,
        Err(JobError::UnknownJob { .. })
    ));
    assert!(fourth.snapshot().await.is_ok());
}

/// Runs a command to completion and returns a handle to its retained session.
///
/// **The command is delayed on purpose.** A job handle exists only for a result that yielded, so
/// this has to see `Yielded` before it can wait for `Done` — and `printf x` against a 1 ms yield
/// timeout is a coin flip: when the supervisor reaps the child inside that millisecond the result is
/// `Completed`, there is no session to hand back, and the assertion in [`start_job_with`] fails for
/// reasons that have nothing to do with the test that called this. Measured at roughly one run in
/// ten. The delay makes yielding a certainty; every caller here is asking about what happens *after*
/// the command finishes, so none of them cares that it started 50 ms later.
async fn run_to_completion(manager: Arc<ProcessManager>, command: &str) -> BackgroundJob {
    let job = start_job_with(
        Arc::clone(&manager),
        &format!("sleep 0.05; {command}"),
        ExecLimits::new().with_initial_yield_timeout(Duration::from_millis(1)),
    )
    .await;
    let result = tokio::time::timeout(Duration::from_secs(5), job.wait(JobWaitUntil::Done))
        .await
        .expect("a short command finishes quickly")
        .expect("job is retained through its own closeout");
    assert!(matches!(result, JobWaitResult::Done(_)));
    job
}
