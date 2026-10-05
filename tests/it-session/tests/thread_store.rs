//! The thread store: Codex's `ThreadStore` lifecycle and reads over the rollout directory and in
//! memory, and resuming a thread from it. Ported from Codex's local store tests
//! (`thread-store/src/local/mod.rs`): resuming reopens the live writer and appends, a second live
//! writer is refused until the first is shut down, discarding lets go of the writer, and shutting
//! down a thread that recorded nothing does not create its rollout.

use std::path::PathBuf;

use ra_core::{
    agent::control::AgentPath,
    error::{Error, SessionErrorKind},
    item::{AgentId, ItemId, Message, MessageRole, ModelInputItem, RunItem, RunItemKind},
    session::{
        SessionId,
        rollout::{
            RolloutItem, RolloutRecorder, RolloutRunEnd, RolloutRunEnded, RolloutRunStarted,
            RolloutThreadSpawn, RolloutThreadStore,
        },
    },
    state::RunId,
};
use ra_session::{
    InMemoryThreadStore, LoadThreadHistoryParams, ReadThreadParams, ResumeThreadParams,
    ResumedThread, RolloutFileRecorder, RolloutReader, RolloutSessionMeta, RolloutThreadDirectory,
    RolloutWriter, ThreadStore,
};
use serde_json::json;

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir()
        .join("rusty_agent_tests")
        .join("thread_store")
        .join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn spawn() -> RolloutThreadSpawn {
    RolloutThreadSpawn::new(
        SessionId::new("root"),
        SessionId::new("root"),
        1,
        AgentPath::root().join("worker").unwrap(),
    )
}

fn user(text: &str) -> ModelInputItem {
    ModelInputItem::Message(Message::user(text))
}

/// One completed run: its start on `text`, an answer, and its end.
fn record_run(recorder: &dyn RolloutRecorder, run: &str, text: &str) {
    let run_id = RunId::new(run);
    recorder.record(RolloutItem::RunStarted(
        RolloutRunStarted::new(run_id.clone(), AgentId::new("worker")).with_input(vec![user(text)]),
    ));
    recorder.record(RolloutItem::Item(RunItem::new(
        ItemId::new(format!("{run}-answer")),
        RunItemKind::Message(Message::text(
            MessageRole::Assistant,
            format!("{text}: done"),
        )),
    )));
    recorder.record(RolloutItem::RunEnded(RolloutRunEnded::new(
        run_id,
        RolloutRunEnd::Completed,
    )));
}

/// The error of a call that must fail; a recorder has no `Debug` for `unwrap_err` to show.
fn failure<T>(result: ra_core::error::Result<T>) -> Error {
    match result {
        Ok(_) => panic!("expected the call to fail"),
        Err(error) => error,
    }
}

fn is_kind(error: &Error, kind: SessionErrorKind) -> bool {
    matches!(error, Error::Session { kind: found, .. } if *found == kind)
}

fn types(store_records: &[ra_session::RolloutRecord]) -> Vec<&str> {
    store_records
        .iter()
        .map(ra_session::RolloutRecord::type_name)
        .collect()
}

// ---------------------------------------------------------------------------------------------
// The contract both stores keep
// ---------------------------------------------------------------------------------------------

async fn an_unknown_thread_is_not_found(store: &dyn ThreadStore) {
    let session_id = SessionId::new("nobody");
    let error = store
        .load_history(&LoadThreadHistoryParams::new(session_id.clone()))
        .await
        .unwrap_err();
    assert!(is_kind(&error, SessionErrorKind::NotFound), "{error}");
    let error = store
        .read_thread(&ReadThreadParams::new(session_id))
        .await
        .unwrap_err();
    assert!(is_kind(&error, SessionErrorKind::NotFound), "{error}");
}

