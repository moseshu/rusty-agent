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
    assert_eq!(snapshot.state(), &ExecSessionState::Exited { exit_code: Some(0) });
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
    let JobWaitResult::Matched { snapshot, stream } = result else {
        panic!("matching output must report a match: {result:?}");
    };
    assert_eq!(stream, ExecStreamKind::Stderr);
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

    assert!(matches!(job.snapshot().await, Err(JobError::UnknownJob { .. })));
    assert!(matches!(job.cancel().await, Err(JobError::UnknownJob { .. })));
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
    let JobWaitResult::Matched { snapshot, stream } = result else {
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
