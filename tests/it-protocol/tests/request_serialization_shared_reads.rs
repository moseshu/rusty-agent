// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Codex's shared-read cases of `request_serialization.rs`: consecutive reads of one key run
//! together, a read that arrives while a batch runs joins it, and no read overtakes a queued write.
//!
//! The startup case uses `config/read` where Codex uses `configRequirements/read`; both declare a
//! shared read of the `config` queue, and requirements reads are not served by this product yet.

use ra_protocol::frame::RequestId;
use ra_protocol::server::protocol::{
    ClientRequest, ConfigBatchWriteParams, ConfigReadParams, HooksListParams,
};
use ra_protocol::server::test_api::{SerializationAccess, SerializationQueues};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::time::{Duration, timeout};

const FIRST_REQUEST_VALUE: i32 = 1;
const SECOND_REQUEST_VALUE: i32 = 2;
const THIRD_REQUEST_VALUE: i32 = 3;

fn queue_drain_timeout() -> Duration {
    Duration::from_secs(/*secs*/ 1)
}

fn shutdown_wait_timeout() -> Duration {
    Duration::from_millis(/*millis*/ 50)
}

#[tokio::test]
async fn same_key_shared_reads_run_concurrently() {
    let queues = SerializationQueues::default();
    let (blocker_started_tx, blocker_started_rx) = oneshot::channel::<()>();
    let (blocker_release_tx, blocker_release_rx) = oneshot::channel::<()>();
    let (started_tx, mut started_rx) = mpsc::unbounded_channel();
    let (release_tx, _) = broadcast::channel::<()>(/*capacity*/ 1);

    queues
        .enqueue("test", SerializationAccess::Exclusive, async move {
            blocker_started_tx
                .send(())
                .expect("receiver should be open");
            let _ = blocker_release_rx.await;
        })
        .await;
    timeout(queue_drain_timeout(), blocker_started_rx)
        .await
        .expect("blocker should start")
        .expect("sender should be open");

    for value in [FIRST_REQUEST_VALUE, SECOND_REQUEST_VALUE] {
        let started_tx = started_tx.clone();
        let mut release_rx = release_tx.subscribe();
        queues
            .enqueue("test", SerializationAccess::SharedRead, async move {
                started_tx.send(value).expect("receiver should be open");
                let _ = release_rx.recv().await;
            })
            .await;
    }
    drop(started_tx);
    blocker_release_tx
        .send(())
        .expect("blocker should still be waiting");

    let mut started = Vec::new();
    for _ in 0..2 {
        started.push(
            timeout(queue_drain_timeout(), started_rx.recv())
                .await
                .expect("timed out waiting for shared read")
                .expect("sender should be open"),
        );
    }
    assert_eq!(started, vec![FIRST_REQUEST_VALUE, SECOND_REQUEST_VALUE]);

    release_tx
        .send(())
        .expect("shared reads should still be waiting");
}

#[tokio::test]
async fn later_shared_read_joins_running_shared_read() {
    let queues = SerializationQueues::default();
    let (first_started_tx, first_started_rx) = oneshot::channel::<()>();
    let (first_release_tx, first_release_rx) = oneshot::channel::<()>();
    let (later_started_tx, later_started_rx) = oneshot::channel::<()>();

    queues
        .enqueue("test", SerializationAccess::SharedRead, async move {
            first_started_tx.send(()).expect("receiver should be open");
            let _ = first_release_rx.await;
        })
        .await;
    timeout(queue_drain_timeout(), first_started_rx)
        .await
        .expect("first read should start")
        .expect("sender should be open");

    queues
        .enqueue("test", SerializationAccess::SharedRead, async move {
            later_started_tx.send(()).expect("receiver should be open");
        })
        .await;

    timeout(queue_drain_timeout(), later_started_rx)
        .await
        .expect("later read should join the running read")
        .expect("sender should be open");
    first_release_tx
        .send(())
        .expect("first read should still be waiting");
}