async fn a_resumed_thread_appends_after_what_it_holds(store: &dyn ThreadStore) {
    let session_id = SessionId::new("worker-1");
    let recorder = store.create_thread(&session_id, &spawn()).await.unwrap();
    record_run(recorder.as_ref(), "run-1", "before resume");
    recorder.shutdown().await.unwrap();

    let resumed = store
        .resume_thread(&ResumeThreadParams::new(session_id.clone()))
        .await
        .unwrap();
    record_run(resumed.as_ref(), "run-2", "after resume");
    resumed.shutdown().await.unwrap();

    let history = store
        .load_history(&LoadThreadHistoryParams::new(session_id.clone()))
        .await
        .unwrap();
    assert_eq!(history.session_id(), &session_id);
    assert_eq!(
        types(history.records()),
        vec![
            "session_meta",
            "run_started",
            "item",
            "run_ended",
            "run_started",
            "item",
            "run_ended",
        ]
    );
    assert!(
        history
            .records()
            .windows(2)
            .all(|pair| pair[0].timeline_seq() < pair[1].timeline_seq())
    );
    let rebuilt = ra_session::reconstruct_history(history.records()).unwrap();
    assert_eq!(
        rebuilt.history(),
        vec![
            user("before resume"),
            ModelInputItem::Message(Message::text(MessageRole::Assistant, "before resume: done")),
            user("after resume"),
            ModelInputItem::Message(Message::text(MessageRole::Assistant, "after resume: done")),
        ]
    );
}

async fn a_thread_reads_back_with_its_metadata(store: &dyn ThreadStore) {
    let session_id = SessionId::new("worker-2");
    let recorder = store.create_thread(&session_id, &spawn()).await.unwrap();
    record_run(recorder.as_ref(), "run-1", "hello");
    recorder.shutdown().await.unwrap();

    let thread = store
        .read_thread(&ReadThreadParams::new(session_id.clone()))
        .await
        .unwrap();
    assert_eq!(thread.session_id(), &session_id);
    assert_eq!(thread.thread_spawn(), Some(&spawn()));
    assert_eq!(thread.parent_session_id(), Some(&SessionId::new("root")));
    assert!(thread.created_at().is_some());
    assert!(thread.history().is_none());

    let thread = store
        .read_thread(&ReadThreadParams::new(session_id.clone()).with_history())
        .await
        .unwrap();
    assert_eq!(thread.thread_spawn(), Some(&spawn()));
    let history = store
        .load_history(&LoadThreadHistoryParams::new(session_id))
        .await
        .unwrap();
    assert_eq!(thread.history(), Some(&history));
}

async fn a_shut_down_or_discarded_recorder_writes_nothing_more(store: &dyn ThreadStore) {
    let session_id = SessionId::new("worker-3");
    let recorder = store.create_thread(&session_id, &spawn()).await.unwrap();
    record_run(recorder.as_ref(), "run-1", "kept");
    recorder.shutdown().await.unwrap();
    record_run(recorder.as_ref(), "run-2", "after shutdown");
    assert!(recorder.flush().await.is_err());

    let resumed = store
        .resume_thread(&ResumeThreadParams::new(session_id.clone()))
        .await
        .unwrap();
    resumed.discard().await.unwrap();
    record_run(resumed.as_ref(), "run-3", "after discard");
    assert!(resumed.flush().await.is_err());

    let history = store
        .load_history(&LoadThreadHistoryParams::new(session_id))
        .await
        .unwrap();
    assert_eq!(
        types(history.records()),
        vec!["session_meta", "run_started", "item", "run_ended"]
    );
}

async fn resuming_rebuilds_the_history_and_reopens_the_thread(store: &dyn ThreadStore) {
    let session_id = SessionId::new("worker-4");
    let recorder = store.create_thread(&session_id, &spawn()).await.unwrap();
    record_run(recorder.as_ref(), "run-1", "first");
    recorder.shutdown().await.unwrap();

    let resumed = ResumedThread::resume(store, &ResumeThreadParams::new(session_id.clone()))
        .await
        .unwrap();
    assert_eq!(resumed.history().records().len(), 4);
    let last = resumed.reconstruction().last_run().unwrap();
    assert_eq!(last.run_id(), &RunId::new("run-1"));
    assert_eq!(last.end().unwrap().end(), RolloutRunEnd::Completed);
    assert_eq!(resumed.reconstruction().history().len(), 2);

    record_run(resumed.recorder().as_ref(), "run-2", "second");
    resumed.recorder().shutdown().await.unwrap();
    let history = store
        .load_history(&LoadThreadHistoryParams::new(session_id))
        .await
        .unwrap();
    assert_eq!(history.records().len(), 7);
}

async fn contract(store: &dyn ThreadStore) {
    an_unknown_thread_is_not_found(store).await;
    a_resumed_thread_appends_after_what_it_holds(store).await;
    a_thread_reads_back_with_its_metadata(store).await;
    resuming_rebuilds_the_history_and_reopens_the_thread(store).await;
}

