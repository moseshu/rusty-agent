//! Behaviour of the head/tail capture buffer and of the process manager that drives real children.

use std::{sync::Arc, time::Duration};

use ra_core::{
    event::{HostEventEmitter, InMemoryHostEventSink, exec::ExecEvictionReason},
    item::AgentId,
    state::{RunId, RunState},
};
use ra_exec::{
    command::{ExecCursor, ExecLimits, ExecRequest},
    output::HeadTailBuffer,
    session::{ExecExecutionResult, ExecSessionState, ProcessManager, session_resource_id},
};

/// Anything that should be immediate is given this much room before the test calls it a hang.
const RESPONSIVE: Duration = Duration::from_secs(5);

fn emitter(sink: &Arc<InMemoryHostEventSink>) -> HostEventEmitter {
    let state = RunState::start(RunId::new("run-exec-test"));
    let allocator = state.restore_event_seq_allocator(None);
    HostEventEmitter::new(AgentId::new("tester"), allocator, sink.clone())
}

fn quick_yield() -> ExecLimits {
    ExecLimits::new().with_initial_yield_timeout(Duration::from_millis(100))
}

async fn yielded(
    manager: &ProcessManager,
    request: ExecRequest,
) -> ra_exec::session::ExecSessionId {
    match manager
        .execute(request, None)
        .await
        .expect("command starts")
    {
        ExecExecutionResult::Yielded { session_id, .. } => session_id,
        ExecExecutionResult::Completed(summary) => {
            panic!("expected the command to yield, it finished: {summary:?}")
        }
        other => panic!("expected the command to yield: {other:?}"),
    }
}

#[test]
fn test_head_tail_buffer_within_capacity() {
    let mut buffer = HeadTailBuffer::new(100);
    assert_eq!(buffer.capacity(), 100);
    assert_eq!(buffer.total_bytes(), 0);
    assert!(!buffer.is_truncated());
    assert_eq!(buffer.omitted_bytes(), 0);

    buffer.push_bytes(b"hello world\n");
    assert_eq!(buffer.total_bytes(), 12);
    assert_eq!(buffer.retained_bytes(), 12);
    assert!(!buffer.is_truncated());
    assert_eq!(buffer.to_string_lossy(), "hello world\n");
    assert_eq!(buffer.to_retained_bytes(), b"hello world\n");
}

#[test]
fn test_head_tail_buffer_truncation() {
    let mut buffer = HeadTailBuffer::new(20);
    buffer.push_bytes(b"0123456789");
    assert!(!buffer.is_truncated());

    buffer.push_bytes(b"abcdefghij");
    assert!(!buffer.is_truncated());
    assert_eq!(buffer.total_bytes(), 20);

    buffer.push_bytes(b"KLMNOPQRST");
    assert_eq!(buffer.total_bytes(), 30);
    assert!(buffer.is_truncated());
    assert_eq!(buffer.omitted_bytes(), 10);
    // Retained counts source bytes, so a truncation record does not credit the marker text.
    assert_eq!(buffer.retained_bytes(), 20);

    let formatted = buffer.to_string_lossy();
    assert!(formatted.contains("0123456789"));
    assert!(formatted.contains("KLMNOPQRST"));
    assert!(formatted.contains("[omitted 10 bytes]"));
}

#[test]
fn test_head_tail_buffer_read_from_reports_the_gap_and_ends() {
    let mut buffer = HeadTailBuffer::new(20);
    buffer.push_bytes(b"0123456789");

    let (text, next) = buffer.read_from(0).expect("output past the start");
    assert_eq!(text, "0123456789");
    assert_eq!(next, 10);
    // Nothing new since the cursor is where the stream ends.
    assert!(buffer.read_from(next).is_none());

    buffer.push_bytes(b"abcdefghijKLMNOPQRST");
    // The head is full, so the whole push competes for the ten byte tail: "abcdefghij" is dropped.
    let (text, next) = buffer.read_from(10).expect("output past the first read");
    // The cursor lands inside bytes the buffer dropped: the gap is announced, and the cursor jumps
    // over it instead of advancing by the length of a string that stands in for missing output.
    assert!(
        text.contains("[omitted 10 bytes]"),
        "unexpected text: {text}"
    );
    assert!(text.contains("KLMNOPQRST"));
    assert_eq!(next, 30);
    assert!(buffer.read_from(next).is_none());
}

