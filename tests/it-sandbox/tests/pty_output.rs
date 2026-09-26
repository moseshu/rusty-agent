//! `ra-sandbox::pty_output`: gathering what an interactive process printed while a caller waited.
//!
//! Ported from the reference's `tests/sandbox/test_pty_output.py`, one test per upstream test,
//! plus the deadline and the budget those two leave implicit.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use ra_sandbox::pty_output::{PtyOutputBuffer, collect_pty_output};

// `test_collect_pty_output_waits_for_notification`
#[tokio::test]
async fn a_wait_ends_when_output_arrives_and_the_process_is_done() {
    let buffer = Arc::new(PtyOutputBuffer::new());
    let done = Arc::new(AtomicBool::new(false));

    let producer = {
        let buffer = Arc::clone(&buffer);
        let done = Arc::clone(&done);
        tokio::spawn(async move {
            tokio::task::yield_now().await;
            buffer.push(b"notified output".to_vec());
            done.store(true, Ordering::SeqCst);
            buffer.wake();
        })
    };
    let (output, original_token_count) =
        collect_pty_output(&buffer, || done.load(Ordering::SeqCst), 500, None).await;
    producer.await.expect("producer");

    assert_eq!(output, b"notified output");
    assert_eq!(original_token_count, None);
}

// `test_collect_pty_output_drains_chunks_added_when_done`
#[tokio::test]
async fn output_pushed_while_done_is_being_asked_is_still_returned() {
    let buffer = PtyOutputBuffer::new();
    buffer.push(b"before done".to_vec());

    let (output, original_token_count) = collect_pty_output(
        &buffer,
        || {
            buffer.push(b" after done".to_vec());
            true
        },
        500,
        None,
    )
    .await;

    assert_eq!(output, b"before done after done");
    assert_eq!(original_token_count, None);
}

// Beyond the upstream file.

#[tokio::test]
async fn a_wait_with_nothing_arriving_ends_at_the_deadline() {
    let buffer = PtyOutputBuffer::new();
    let started = Instant::now();

    let (output, original_token_count) = collect_pty_output(&buffer, || false, 200, None).await;

    assert!(output.is_empty());
    assert_eq!(original_token_count, None);
    let elapsed = started.elapsed();
    assert!(elapsed >= Duration::from_millis(200), "{elapsed:?}");
    assert!(elapsed < Duration::from_secs(2), "{elapsed:?}");
}

#[tokio::test]
async fn collected_output_is_cut_to_the_token_budget() {
    let buffer = PtyOutputBuffer::new();
    buffer.push(b"stdout: pwd\n".to_vec());
    buffer.push(b"stderr: pwd".to_vec());

    let (output, original_token_count) = collect_pty_output(&buffer, || true, 500, Some(2)).await;

    assert_eq!(String::from_utf8(output).expect("utf-8"), "…6 tok");
    assert_eq!(original_token_count, Some(6));
}

/// Bytes that are not UTF-8 come back replaced, as the reference decodes before it cuts.
#[tokio::test]
async fn invalid_utf8_is_replaced_even_without_a_cut() {
    let buffer = PtyOutputBuffer::new();
    buffer.push(vec![b'a', 0xff, b'b']);

    let (output, _) = collect_pty_output(&buffer, || true, 500, None).await;

    assert_eq!(String::from_utf8(output).expect("utf-8"), "a\u{fffd}b");
}
