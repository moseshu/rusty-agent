//! How long runs and large directories fare: recording stays off the caller's path and keeps up,
//! and a listing of the state database does not grow with the directory.
//!
//! The gates here use bounds an order of magnitude above what a debug build measures, so they
//! catch a write path that waits on the disk per record, or a listing that reads every rollout,
//! not a slow machine. The ignored measurements print the figures the development plan records;
//! run them in release:
//!
//! ```text
//! cargo test --release -p it-session-sqlite --test state_db_throughput -- --ignored --nocapture
//! ```

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use ra_core::{
    event::EventTimestamp,
    item::{AgentId, ItemId, Message, ModelInputItem, OutputPhase, RunItem, RunItemKind},
    session::{
        SessionId,
        rollout::{
            PersistContext, RolloutItem, RolloutModelUsage, RolloutRecorder, RolloutRunStarted,
            RolloutTurnContext,
        },
    },
    state::RunId,
    usage::{RequestUsage, Usage},
};
use ra_session::{
    CreateThreadParams, ListThreadsParams, RolloutSessionMeta, RolloutThreadDirectory,
    RolloutWriter, ThreadStore, store::local,
};

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir()
        .join("rusty_agent_tests")
        .join("state_db_throughput")
        .join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// The records of one turn of a long run: its start with the user's message, the turn context,
/// the answer and the model call's usage.
fn turn(index: usize, answer_len: usize) -> [RolloutItem; 4] {
    let run_id = RunId::new(format!("run-{index}"));
    [
        RolloutItem::RunStarted(
            RolloutRunStarted::new(run_id.clone(), AgentId::new("lead")).with_input(vec![
                ModelInputItem::Message(Message::user(format!("step {index}"))),
            ]),
        ),
        RolloutItem::TurnContext(
            RolloutTurnContext::new(run_id.clone(), 0)
                .with_model("gpt-5")
                .with_effort("high"),
        ),
        RolloutItem::Item(RunItem::new(
            ItemId::new(format!("answer-{index}")),
            RunItemKind::Message(Message::assistant(
                "x".repeat(answer_len),
                OutputPhase::Final,
            )),
        )),
        RolloutItem::ModelUsage(RolloutModelUsage::new(
            run_id,
            Usage::from_request(RequestUsage::new(1_000, 200)),
        )),
    ]
}

/// Records `turns` turns into a new thread of `store` and returns how long recording took the
/// caller and how long until the last record and its metadata were written.
async fn record_long_run(
    store: &RolloutThreadDirectory,
    turns: usize,
    answer_len: usize,
) -> (Duration, Duration) {
    let recorder: Arc<dyn RolloutRecorder> = store
        .create_thread_with(&CreateThreadParams::new(
            RolloutSessionMeta::new(SessionId::new("long-run"))
                .with_created_at(EventTimestamp::from_millis(1_000))
                .with_cwd("/work"),
        ))
        .await
        .unwrap();
    let started = Instant::now();
    for index in 0..turns {
        for item in turn(index, answer_len) {
            recorder.record(item);
        }
    }
    let recording = started.elapsed();
    recorder.persist(PersistContext::Standard).await.unwrap();
    let written = started.elapsed();
    recorder.shutdown().await.unwrap();
    (recording, written)
}

/// Writes `count` threads of `records` records each straight into rollout files.
async fn write_threads(dir: &Path, count: usize, records: usize) {
    for index in 0..count {
        let session_id = SessionId::new(format!("sess-{index:06}"));
        let path = RolloutThreadDirectory::new(dir)
            .rollout_path(&session_id)
            .unwrap();
        let mut writer = RolloutWriter::open(&path, session_id.clone())
            .await
            .unwrap();
        writer
            .append_session_meta(
                RolloutSessionMeta::new(session_id)
                    .with_created_at(EventTimestamp::from_millis(u64::try_from(index).unwrap()))
                    .with_cwd("/work")
                    .with_model_provider("openai"),
            )
            .await
            .unwrap();
        for item in turn(0, 500).into_iter().cycle().take(records) {
            writer.append(item).await.unwrap();
        }
        writer.flush().await.unwrap();
    }
}

