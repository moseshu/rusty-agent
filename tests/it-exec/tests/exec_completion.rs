//! Arrival capture and completion ownership through the public process API.
#![cfg(unix)]
use ra_core::{
    event::{HostEventBody, HostEventEmitter, InMemoryHostEventSink, exec::ExecEvent},
    item::AgentId,
    state::{RunId, RunState},
};
use ra_exec::{
    command::{ExecLimits, ExecRequest},
    output::{HeadTailBuffer, omission_marker},
    session::{ExecSessionId, ProcessManager},
};
use std::{sync::Arc, time::Duration};

fn request(command: &str) -> ExecRequest {
    ExecRequest::new(command)
        .with_shell(Some("/bin/sh"))
        .with_call_id("original")
}
async fn background(manager: &ProcessManager) -> ExecSessionId {
    let result = manager
        .execute_observed(
            request(
                "while read l; do case $l in o*) echo $l;; e*) echo $l >&2;; q) exit;; esac; done",
            )
            .with_limits(ExecLimits::new().with_initial_yield_timeout(Duration::from_millis(10))),
            None,
            manager.environment(),
        )
        .await
        .unwrap();
    assert!(result.completion().is_none());
    result.result().session_id().unwrap().clone()
}
async fn send(
    manager: &ProcessManager,
    id: &ExecSessionId,
    line: &str,
    stdout: usize,
    stderr: usize,
) {
    manager
        .write_stdin(id, Some(line), false, None)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let summary = manager.get_output_summary(id).await.unwrap();
            if summary.stdout_bytes() == stdout && summary.stderr_bytes() == stderr {
                break;
            }
            manager
                .wait_for_output(
                    id,
                    summary.stdout_bytes() as u64,
                    summary.stderr_bytes() as u64,
                    Duration::from_millis(10),
                )
                .await
                .unwrap();
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn arrival_order_is_controlled_by_observed_pipe_boundaries() {
    for (first, second, expected) in [("e1\n", "o2\n", "e1\no2\n"), ("o1\n", "e2\n", "o1\ne2\n")] {
        let manager = ProcessManager::default();
        let id = background(&manager).await;
        let guard = manager.lock_interaction(&id).await.unwrap();
        assert_eq!(guard.cursors().await, (0, 0));
        send(
            &manager,
            &id,
            first,
            usize::from(first.starts_with('o')) * 3,
            usize::from(first.starts_with('e')) * 3,
        )
        .await;
        send(&manager, &id, second, 3, 3).await;
        manager
            .write_stdin(&id, Some("q\n"), false, None)
            .await
            .unwrap();
        assert!(manager.wait_for_close(&id).await);
        let delivery = guard.deliver(true).await;
        let completion = delivery.completion().unwrap();
        assert_eq!(completion.output(), Some(expected));
        assert_eq!(completion.total_bytes(), 6);
        assert_eq!(completion.omitted_bytes(), 0);
        assert_eq!(guard.cursors().await, (3, 3));
        assert_eq!(completion.origin_call_id(), Some("original"));
        assert!(guard.deliver(true).await.completion().is_none());
    }
}

#[tokio::test]
async fn delivery_advances_both_projections_and_false_snapshot_does_not_claim() {
    let manager = ProcessManager::default();
    let id = background(&manager).await;
    let guard = manager.lock_interaction(&id).await.unwrap();
    send(&manager, &id, "e1\n", 0, 3).await;
    let first = guard.deliver(false).await;
    assert_eq!(first.stderr().unwrap().text(), "e1\n");
    assert!(first.completion().is_none());
    send(&manager, &id, "o2\n", 3, 3).await;
    manager
        .write_stdin(&id, Some("q\n"), false, None)
        .await
        .unwrap();
    assert!(manager.wait_for_close(&id).await);
    let second = guard.deliver(false).await;
    assert_eq!(second.stdout().unwrap().text(), "o2\n");
    assert!(second.stderr().is_none());
    assert_eq!(guard.cursors().await, (3, 3));
    let last = guard.deliver(true).await;
    assert_eq!(last.completion().unwrap().output(), Some(""));
    assert_eq!(last.completion().unwrap().total_bytes(), 0);
}

#[tokio::test]
async fn held_guard_claims_after_release() {
    let manager = ProcessManager::default();
    let id = background(&manager).await;
    send(&manager, &id, "e1\n", 0, 3).await;
    manager
        .write_stdin(&id, Some("q\n"), false, None)
        .await
        .unwrap();
    assert!(manager.wait_for_close(&id).await);
    let guard = manager.lock_interaction(&id).await.unwrap();
    manager.release(&id).await;
    assert!(manager.terminal(&id).await.is_none());
    let delivery = guard.deliver(true).await;
    assert_eq!(delivery.completion().unwrap().output(), Some("e1\n"));
    assert_eq!(
        delivery.completion().unwrap().origin_call_id(),
        Some("original")
    );
}

#[tokio::test]
async fn synchronous_report_survives_later_registry_eviction() {
    let manager = ProcessManager::new(ExecLimits::new().with_max_sessions(1));
    let first = manager
        .execute_observed(request("printf first"), None, manager.environment())
        .await
        .unwrap();
    for _ in 0..3 {
        manager.execute(request("true"), None).await.unwrap();
    }
    let completion = first.completion().unwrap();
    assert!(manager.terminal(completion.session_id()).await.is_none());
    assert_eq!(completion.output(), Some("first"));
    assert_eq!(completion.origin_call_id(), Some("original"));
}

#[tokio::test]
async fn synchronous_claim_happens_after_deterministic_release() {
    let manager = Arc::new(ProcessManager::default());
    let reached = Arc::new(tokio::sync::Notify::new());
    let resume = Arc::new(tokio::sync::Notify::new());
    manager.test_pause_completion(reached.clone(), resume.clone());
    let sink = Arc::new(InMemoryHostEventSink::new());
    let state = RunState::start(RunId::new("run"));
    let emitter = HostEventEmitter::new(
        AgentId::new("agent"),
        state.restore_event_seq_allocator(None),
        sink.clone(),
    );
    let task = tokio::spawn({
        let manager = manager.clone();
        async move {
            manager
                .execute_observed(
                    request("printf detached"),
                    Some(&emitter),
                    manager.environment(),
                )
                .await
                .unwrap()
        }
    });
    tokio::time::timeout(Duration::from_secs(5), reached.notified())
        .await
        .unwrap();
    let id = sink
        .events()
        .iter()
        .find_map(|event| match event.body() {
            HostEventBody::Exec(ExecEvent::Started(started)) => Some(started.session_id().clone()),
            _ => None,
        })
        .unwrap();
    manager.release(&id).await;
    assert!(manager.terminal(&id).await.is_none());
    resume.notify_one();
    let result = task.await.unwrap();
    let completion = result.completion().unwrap();
    assert_eq!(completion.session_id(), &id);
    assert_eq!(completion.output(), Some("detached"));
    assert_eq!(completion.origin_call_id(), Some("original"));
}

#[test]
fn byte_projection_decodes_after_joining_and_keeps_real_gaps() {
    let source = "aaa中".as_bytes();
    for chunks in [
        vec![source],
        vec![&source[..4], &source[4..5], &source[5..]],
    ] {
        let mut buffer = HeadTailBuffer::new(8);
        for chunk in chunks {
            buffer.push_bytes(chunk);
        }
        let (bytes, next) = buffer.test_read_bytes_from(0).unwrap();
        assert_eq!(String::from_utf8_lossy(&bytes), "aaa中");
        assert_eq!(next, 6);
    }
    let mut buffer = HeadTailBuffer::new(8);
    let source = "aaa中012345中".as_bytes();
    buffer.push_bytes(source);
    for offset in [0, 4, 6, 12] {
        let tail_start = source.len() - 4;
        let mut expected = source[offset.min(4)..4].to_vec();
        let omitted = tail_start.saturating_sub(offset.max(4));
        if omitted > 0 {
            expected.extend_from_slice(format!("\n{}\n", omission_marker(omitted)).as_bytes());
        }
        expected.extend_from_slice(&source[offset.max(tail_start)..]);
        let (bytes, next) = buffer.test_read_bytes_from(offset).unwrap();
        assert_eq!(bytes, expected);
        assert_eq!(next, source.len());
    }
}

#[tokio::test]
async fn observed_utf8_and_empty_capture_have_output() {
    let manager = ProcessManager::new(ExecLimits::new().with_max_capture_bytes(8));
    for (command, expected) in [("printf 'aaa中'", "aaa中"), ("true", "")] {
        let result = manager
            .execute_observed(request(command), None, manager.environment())
            .await
            .unwrap();
        assert_eq!(result.completion().unwrap().output(), Some(expected));
    }
    let result = manager
        .execute_observed(
            request("read l; printf 'aaa中'").with_limits(
                ExecLimits::new().with_initial_yield_timeout(Duration::from_millis(10)),
            ),
            None,
            manager.environment(),
        )
        .await
        .unwrap();
    let id = result.result().session_id().unwrap();
    manager
        .write_stdin(id, Some("q\n"), false, None)
        .await
        .unwrap();
    manager.wait_for_close(id).await;
    let guard = manager.lock_interaction(id).await.unwrap();
    let delivery = guard.deliver(true).await;
    assert_eq!(delivery.completion().unwrap().output(), Some("aaa中"));
}

#[tokio::test]
async fn completion_span_starts_after_the_sum_of_delivered_stream_positions() {
    let manager = ProcessManager::default();
    let id = background(&manager).await;
    let guard = manager.lock_interaction(&id).await.unwrap();
    send(&manager, &id, "e1\n", 0, 3).await;
    assert!(guard.deliver(false).await.completion().is_none());
    send(&manager, &id, "o2\n", 3, 3).await;
    manager
        .write_stdin(&id, Some("q\n"), false, None)
        .await
        .unwrap();
    manager.wait_for_close(&id).await;
    let delivery = guard.deliver(true).await;
    assert!(delivery.stderr().is_none());
    assert_eq!(delivery.stdout().unwrap().text(), "o2\n");
    let completion = delivery.completion().unwrap();
    assert_eq!(completion.output(), Some("o2\n"));
    assert_eq!(completion.total_bytes(), 3);
    assert_eq!(guard.cursors().await, (3, 3));
}

#[tokio::test]
async fn observed_truncation_decodes_only_after_inserting_the_gap_bytes() {
    let manager = ProcessManager::new(ExecLimits::new().with_max_capture_bytes(8));
    let source = "aaa中012345中".as_bytes();
    let mut expected = source[..4].to_vec();
    expected.extend_from_slice(format!("\n{}\n", omission_marker(source.len() - 8)).as_bytes());
    expected.extend_from_slice(&source[source.len() - 4..]);
    let result = manager
        .execute_observed(
            request("printf 'aaa中012345中'"),
            None,
            manager.environment(),
        )
        .await
        .unwrap();
    let completion = result.completion().unwrap();
    assert_eq!(
        completion.output(),
        Some(String::from_utf8_lossy(&expected).as_ref())
    );
    assert_eq!(completion.total_bytes(), source.len());
    assert_eq!(completion.omitted_bytes(), source.len() - 8);
}