/// A record written before retained bytes were tracked must not claim nothing survived.
#[test]
fn test_output_summary_without_retained_bytes_falls_back_to_its_text() {
    let stored = serde_json::json!({
        "schema_version": 1,
        "stdout": "eight ch",
        "stderr": "four",
        "stdout_bytes": 8,
        "stderr_bytes": 4,
        "duration_ms": 5,
        "is_truncated": false
    });
    let summary: ra_exec::output::ExecOutputSummary =
        serde_json::from_value(stored).expect("an older record still reads");

    assert_eq!(summary.total_bytes(), 12);
    // Not zero: the text it carries is right there, and a truncation replayed from this record
    // would otherwise report that a command with output retained none of it.
    assert_eq!(summary.retained_bytes(), 12);
}

#[test]
fn test_session_resource_identity() {
    let session_id = ra_core::event::exec::ExecSessionId::new("session-123");
    let resource_id = session_resource_id(&session_id).expect("resource id derived");
    assert_eq!(resource_id.to_string(), "process:session-123");
}

#[tokio::test]
async fn test_process_manager_execute_simple_command() {
    let manager = ProcessManager::default();
    let result = manager
        .execute(ExecRequest::new("echo 'hello rust'"), None)
        .await
        .expect("execution succeeds");

    match result {
        ExecExecutionResult::Completed(summary) => {
            assert_eq!(summary.stdout().trim(), "hello rust");
            assert!(summary.stderr().is_empty());
            assert_eq!(summary.exit_code(), Some(0));
            assert!(!summary.is_truncated());
        }
        other => panic!("expected a completed result: {other:?}"),
    }
}

#[tokio::test]
async fn test_process_manager_execute_with_exit_code() {
    let manager = ProcessManager::default();
    let result = manager
        .execute(ExecRequest::new("exit 42"), None)
        .await
        .expect("execution succeeds");

    assert_eq!(result.summary().exit_code(), Some(42));
}

#[tokio::test]
async fn test_process_manager_execute_with_stderr() {
    let manager = ProcessManager::default();
    let result = manager
        .execute(ExecRequest::new("echo 'an error occurred' >&2"), None)
        .await
        .expect("execution succeeds");

    let summary = result.summary();
    assert!(summary.stdout().is_empty());
    assert_eq!(summary.stderr().trim(), "an error occurred");
    assert_eq!(summary.exit_code(), Some(0));
}

#[tokio::test]
async fn test_process_manager_passes_arguments_to_the_shell() {
    let manager = ProcessManager::default();
    let request = ExecRequest::new("echo \"$1-$2\"").with_args(["sh", "left", "right"]);
    let result = manager
        .execute(request, None)
        .await
        .expect("execution succeeds");

    assert_eq!(result.summary().stdout().trim(), "left-right");
}

#[tokio::test]
async fn test_process_manager_honours_a_requested_shell() {
    let manager = ProcessManager::default();
    let request = ExecRequest::new("echo $0").with_shell(Some("/bin/sh"));
    let result = manager
        .execute(request, None)
        .await
        .expect("execution succeeds");

    assert_eq!(result.summary().stdout().trim(), "/bin/sh");
}

/// A command that finishes long before the yield timeout must be reported as finished.
///
/// The exit is announced through a notification that only reaches waiters already registered, so a
/// process quick enough to exit before this call started listening used to leave it waiting out the
/// whole yield window and then calling a finished command "still running". A multi-threaded runtime
/// is what lets the supervisor actually win that race.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_process_manager_does_not_miss_a_fast_exit() {
    let manager = ProcessManager::default();
    let limits = ExecLimits::new().with_initial_yield_timeout(Duration::from_secs(30));

    for _ in 0_u8..20 {
        let request = ExecRequest::new("true").with_limits(limits.clone());
        let result = tokio::time::timeout(RESPONSIVE, manager.execute(request, None))
            .await
            .expect("a command that exits at once must not wait out the yield window")
            .expect("execution succeeds");
        assert!(
            matches!(result, ExecExecutionResult::Completed(_)),
            "a finished command was reported as still running: {result:?}"
        );
    }
}