#[tokio::test]
async fn later_shared_read_waits_behind_writer_queued_during_running_read() {
    let queues = SerializationQueues::default();
    let (first_read_started_tx, first_read_started_rx) = oneshot::channel::<()>();
    let (first_read_release_tx, first_read_release_rx) = oneshot::channel::<()>();
    let (write_started_tx, write_started_rx) = oneshot::channel::<()>();
    let (write_release_tx, write_release_rx) = oneshot::channel::<()>();
    let (later_read_started_tx, later_read_started_rx) = oneshot::channel::<()>();

    queues
        .enqueue("test", SerializationAccess::SharedRead, async move {
            first_read_started_tx
                .send(())
                .expect("receiver should be open");
            let _ = first_read_release_rx.await;
        })
        .await;
    timeout(queue_drain_timeout(), first_read_started_rx)
        .await
        .expect("first read should start")
        .expect("sender should be open");

    queues
        .enqueue("test", SerializationAccess::Exclusive, async move {
            write_started_tx.send(()).expect("receiver should be open");
            let _ = write_release_rx.await;
        })
        .await;
    queues
        .enqueue("test", SerializationAccess::SharedRead, async move {
            later_read_started_tx
                .send(())
                .expect("receiver should be open");
        })
        .await;

    let mut write_started_rx = Box::pin(write_started_rx);
    timeout(shutdown_wait_timeout(), &mut write_started_rx)
        .await
        .expect_err("write should wait for the running read");
    let mut later_read_started_rx = Box::pin(later_read_started_rx);
    timeout(shutdown_wait_timeout(), &mut later_read_started_rx)
        .await
        .expect_err("later read should wait behind the queued write");

    first_read_release_tx
        .send(())
        .expect("first read should still be waiting");
    timeout(queue_drain_timeout(), &mut write_started_rx)
        .await
        .expect("write should start after the running read finishes")
        .expect("sender should be open");
    timeout(shutdown_wait_timeout(), &mut later_read_started_rx)
        .await
        .expect_err("later read should wait for the running write");

    write_release_tx
        .send(())
        .expect("write should still be waiting");
    timeout(queue_drain_timeout(), &mut later_read_started_rx)
        .await
        .expect("later read should start after the write finishes")
        .expect("sender should be open");
}

#[tokio::test]
async fn startup_config_reads_run_concurrently_and_exclude_writes() {
    let queues = SerializationQueues::default();
    let (blocker_started_tx, blocker_started_rx) = oneshot::channel::<()>();
    let (blocker_release_tx, blocker_release_rx) = oneshot::channel::<()>();
    let (read_started_tx, mut read_started_rx) = mpsc::unbounded_channel();
    let (read_release_tx, _) = broadcast::channel::<()>(/*capacity*/ 1);
    let (write_started_tx, write_started_rx) = oneshot::channel::<()>();

    queues
        .enqueue("config", SerializationAccess::Exclusive, async move {
            blocker_started_tx
                .send(())
                .expect("receiver should be open");
            let _ = blocker_release_rx.await;
        })
        .await;
    timeout(queue_drain_timeout(), blocker_started_rx)
        .await
        .expect("blocker should start")
        .expect("sender should be open");

    for (value, request) in [
        (
            FIRST_REQUEST_VALUE,
            ClientRequest::HooksList {
                request_id: RequestId::Integer(i64::from(FIRST_REQUEST_VALUE)),
                params: HooksListParams { cwds: Vec::new() },
            },
        ),
        (
            SECOND_REQUEST_VALUE,
            ClientRequest::ConfigRead {
                request_id: RequestId::Integer(i64::from(SECOND_REQUEST_VALUE)),
                params: ConfigReadParams {
                    include_layers: false,
                    cwd: None,
                },
            },
        ),
    ] {
        let read_started_tx = read_started_tx.clone();
        let mut read_release_rx = read_release_tx.subscribe();
        assert!(
            queues
                .enqueue_request(&request, async move {
                    read_started_tx
                        .send(value)
                        .expect("receiver should be open");
                    let _ = read_release_rx.recv().await;
                })
                .await,
            "startup config reads should be serialized"
        );
    }

    let config_write = ClientRequest::ConfigBatchWrite {
        request_id: RequestId::Integer(i64::from(THIRD_REQUEST_VALUE)),
        params: ConfigBatchWriteParams {
            edits: Vec::new(),
            file_path: None,
            expected_version: None,
            reload_user_config: false,
        },
    };
    assert!(
        queues
            .enqueue_request(&config_write, async move {
                write_started_tx.send(()).expect("receiver should be open");
            })
            .await,
        "config writes should be serialized"
    );
    drop(read_started_tx);
    blocker_release_tx
        .send(())
        .expect("blocker should still be waiting");

    for _ in 0..2 {
        timeout(queue_drain_timeout(), read_started_rx.recv())
            .await
            .expect("timed out waiting for shared read")
            .expect("sender should be open");
    }
    let mut write_started_rx = Box::pin(write_started_rx);
    timeout(shutdown_wait_timeout(), &mut write_started_rx)
        .await
        .expect_err("write should wait for running shared reads");

    read_release_tx
        .send(())
        .expect("shared reads should still be waiting");
    timeout(queue_drain_timeout(), &mut write_started_rx)
        .await
        .expect("write should start after shared reads finish")
        .expect("sender should be open");
}