async fn time_page(store: &RolloutThreadDirectory, params: &ListThreadsParams) -> Duration {
    let started = Instant::now();
    let page = store.list_threads(params).await.unwrap();
    assert_eq!(page.items().len(), params.page_size());
    started.elapsed()
}

/// A run of 1200 records, every fourth of which changes the thread's metadata, is recorded
/// without the caller waiting on the disk or the database, and written — rollout and rows — well
/// within the bound.
#[tokio::test(flavor = "multi_thread")]
async fn a_long_run_is_not_held_up_by_its_writes() {
    let dir = temp_dir("long_run");
    let db = local::try_init(&dir).await.unwrap();
    let store = RolloutThreadDirectory::new(&dir).with_state_db(db);
    let (recording, written) = record_long_run(&store, 300, 1_000).await;
    assert!(
        recording < Duration::from_millis(500),
        "recording waited on the writes: {recording:?}"
    );
    assert!(
        written < Duration::from_secs(20),
        "1200 records took {written:?} to write"
    );
}

/// A page of the state database costs the same whatever the directory holds; the scan of
/// rollouts reads the head of every one.
#[tokio::test(flavor = "multi_thread")]
async fn a_page_of_the_database_does_not_read_every_rollout() {
    let dir = temp_dir("listing");
    write_threads(&dir, 400, 4).await;
    let db = local::try_init(&dir).await.unwrap();
    let store = RolloutThreadDirectory::new(&dir).with_state_db(db);
    let params = ListThreadsParams::new(25);
    // Warm both paths once.
    time_page(&store, &params.clone().with_state_db_only()).await;
    time_page(&RolloutThreadDirectory::new(&dir), &params).await;

    let from_db = time_page(&store, &params.clone().with_state_db_only()).await;
    let scanned = time_page(&RolloutThreadDirectory::new(&dir), &params).await;
    assert!(
        from_db * 2 < scanned,
        "a page of the database took {from_db:?}, the scan of 400 rollouts {scanned:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "measurement; run in release with --nocapture"]
async fn measure_long_runs() {
    for turns in [250, 2_500] {
        let records = turns * 4;
        let plain_dir = temp_dir(&format!("measure_plain_{turns}"));
        let (recording, written) =
            record_long_run(&RolloutThreadDirectory::new(&plain_dir), turns, 1_000).await;
        println!(
            "{records} records, no database: recorded in {recording:?}, written in {written:?} \
             ({:?} per record)",
            written / u32::try_from(records).unwrap()
        );
        let dir = temp_dir(&format!("measure_db_{turns}"));
        let store =
            RolloutThreadDirectory::new(&dir).with_state_db(local::try_init(&dir).await.unwrap());
        let (recording, written) = record_long_run(&store, turns, 1_000).await;
        println!(
            "{records} records, state database: recorded in {recording:?}, written in \
             {written:?} ({:?} per record)",
            written / u32::try_from(records).unwrap()
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "measurement; run in release with --nocapture"]
async fn measure_listings() {
    for count in [1_000, 10_000] {
        let dir = temp_dir(&format!("measure_listing_{count}"));
        write_threads(&dir, count, 100).await;
        let started = Instant::now();
        let db = local::try_init(&dir).await.unwrap();
        println!("{count} threads: backfill {:?}", started.elapsed());
        let store = RolloutThreadDirectory::new(&dir).with_state_db(db);
        let plain = RolloutThreadDirectory::new(&dir);
        let params = ListThreadsParams::new(25);
        for _ in 0..3 {
            println!(
                "{count} threads: scan {:?}, scan and repair {:?}, database only {:?}",
                time_page(&plain, &params).await,
                time_page(&store, &params).await,
                time_page(&store, &params.clone().with_state_db_only()).await,
            );
        }
    }
}