/// Output events carry text, and a character split across two reads is still one character.
///
/// A pipe read stops at whatever byte the kernel had ready, which for a stream of three-byte
/// characters lands mid-character most of the time. Decoding each read on its own turned those into
/// replacement characters — visible in the UI, and never in the captured output, so the two
/// disagreed about what the command had printed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_process_manager_emits_whole_characters_across_reads() {
    let manager = ProcessManager::default();
    let sink = Arc::new(InMemoryHostEventSink::new());
    let emitter = emitter(&sink);

    let request = ExecRequest::new("for i in $(seq 1 500); do printf '一二三四五六七八九十'; done");
    let result = manager
        .execute(request, Some(&emitter))
        .await
        .expect("execution succeeds");

    let captured = result.summary().stdout().to_owned();
    assert_eq!(captured.chars().count(), 5000);

    let streamed: String = sink
        .events()
        .iter()
        .filter_map(|event| match event.body() {
            ra_core::event::HostEventBody::Exec(ra_core::event::exec::ExecEvent::Output(out)) => {
                Some(out.text().to_owned())
            }
            _ => None,
        })
        .collect();
    assert!(
        !streamed.contains('\u{fffd}'),
        "a character was split across two events"
    );
    assert_eq!(streamed, captured);
}

/// Collects the text of every output event for a session, in order.
fn streamed_text(sink: &Arc<InMemoryHostEventSink>) -> String {
    sink.events()
        .iter()
        .filter_map(|event| match event.body() {
            ra_core::event::HostEventBody::Exec(ra_core::event::exec::ExecEvent::Output(out)) => {
                Some(out.text().to_owned())
            }
            _ => None,
        })
        .collect()
}

/// One byte that is not text must not cost the event stream everything printed after it.
///
/// Decoding stopped at the first invalid sequence and returned only the valid prefix, dropping the
/// rest of that read. The captured output kept all of it, so the events and the summary stopped
/// describing the same command.
#[tokio::test]
async fn test_process_manager_keeps_output_after_an_invalid_byte() {
    let manager = ProcessManager::default();
    let sink = Arc::new(InMemoryHostEventSink::new());
    let emitter = emitter(&sink);

    // `A`, one byte that begins no valid character, then `B`.
    let result = manager
        .execute(ExecRequest::new(r"printf 'A\377B'"), Some(&emitter))
        .await
        .expect("execution succeeds");

    let streamed = streamed_text(&sink);
    assert!(
        streamed.starts_with('A'),
        "text before the bad byte was lost"
    );
    assert!(streamed.ends_with('B'), "text after the bad byte was lost");
    assert_eq!(streamed, result.summary().stdout());
}

/// A stream that ends mid-character still delivers those bytes.
///
/// The decoder holds back an incomplete character in case the rest arrives in the next read. At end
/// of stream nothing more is coming, and without a flush those bytes reached the capture buffer but
/// no event.
#[tokio::test]
async fn test_process_manager_flushes_a_partial_character_at_end_of_stream() {
    let manager = ProcessManager::default();
    let sink = Arc::new(InMemoryHostEventSink::new());
    let emitter = emitter(&sink);

    // The first two bytes of a three-byte character, and nothing after them.
    let result = manager
        .execute(ExecRequest::new(r"printf 'start\344\275'"), Some(&emitter))
        .await
        .expect("execution succeeds");

    let streamed = streamed_text(&sink);
    assert!(streamed.starts_with("start"));
    assert_eq!(streamed, result.summary().stdout());
}

#[tokio::test]
async fn test_process_manager_yields_on_long_running_command() {
    let manager = ProcessManager::default();
    let sink = Arc::new(InMemoryHostEventSink::new());
    let emitter = emitter(&sink);

    let request = ExecRequest::new("sleep 30; echo done").with_limits(quick_yield());
    let result = manager
        .execute(request, Some(&emitter))
        .await
        .expect("execution started");

    let ExecExecutionResult::Yielded { session_id, .. } = result else {
        panic!("expected the command to yield: {result:?}");
    };
    assert!(manager.active_sessions().await.contains(&session_id));

    let yielded = sink.events().iter().any(|event| {
        matches!(
            event.body(),
            ra_core::event::HostEventBody::Exec(ra_core::event::exec::ExecEvent::Yielded(y))
                if y.session_id() == &session_id
        )
    });
    assert!(yielded, "the yield was not recorded in the event stream");

    tokio::time::timeout(RESPONSIVE, manager.cancel(&session_id))
        .await
        .expect("cancelling must not wait for the process it is cancelling");
    assert_eq!(
        manager.get_session_state(&session_id).await,
        Some(ExecSessionState::Cancelled)
    );
}

