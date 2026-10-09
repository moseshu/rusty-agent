//! The gate a connection's requests pass through: Codex's `connection_rpc_gate.rs` unit tests,
//! run against the gate the crate's `test-api` door reaches.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use ra_protocol::server::test_api::RpcGate;
use tokio::{sync::oneshot, time::timeout};

#[tokio::test]
async fn run_executes_while_open() {
    let gate = RpcGate::new();
    let ran = Arc::new(AtomicBool::new(false));
    let ran_clone = Arc::clone(&ran);

    gate.run(async move {
        ran_clone.store(true, Ordering::Release);
    })
    .await;

    assert!(ran.load(Ordering::Acquire));
}

#[tokio::test]
async fn run_drops_future_without_polling_after_close() {
    let gate = RpcGate::new();
    gate.close().await;
    let polled = Arc::new(AtomicBool::new(false));
    let polled_clone = Arc::clone(&polled);

    gate.run(async move {
        polled_clone.store(true, Ordering::Release);
    })
    .await;

    assert!(!polled.load(Ordering::Acquire));
    assert!(!gate.is_accepting().await);
}

#[tokio::test]
async fn close_returns_while_started_run_remains_active() {
    let gate = Arc::new(RpcGate::new());
    let (started_tx, started_rx) = oneshot::channel();
    let (finish_tx, finish_rx) = oneshot::channel::<()>();
    let gate_for_run = Arc::clone(&gate);
    let run_task = tokio::spawn(async move {
        gate_for_run
            .run(async move {
                started_tx.send(()).expect("receiver should be open");
                let _ = finish_rx.await;
            })
            .await;
    });

    started_rx.await.expect("run should start");
    gate.close().await;
    assert!(!gate.is_accepting().await);
    assert_eq!(gate.inflight_count(), 1);

    finish_tx.send(()).expect("running future should be waiting");
    run_task.await.expect("run task should complete");
    gate.shutdown().await;
}

#[tokio::test]
async fn shutdown_waits_for_started_run_to_finish() {
    let gate = Arc::new(RpcGate::new());
    let (started_tx, started_rx) = oneshot::channel();
    let (finish_tx, finish_rx) = oneshot::channel::<()>();
    let gate_for_run = Arc::clone(&gate);
    let run_task = tokio::spawn(async move {
        gate_for_run
            .run(async move {
                started_tx.send(()).expect("receiver should be open");
                let _ = finish_rx.await;
            })
            .await;
    });

    started_rx.await.expect("run should start");
    assert_eq!(gate.inflight_count(), 1);

    let gate_for_shutdown = Arc::clone(&gate);
    let shutdown_task = tokio::spawn(async move {
        gate_for_shutdown.shutdown().await;
    });

    timeout(Duration::from_millis(50), shutdown_task)
        .await
        .expect_err("shutdown should wait for the running future");

    finish_tx.send(()).expect("running future should be waiting");
    run_task.await.expect("run task should complete");
    gate.shutdown().await;
    assert_eq!(gate.inflight_count(), 0);
}

#[tokio::test]
async fn shutdown_drops_late_runs_while_waiting_for_inflight_work() {
    let gate = Arc::new(RpcGate::new());
    let (started_tx, started_rx) = oneshot::channel();
    let (finish_tx, finish_rx) = oneshot::channel::<()>();
    let gate_for_run = Arc::clone(&gate);
    let run_task = tokio::spawn(async move {
        gate_for_run
            .run(async move {
                started_tx.send(()).expect("receiver should be open");
                let _ = finish_rx.await;
            })
            .await;
    });

    started_rx.await.expect("run should start");
    let gate_for_shutdown = Arc::clone(&gate);
    let shutdown_task = tokio::spawn(async move {
        gate_for_shutdown.shutdown().await;
    });

    timeout(Duration::from_millis(50), shutdown_task)
        .await
        .expect_err("shutdown should wait for the running future");

    let late_polled = Arc::new(AtomicBool::new(false));
    let late_polled_clone = Arc::clone(&late_polled);
    gate.run(async move {
        late_polled_clone.store(true, Ordering::Release);
    })
    .await;

    assert!(!late_polled.load(Ordering::Acquire));

    finish_tx
        .send(())
        .expect("running future should still be waiting");
    run_task.await.expect("run task should complete");
    gate.shutdown().await;
    assert_eq!(gate.inflight_count(), 0);
}

#[tokio::test]
async fn run_is_counted_before_handler_body_continues() {
    let gate = Arc::new(RpcGate::new());
    let (entered_tx, entered_rx) = oneshot::channel();
    let (continue_tx, continue_rx) = oneshot::channel::<()>();
    let gate_for_run = Arc::clone(&gate);
    let run_task = tokio::spawn(async move {
        gate_for_run
            .run(async move {
                entered_tx.send(()).expect("receiver should be open");
                let _ = continue_rx.await;
            })
            .await;
    });

    entered_rx.await.expect("handler body should be entered");
    assert_eq!(gate.inflight_count(), 1);

    continue_tx
        .send(())
        .expect("handler body should still be waiting");
    run_task.await.expect("run task should complete");
    assert_eq!(gate.inflight_count(), 0);
}