#[tokio::test]
async fn later_shared_reads_do_not_jump_ahead_of_queued_write() {
    let queues = SerializationQueues::default();
    let (blocker_started_tx, blocker_started_rx) = oneshot::channel::<()>();
    let (blocker_release_tx, blocker_release_rx) = oneshot::channel::<()>();
    let (first_read_started_tx, first_read_started_rx) = oneshot::channel::<()>();
    let (first_read_release_tx, first_read_release_rx) = oneshot::channel::<()>();
    let (write_started_tx, write_started_rx) = oneshot::channel::<()>();
    let (write_release_tx, write_release_rx) = oneshot::channel::<()>();
    let (later_read_started_tx, later_read_started_rx) = oneshot::channel::<()>();

    queues
        .enqueue("test", SerializationAccess::Exclusive, async move {
            blocker_started_tx
                .send(())
                .expect("receiver should be open");
            let _ = blocker_release_rx.await;
        })
        .await;
    timeout(queue_drain_timeout(), blocker_started_rx)
        .await
        .expect("blocker should start")
        .expect("sender should be open");

    queues
        .enqueue("test", SerializationAccess::SharedRead, async move {
            first_read_started_tx
                .send(())
                .expect("receiver should be open");
            let _ = first_read_release_rx.await;
        })
        .await;
    queues
        .enqueue("test", SerializationAccess::Exclusive, async move {
            write_started_tx.send(()).expect("receiver should be open");
            let _ = write_release_rx.await;
        })
        .await;
    queues
        .enqueue("test", SerializationAccess::SharedRead, async move {
            later_read_started_tx
                .send(())
                .expect("receiver should be open");
        })
        .await;
    blocker_release_tx
        .send(())
        .expect("blocker should still be waiting");

    timeout(queue_drain_timeout(), first_read_started_rx)
        .await
        .expect("first read should start")
        .expect("sender should be open");
    let mut write_started_rx = Box::pin(write_started_rx);
    timeout(shutdown_wait_timeout(), &mut write_started_rx)
        .await
        .expect_err("write should wait for the first read");
    let mut later_read_started_rx = Box::pin(later_read_started_rx);
    timeout(shutdown_wait_timeout(), &mut later_read_started_rx)
        .await
        .expect_err("later read should wait behind the queued write");

    first_read_release_tx
        .send(())
        .expect("first read should still be waiting");
    timeout(queue_drain_timeout(), &mut write_started_rx)
        .await
        .expect("write should start after the first read")
        .expect("sender should be open");
    timeout(shutdown_wait_timeout(), &mut later_read_started_rx)
        .await
        .expect_err("later read should still wait while the write is running");

    write_release_tx
        .send(())
        .expect("write should still be waiting");
    timeout(queue_drain_timeout(), &mut later_read_started_rx)
        .await
        .expect("later read should start after the write")
        .expect("sender should be open");
}