/// Cancelling a process that never exits on its own must return immediately.
///
/// `cat` with an open standard input runs until something stops it. Holding the child across its
/// own `wait()` made every path that wanted to kill it block until it had already died, so this
/// call hung for as long as the process did — which for `cat` is forever.
#[tokio::test]
async fn test_process_manager_cancels_a_process_that_never_exits() {
    let manager = ProcessManager::default();
    let session_id = yielded(&manager, ExecRequest::new("cat").with_limits(quick_yield())).await;

    tokio::time::timeout(RESPONSIVE, manager.cancel(&session_id))
        .await
        .expect("cancelling a running process must not wait for it to exit");

    assert_eq!(
        manager.get_session_state(&session_id).await,
        Some(ExecSessionState::Cancelled)
    );
    // The slot is released when the process is actually gone, which is shortly after — not at the
    // moment the signal was sent.
    let deadline = std::time::Instant::now() + RESPONSIVE;
    while manager.active_sessions().await.contains(&session_id) {
        assert!(
            std::time::Instant::now() < deadline,
            "a cancelled session never released its slot"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn test_process_manager_stdin_and_cursor_read() {
    let manager = ProcessManager::default();
    let session_id = yielded(&manager, ExecRequest::new("cat").with_limits(quick_yield())).await;

    manager
        .write_stdin(&session_id, Some("interactive line 1\n"), false, None)
        .await
        .expect("write succeeds");
    tokio::time::sleep(Duration::from_millis(100)).await;

    let (text, next_cursor) = manager
        .read_output(&session_id, ExecCursor::stdout_start())
        .await
        .expect("reading output succeeds")
        .expect("the echoed line has arrived");
    assert!(text.contains("interactive line 1"));
    assert_eq!(next_cursor.offset(), text.len() as u64);

    // A second read from the returned cursor has nothing to add.
    assert!(
        manager
            .read_output(&session_id, next_cursor)
            .await
            .expect("reading output succeeds")
            .is_none()
    );

    tokio::time::timeout(RESPONSIVE, manager.cancel(&session_id))
        .await
        .expect("cancel is prompt");
}

/// The interactive delivery mark only ever advances, and never past what the session produced.
///
/// Both halves are load bearing. The mark is what one caller — the model's `write_stdin`
/// conversation — reads to learn which bytes it has not seen; a mark that could move backward would
/// repeat output already delivered, and one that could name a future offset would skip output that
/// had not arrived yet. Neither is reachable through the tool today, which is exactly why the
/// guarantee is asserted here rather than left to the one caller that happens to behave.
#[tokio::test]
async fn test_the_interactive_delivery_mark_advances_and_stays_within_produced_output() {
    let manager = ProcessManager::default();
    let session_id = yielded(&manager, ExecRequest::new("cat").with_limits(quick_yield())).await;

    manager
        .write_stdin(&session_id, Some("marked line\n"), false, None)
        .await
        .expect("write succeeds");
    tokio::time::sleep(Duration::from_millis(100)).await;
    let produced = manager
        .get_output_summary(&session_id)
        .await
        .expect("the session is retained")
        .stdout_bytes() as u64;
    assert!(produced > 0, "the echoed line has arrived");

    // Beyond what exists: clamped to what the session actually produced.
    manager
        .mark_interactive_output_delivered(&session_id, produced + 4_096, 0)
        .await
        .expect("marking succeeds");
    assert_eq!(
        manager
            .interactive_output_cursors(&session_id)
            .await
            .expect("cursors are readable"),
        (produced, 0)
    );

    // Backward: refused in favour of the position already reached.
    manager
        .mark_interactive_output_delivered(&session_id, 0, 0)
        .await
        .expect("marking succeeds");
    assert_eq!(
        manager
            .interactive_output_cursors(&session_id)
            .await
            .expect("cursors are readable"),
        (produced, 0)
    );

    tokio::time::timeout(RESPONSIVE, manager.cancel(&session_id))
        .await
        .expect("cancel is prompt");
}

/// A combined cursor cannot be resumed from, so it is refused rather than answered wrongly.
#[tokio::test]
async fn test_process_manager_refuses_a_combined_cursor() {
    let manager = ProcessManager::default();
    let session_id = yielded(&manager, ExecRequest::new("cat").with_limits(quick_yield())).await;

    let error = manager
        .read_output(
            &session_id,
            ExecCursor::new(ra_core::event::exec::ExecStreamKind::Combined, 0),
        )
        .await
        .expect_err("a combined cursor has no single position");
    assert!(matches!(
        error,
        ra_exec::session::ExecError::UnaddressableStream { .. }
    ));

    tokio::time::timeout(RESPONSIVE, manager.cancel(&session_id))
        .await
        .expect("cancel is prompt");
}

#[tokio::test]
async fn test_process_manager_eviction() {
    let manager = ProcessManager::default();
    let session_id = yielded(
        &manager,
        ExecRequest::new("sleep 30").with_limits(quick_yield()),
    )
    .await;

    tokio::time::timeout(
        RESPONSIVE,
        manager.evict(&session_id, ExecEvictionReason::CapacityExceeded),
    )
    .await
    .expect("eviction is prompt");

    assert_eq!(
        manager.get_session_state(&session_id).await,
        Some(ExecSessionState::Expired {
            reason: ExecEvictionReason::CapacityExceeded
        })
    );
}

/// A finished session leaves the active set but stays readable.
///
/// Both halves matter: a manager that never retired anything filled its capacity with commands that
/// had already exited and then evicted a corpse to make room for every new one, and a manager that
/// forgot them entirely could not answer for the command it had just run.
#[tokio::test]
async fn test_process_manager_retires_finished_sessions() {
    let manager = ProcessManager::default();
    let request = ExecRequest::new("echo retired").with_limits(quick_yield());
    let result = manager
        .execute(request, None)
        .await
        .expect("execution succeeds");
    assert!(matches!(result, ExecExecutionResult::Completed(_)));

    assert!(manager.active_sessions().await.is_empty());
}

/// Capacity counts what is running, and evicting to make room is what it does about it.
#[tokio::test]
async fn test_process_manager_evicts_the_oldest_running_session_at_capacity() {
    let limits = ExecLimits::new()
        .with_max_sessions(2)
        .with_initial_yield_timeout(Duration::from_millis(100));
    let manager = ProcessManager::new(limits.clone());

    let first = yielded(
        &manager,
        ExecRequest::new("sleep 30").with_limits(limits.clone()),
    )
    .await;
    let second = yielded(
        &manager,
        ExecRequest::new("sleep 30").with_limits(limits.clone()),
    )
    .await;
    let third = yielded(&manager, ExecRequest::new("sleep 30").with_limits(limits)).await;

    let active = manager.active_sessions().await;
    assert!(active.len() <= 2, "capacity was exceeded: {active:?}");
    assert!(!active.contains(&first));
    assert_eq!(
        manager.get_session_state(&first).await,
        Some(ExecSessionState::Expired {
            reason: ExecEvictionReason::CapacityExceeded
        })
    );

    for session_id in [second, third] {
        tokio::time::timeout(RESPONSIVE, manager.cancel(&session_id))
            .await
            .expect("cancel is prompt");
    }
}

/// The hard timeout a caller asks for is the one the process gets.
#[tokio::test]
async fn test_process_manager_stops_a_command_at_its_total_timeout() {
    let manager = ProcessManager::default();
    let limits = ExecLimits::new()
        .with_initial_yield_timeout(Duration::from_millis(100))
        .with_total_timeout(Some(Duration::from_millis(300)));
    let session_id = yielded(&manager, ExecRequest::new("sleep 30").with_limits(limits)).await;

    let deadline = std::time::Instant::now() + RESPONSIVE;
    loop {
        let state = manager.get_session_state(&session_id).await;
        if state
            .as_ref()
            .is_some_and(ra_exec::session::ExecSessionState::is_terminal)
        {
            assert_eq!(
                state,
                Some(ExecSessionState::Expired {
                    reason: ExecEvictionReason::TotalTimeout
                })
            );
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the total timeout never took the process away"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Whether a process id still names a live process.
fn is_alive(pid: &str) -> bool {
    std::process::Command::new("kill")
        .args(["-0", pid])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

async fn wait_until_gone(pid: &str) {
    let deadline = std::time::Instant::now() + RESPONSIVE;
    while is_alive(pid) {
        assert!(
            std::time::Instant::now() < deadline,
            "process {pid} outlived the session that started it"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Terminating a session takes the whole process group, including what declines to go quietly.
///
/// The background shell ignores `SIGTERM` outright, and the leader does not — so the leader dies on
/// the first signal and the wait ends while the group is still populated. Escalating to the leader
/// alone, or not escalating once it is reaped, leaves that shell running forever: the orphan the
/// cancellation contract exists to prevent.
#[tokio::test]
async fn test_process_manager_terminates_the_whole_process_group() {
    let manager = ProcessManager::default();
    let request =
        ExecRequest::new(r#"sh -c 'trap "" TERM; while :; do sleep 1; done' & echo $!; wait"#)
            .with_limits(quick_yield());
    let session_id = yielded(&manager, request).await;

    // The background pid is printed before the shell blocks, so it has arrived by the yield.
    let background = manager
        .get_output_summary(&session_id)
        .await
        .expect("the session is readable")
        .stdout()
        .trim()
        .to_owned();
    assert!(
        !background.is_empty(),
        "the command did not report its child"
    );
    assert!(is_alive(&background), "the child never started");

    tokio::time::timeout(RESPONSIVE, manager.cancel(&session_id))
        .await
        .expect("cancel is prompt");
    wait_until_gone(&background).await;
}

/// Writing to a process that never reads must not block anything else about the session.
///
/// The pipe fills, the write parks, and with the session's own lock held that would have frozen
/// every reader of this session — including the cancellation that is the only way out.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_process_manager_stdin_write_does_not_block_cancellation() {
    let manager = Arc::new(ProcessManager::default());
    let session_id = yielded(
        &manager,
        ExecRequest::new("sleep 30").with_limits(quick_yield()),
    )
    .await;

    // Far more than any pipe buffer, sent to a command that will never read a byte of it.
    let payload = "x".repeat(1024 * 1024);
    let writer = {
        let manager = Arc::clone(&manager);
        let session_id = session_id.clone();
        tokio::spawn(async move {
            let _ = manager
                .write_stdin(&session_id, Some(&payload), false, None)
                .await;
        })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;

    tokio::time::timeout(RESPONSIVE, manager.get_session_state(&session_id))
        .await
        .expect("session state stays readable while a write is parked");
    tokio::time::timeout(RESPONSIVE, manager.cancel(&session_id))
        .await
        .expect("cancelling must not wait behind a parked write");

    let _ = tokio::time::timeout(RESPONSIVE, writer).await;
}

/// An idle background command is taken away on its own, with nothing scheduling a sweep.
#[tokio::test]
async fn test_process_manager_expires_an_idle_session() {
    let manager = ProcessManager::default();
    let limits = ExecLimits::new()
        .with_initial_yield_timeout(Duration::from_millis(100))
        .with_idle_timeout(Some(Duration::from_millis(300)));
    let session_id = yielded(&manager, ExecRequest::new("sleep 30").with_limits(limits)).await;

    let deadline = std::time::Instant::now() + RESPONSIVE;
    loop {
        let state = manager.get_session_state(&session_id).await;
        if state
            .as_ref()
            .is_some_and(ra_exec::session::ExecSessionState::is_terminal)
        {
            assert_eq!(
                state,
                Some(ExecSessionState::Expired {
                    reason: ExecEvictionReason::IdleTimeout
                })
            );
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the idle timeout never fired"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Output is activity: a command that keeps talking is not idle, however long it runs.
#[tokio::test]
async fn test_process_manager_keeps_a_talking_session_alive() {
    let manager = ProcessManager::default();
    let limits = ExecLimits::new()
        .with_initial_yield_timeout(Duration::from_millis(100))
        .with_idle_timeout(Some(Duration::from_millis(400)));
    let request = ExecRequest::new("while :; do echo tick; sleep 0.1; done").with_limits(limits);
    let session_id = yielded(&manager, request).await;

    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_eq!(
        manager.get_session_state(&session_id).await,
        Some(ExecSessionState::Running),
        "a session producing output was treated as idle"
    );

    tokio::time::timeout(RESPONSIVE, manager.cancel(&session_id))
        .await
        .expect("cancel is prompt");
}

/// A session dies once, of one cause, and the event stream says so exactly once.
///
/// The command ignores `SIGTERM`, so its eviction takes the full grace period — and its total
/// timeout expires inside that window. The deadline arriving second must not overwrite the reason
/// that already stopped the session, nor add a second eviction event disagreeing with the first.
#[tokio::test]
async fn test_process_manager_reports_one_death_with_one_cause() {
    let manager = ProcessManager::default();
    let sink = Arc::new(InMemoryHostEventSink::new());
    let emitter = emitter(&sink);
    let limits = ExecLimits::new()
        .with_initial_yield_timeout(Duration::from_millis(100))
        // Expires while the eviction below is still waiting out the drain grace.
        .with_total_timeout(Some(Duration::from_millis(700)));
    let request =
        ExecRequest::new(r#"trap "" TERM; echo $$; while :; do sleep 1; done"#).with_limits(limits);

    let result = manager
        .execute(request, Some(&emitter))
        .await
        .expect("command starts");
    let ExecExecutionResult::Yielded { session_id, .. } = result else {
        panic!("expected the command to yield: {result:?}");
    };
    let pid = manager
        .get_output_summary(&session_id)
        .await
        .expect("the session is readable")
        .stdout()
        .trim()
        .to_owned();

    manager
        .evict(&session_id, ExecEvictionReason::HostShutdown)
        .await;
    wait_until_gone(&pid).await;
    // Let the supervisor finish its bookkeeping after the process is reaped.
    let deadline = std::time::Instant::now() + RESPONSIVE;
    while !manager.active_sessions().await.is_empty() {
        assert!(
            std::time::Instant::now() < deadline,
            "the session never retired"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    assert_eq!(
        manager.get_session_state(&session_id).await,
        Some(ExecSessionState::Expired {
            reason: ExecEvictionReason::HostShutdown
        }),
        "a deadline that expired during the drain overwrote the recorded cause"
    );

    let reasons: Vec<ExecEvictionReason> = sink
        .events()
        .iter()
        .filter_map(|event| match event.body() {
            ra_core::event::HostEventBody::Exec(ra_core::event::exec::ExecEvent::Evicted(
                evicted,
            )) if evicted.session_id() == &session_id => Some(evicted.reason().clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        reasons,
        vec![ExecEvictionReason::HostShutdown],
        "the event stream gave the same session more than one cause of death"
    );
}

/// A stopped session holds its slot until its process is gone, not until the signal is sent.
///
/// These commands ignore `SIGTERM`, so eviction takes the full grace period before the group is
/// killed. Freeing the slot on the request instead of on the reap let a caller starting sessions in
/// a loop keep "making room" against processes that had not gone anywhere, and run arbitrarily many
/// of them at once.
#[tokio::test]
async fn test_process_manager_capacity_counts_sessions_that_are_still_dying() {
    let limits = ExecLimits::new()
        .with_max_sessions(2)
        .with_initial_yield_timeout(Duration::from_millis(100));
    let manager = ProcessManager::new(limits.clone());
    let stubborn = r#"trap "" TERM; echo $$; while :; do sleep 1; done"#;

    let mut started = Vec::new();
    for _ in 0_u8..2 {
        let session_id = yielded(
            &manager,
            ExecRequest::new(stubborn).with_limits(limits.clone()),
        )
        .await;
        let pid = manager
            .get_output_summary(&session_id)
            .await
            .expect("the session is readable")
            .stdout()
            .trim()
            .to_owned();
        assert!(is_alive(&pid), "a session's process never started");
        started.push((session_id, pid));
    }

    // Admitting this one has to take the first session away, and that one will not go until it is
    // killed — so this call waits rather than starting a third process beside two live ones.
    let third = yielded(
        &manager,
        ExecRequest::new(stubborn).with_limits(limits.clone()),
    )
    .await;
    let third_pid = manager
        .get_output_summary(&third)
        .await
        .expect("the session is readable")
        .stdout()
        .trim()
        .to_owned();

    let (evicted_id, evicted_pid) = started[0].clone();
    assert_eq!(
        manager.get_session_state(&evicted_id).await,
        Some(ExecSessionState::Expired {
            reason: ExecEvictionReason::CapacityExceeded
        })
    );
    assert!(
        !is_alive(&evicted_pid),
        "the third session started while the evicted one was still running"
    );

    let live = [&started[1].1, &third_pid]
        .iter()
        .filter(|pid| is_alive(pid))
        .count();
    assert!(live <= 2, "{live} process groups were running at once");

    // Left running, these outlive the runtime that supervises them, so wait for them to be gone.
    for session_id in [started[1].0.clone(), third] {
        manager.cancel(&session_id).await;
    }
    for pid in [started[1].1.clone(), third_pid] {
        wait_until_gone(&pid).await;
    }
}

/// A single slot stays a single slot, even when its holder needs the whole drain window to go.
///
/// `max_sessions` of one is the case where the admission arithmetic has no headroom at all: the
/// caller must wait for the one running command to be gone before its own can start, and the
/// command here ignores `SIGTERM` so that wait is the full escalation rather than an instant.
#[tokio::test]
async fn test_process_manager_holds_a_single_slot_through_a_full_drain() {
    let limits = ExecLimits::new()
        .with_max_sessions(1)
        .with_initial_yield_timeout(Duration::from_millis(100));
    let manager = ProcessManager::new(limits.clone());
    let stubborn = r#"trap "" TERM; echo $$; while :; do sleep 1; done"#;

    let first = yielded(
        &manager,
        ExecRequest::new(stubborn).with_limits(limits.clone()),
    )
    .await;
    let first_pid = manager
        .get_output_summary(&first)
        .await
        .expect("the session is readable")
        .stdout()
        .trim()
        .to_owned();
    assert!(is_alive(&first_pid));

    let second = yielded(
        &manager,
        ExecRequest::new(stubborn).with_limits(limits.clone()),
    )
    .await;
    assert!(
        !is_alive(&first_pid),
        "the second session started while the first was still running"
    );
    let active = manager.active_sessions().await;
    assert_eq!(active.len(), 1, "capacity was exceeded: {active:?}");

    let second_pid = manager
        .get_output_summary(&second)
        .await
        .expect("the session is readable")
        .stdout()
        .trim()
        .to_owned();
    manager.cancel(&second).await;
    wait_until_gone(&second_pid).await;
}

/// The refusal names the ceiling it could not get under, rather than a session identifier.
///
/// Reaching this state for real needs a process that survives `SIGKILL` for seconds, which no
/// portable command can produce, so what is pinned here is the answer's shape.
#[test]
fn test_at_capacity_error_reports_the_ceiling() {
    let error = ra_exec::session::ExecError::AtCapacity { max_sessions: 4 };
    let message = error.to_string();
    assert!(message.contains('4'), "unexpected message: {message}");
    assert!(
        message.contains("have not ended"),
        "unexpected message: {message}"
    );
}

/// The manager's limits are a ceiling: a request may ask for less, never for more.
#[test]
fn test_exec_limits_only_tighten() {
    let ceiling = ExecLimits::new()
        .with_initial_yield_timeout(Duration::from_secs(10))
        .with_max_capture_bytes(1024)
        .with_idle_timeout(Some(Duration::from_secs(300)))
        .with_total_timeout(None);

    let greedy = ExecLimits::new()
        .with_initial_yield_timeout(Duration::from_secs(60))
        .with_max_capture_bytes(1024 * 1024)
        .with_idle_timeout(Some(Duration::from_secs(3600)))
        .with_total_timeout(None)
        .tightened_by(&ceiling);
    assert_eq!(greedy.initial_yield_timeout(), Duration::from_secs(10));
    assert_eq!(greedy.max_capture_bytes(), 1024);
    assert_eq!(greedy.idle_timeout(), Some(Duration::from_secs(300)));

    let modest = ExecLimits::new()
        .with_initial_yield_timeout(Duration::from_secs(1))
        .with_max_capture_bytes(64)
        .with_idle_timeout(Some(Duration::from_secs(5)))
        // A limit where the ceiling has none is still a limit.
        .with_total_timeout(Some(Duration::from_secs(30)))
        .tightened_by(&ceiling);
    assert_eq!(modest.initial_yield_timeout(), Duration::from_secs(1));
    assert_eq!(modest.max_capture_bytes(), 64);
    assert_eq!(modest.idle_timeout(), Some(Duration::from_secs(5)));
    assert_eq!(modest.total_timeout(), Some(Duration::from_secs(30)));
}

/// The manager's own limits reach the process, rather than being decoration on the constructor.
#[tokio::test]
async fn test_process_manager_limits_bound_a_request_that_asks_for_more() {
    let manager = ProcessManager::new(
        ExecLimits::new()
            .with_initial_yield_timeout(Duration::from_millis(100))
            .with_total_timeout(Some(Duration::from_millis(300))),
    );
    // Asks for an hour; the manager allows 300ms.
    let request = ExecRequest::new("sleep 30").with_limits(
        ExecLimits::new()
            .with_initial_yield_timeout(Duration::from_millis(100))
            .with_total_timeout(Some(Duration::from_secs(3600))),
    );
    let session_id = yielded(&manager, request).await;

    let deadline = std::time::Instant::now() + RESPONSIVE;
    loop {
        let state = manager.get_session_state(&session_id).await;
        if state
            .as_ref()
            .is_some_and(ra_exec::session::ExecSessionState::is_terminal)
        {
            assert_eq!(
                state,
                Some(ExecSessionState::Expired {
                    reason: ExecEvictionReason::TotalTimeout
                })
            );
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the manager's ceiling never reached the process"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// A command that fails to start is a failure with a name, not a session left in limbo.
#[tokio::test]
async fn test_process_manager_reports_a_shell_that_does_not_exist() {
    let manager = ProcessManager::default();
    let request = ExecRequest::new("echo hi").with_shell(Some("/nonexistent/shell"));

    let error = manager
        .execute(request, None)
        .await
        .expect_err("a missing shell cannot run a command");
    assert!(matches!(error, ra_exec::session::ExecError::Spawn { .. }));
    assert!(manager.active_sessions().await.is_empty());
}