#[tokio::test]
async fn the_rollout_directory_keeps_the_thread_store_contract() {
    let store = RolloutThreadDirectory::new(temp_dir("contract"));
    contract(&store).await;
    a_shut_down_or_discarded_recorder_writes_nothing_more(&store).await;
}

#[tokio::test]
async fn the_in_memory_store_keeps_the_thread_store_contract() {
    contract(&InMemoryThreadStore::new()).await;
}

// ---------------------------------------------------------------------------------------------
// The rollout directory
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_thread_has_one_live_writer_until_it_is_shut_down_or_discarded() {
    let directory = RolloutThreadDirectory::new(temp_dir("one_live_writer"));
    let session_id = SessionId::new("worker-1");
    let params = ResumeThreadParams::new(session_id.clone());
    let primary = directory
        .create_thread(&session_id, &spawn())
        .await
        .unwrap();
    record_run(primary.as_ref(), "run-1", "hello");
    primary.flush().await.unwrap();

    // A second directory over the same files stands in for another process.
    let secondary = RolloutThreadDirectory::new(directory.path());
    let error = failure(secondary.resume_thread(&params).await);
    assert!(error.to_string().contains("another writer"), "{error}");

    primary.shutdown().await.unwrap();
    let resumed = secondary.resume_thread(&params).await.unwrap();
    let error = failure(directory.resume_thread(&params).await);
    assert!(error.to_string().contains("another writer"), "{error}");

    resumed.discard().await.unwrap();
    directory.resume_thread(&params).await.unwrap();
}

#[tokio::test]
async fn shutting_down_a_thread_that_recorded_nothing_does_not_create_its_rollout() {
    let directory = RolloutThreadDirectory::new(temp_dir("nothing_recorded"));
    let session_id = SessionId::new("worker-1");
    let recorder = directory
        .create_thread(&session_id, &spawn())
        .await
        .unwrap();
    recorder.flush().await.unwrap();
    recorder.shutdown().await.unwrap();

    assert!(!directory.rollout_path(&session_id).unwrap().exists());
    let error = failure(
        directory
            .resume_thread(&ResumeThreadParams::new(session_id))
            .await,
    );
    assert!(is_kind(&error, SessionErrorKind::NotFound), "{error}");
}

#[tokio::test]
async fn a_thread_read_from_the_directory_names_its_rollout_and_may_lack_metadata() {
    let directory = RolloutThreadDirectory::new(temp_dir("root_without_meta"));
    let session_id = SessionId::new("root");
    let path = directory.rollout_path(&session_id).unwrap();
    // A root thread whose host created its recorder without session metadata.
    let recorder = RolloutFileRecorder::create(&path, session_id.clone());
    record_run(&recorder, "run-1", "hello");
    recorder.shutdown().await.unwrap();

    let thread = directory
        .read_thread(&ReadThreadParams::new(session_id))
        .await
        .unwrap();
    assert_eq!(thread.rollout_path(), Some(path.as_path()));
    assert_eq!(thread.thread_spawn(), None);
    assert_eq!(thread.created_at(), None);
}

#[tokio::test]
async fn a_thread_whose_history_cannot_be_rebuilt_discards_its_reopened_writer() {
    let directory = RolloutThreadDirectory::new(temp_dir("unrebuildable"));
    let session_id = SessionId::new("root");
    let path = directory.rollout_path(&session_id).unwrap();
    let mut writer = RolloutWriter::open(&path, session_id.clone())
        .await
        .unwrap();
    writer
        .append_session_meta(RolloutSessionMeta::new(session_id.clone()))
        .await
        .unwrap();
    writer.flush().await.unwrap();
    drop(writer);
    // A run start whose payload this build cannot read.
    let seq = RolloutReader::open(&path).read_all().await.unwrap().len();
    let line = json!({
        "timeline_seq": seq,
        "at": 0,
        "type": "run_started",
        "payload": {"run_id": 7},
    });
    let mut contents = std::fs::read_to_string(&path).unwrap();
    contents.push_str(&format!("{line}\n"));
    std::fs::write(&path, contents).unwrap();

    let error = ResumedThread::resume(&directory, &ResumeThreadParams::new(session_id.clone()))
        .await
        .unwrap_err();
    assert!(is_kind(&error, SessionErrorKind::Corrupted), "{error}");
    // Nothing holds the rollout: it can be opened for writing at once.
    RolloutWriter::open(&path, session_id).await.unwrap();
}
