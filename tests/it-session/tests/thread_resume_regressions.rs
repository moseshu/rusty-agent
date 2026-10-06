//! Resume ownership, persistence and in-memory history semantics from Codex's thread store.

use std::{path::PathBuf, sync::Arc, time::Duration};

use async_trait::async_trait;
use ra_core::{
    agent::control::AgentPath,
    error::{Error, Result},
    item::{AgentId, Message, ModelInputItem},
    session::{
        SessionId,
        rollout::{
            RolloutItem, RolloutRecorder, RolloutRunStarted, RolloutThreadSpawn, RolloutThreadStore,
        },
    },
    state::RunId,
};
use ra_session::{
    ArchiveThreadParams, CreateThreadParams, DeleteThreadParams, InMemoryThreadStore,
    ListThreadsParams, LoadThreadHistoryParams, ReadThreadParams, ResumeThreadParams,
    ResumedThread, RolloutFileRecorder, RolloutReader, RolloutSessionMeta, RolloutThreadDirectory,
    StoredThread, StoredThreadHistory, ThreadPage, ThreadStore, UpdateThreadMetadataParams,
};

fn directory(name: &str) -> RolloutThreadDirectory {
    let path: PathBuf = std::env::temp_dir()
        .join("rusty_agent_tests/thread_resume_regressions")
        .join(name);
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();
    RolloutThreadDirectory::new(path)
}

fn spawn() -> RolloutThreadSpawn {
    RolloutThreadSpawn::new(
        SessionId::new("root"),
        SessionId::new("root"),
        1,
        AgentPath::root().join("worker").unwrap(),
    )
}

fn record(recorder: &dyn RolloutRecorder, run: &str, text: &str) {
    recorder.record(RolloutItem::RunStarted(
        RolloutRunStarted::new(RunId::new(run), AgentId::new("lead"))
            .with_input(vec![ModelInputItem::Message(Message::user(text))]),
    ));
}

#[tokio::test]
async fn an_idle_thread_is_owned_before_its_rollout_is_materialized() {
    let directory = directory("idle_ownership");
    let secondary = RolloutThreadDirectory::new(directory.path());
    let session_id = SessionId::new("worker");
    let owner = directory
        .create_thread(&session_id, &spawn())
        .await
        .unwrap();
    assert!(!directory.rollout_path(&session_id).unwrap().exists());
    assert!(
        secondary
            .create_thread(&session_id, &spawn())
            .await
            .is_err()
    );
    let Err(error) = secondary
        .resume_thread(&ResumeThreadParams::new(session_id.clone()))
        .await
    else {
        panic!("the idle owner must prevent resume");
    };
    assert!(error.to_string().contains("another writer"), "{error}");
    owner.discard().await.unwrap();
    secondary
        .create_thread(&session_id, &spawn())
        .await
        .unwrap()
        .shutdown()
        .await
        .unwrap();
    assert!(
        !directory
            .path()
            .join("thread-writer-locks/worker.lock")
            .exists()
    );
}

#[tokio::test]
async fn acquiring_ownership_removes_stale_locks_and_preserves_active_ones() {
    let directory = directory("stale_ownership");
    let active = SessionId::new("active");
    let owner = directory.create_thread(&active, &spawn()).await.unwrap();
    let stale = directory.path().join("thread-writer-locks/stale.lock");
    std::fs::File::create(&stale).unwrap();
    let secondary = RolloutThreadDirectory::new(directory.path());
    let other = secondary
        .create_thread(&SessionId::new("other"), &spawn())
        .await
        .unwrap();
    assert!(!stale.exists());
    assert!(secondary.create_thread(&active, &spawn()).await.is_err());
    owner.shutdown().await.unwrap();
    other.shutdown().await.unwrap();
}

#[tokio::test]
async fn persisting_a_thread_without_a_run_keeps_its_metadata_for_resume() {
    let directory = directory("persist_idle");
    let session_id = SessionId::new("worker");
    let owner = directory
        .create_thread(&session_id, &spawn())
        .await
        .unwrap();
    owner.persist().await.unwrap();
    owner.persist().await.unwrap();
    owner.shutdown().await.unwrap();
    let resumed = ResumedThread::resume(&directory, &ResumeThreadParams::new(session_id))
        .await
        .unwrap();
    assert_eq!(resumed.history().records().len(), 1);
    assert_eq!(resumed.history().records()[0].type_name(), "session_meta");
    assert!(resumed.reconstruction().history().is_empty());
    assert!(resumed.reconstruction().last_run().is_none());
    resumed.recorder().shutdown().await.unwrap();
}

#[tokio::test]
async fn failed_persistence_retains_initial_metadata_for_retry() {
    let directory = directory("persist_retry");
    let blocker = directory.path().join("blocked");
    std::fs::write(&blocker, "not a directory").unwrap();
    let path = blocker.join("rollout-root.jsonl");
    let recorder = RolloutFileRecorder::create_with_session_meta(
        &path,
        RolloutSessionMeta::new(SessionId::new("root")),
    );
    assert!(recorder.persist().await.is_err());
    std::fs::remove_file(blocker).unwrap();
    recorder.persist().await.unwrap();
    recorder.shutdown().await.unwrap();
    let records = RolloutReader::open(path).read_all().await.unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].type_name(), "session_meta");
}

#[tokio::test]
async fn in_memory_resume_creates_empty_history_and_installs_supplied_history() {
    let store = InMemoryThreadStore::new();
    let session_id = SessionId::new("worker");
    let params = ResumeThreadParams::new(session_id.clone());
    let resumed = ResumedThread::resume(&store, &params).await.unwrap();
    assert!(resumed.history().records().is_empty());
    record(resumed.recorder().as_ref(), "old-run", "old");
    let retained = store
        .load_history(&LoadThreadHistoryParams::new(session_id.clone()))
        .await
        .unwrap();
    store.resume_thread(&params).await.unwrap();
    assert_eq!(
        store
            .load_history(&LoadThreadHistoryParams::new(session_id.clone()))
            .await
            .unwrap(),
        retained
    );

    let source = InMemoryThreadStore::new();
    let source_owner = source.create_thread(&session_id, &spawn()).await.unwrap();
    record(source_owner.as_ref(), "new-run", "new");
    let supplied = source
        .load_history(&LoadThreadHistoryParams::new(session_id.clone()))
        .await
        .unwrap()
        .into_records();
    let loaded = ResumedThread::resume(
        &store,
        &params.clone().with_history(Arc::new(supplied.clone())),
    )
    .await
    .unwrap();
    assert_eq!(loaded.history().records(), supplied);
    assert_eq!(
        loaded.reconstruction().last_run().unwrap().run_id(),
        &RunId::new("new-run")
    );

    let empty = ResumedThread::resume(&store, &params.with_history(Vec::new()))
        .await
        .unwrap();
    assert!(empty.history().records().is_empty());
    record(empty.recorder().as_ref(), "after-empty", "after");
    assert_eq!(
        store
            .load_history(&LoadThreadHistoryParams::new(session_id))
            .await
            .unwrap()
            .records()[0]
            .timeline_seq(),
        0
    );
}

#[tokio::test]
async fn in_memory_lifecycle_notifications_keep_recording_handles_writable() {
    let store = InMemoryThreadStore::new();
    let session_id = SessionId::new("worker");
    let owner = store.create_thread(&session_id, &spawn()).await.unwrap();
    owner.persist().await.unwrap();
    owner.shutdown().await.unwrap();
    record(owner.as_ref(), "after-shutdown", "one");
    owner.discard().await.unwrap();
    record(owner.as_ref(), "after-discard", "two");
    owner.flush().await.unwrap();
    assert_eq!(
        store
            .load_history(&LoadThreadHistoryParams::new(session_id))
            .await
            .unwrap()
            .records()
            .len(),
        3
    );
}

struct ResumeProbe {
    directory: RolloutThreadDirectory,
    final_writer: Option<Arc<dyn RolloutRecorder>>,
    history_gate: Option<Arc<tokio::sync::Notify>>,
    fail_history: bool,
}

#[async_trait]
impl RolloutThreadStore for ResumeProbe {
    async fn create_thread(
        &self,
        session_id: &SessionId,
        spawn: &RolloutThreadSpawn,
    ) -> Result<Arc<dyn RolloutRecorder>> {
        self.directory.create_thread(session_id, spawn).await
    }
}

#[async_trait]
impl ThreadStore for ResumeProbe {
    async fn create_thread_with(
        &self,
        params: &CreateThreadParams,
    ) -> Result<Arc<dyn RolloutRecorder>> {
        self.directory.create_thread_with(params).await
    }

    async fn resume_thread(&self, params: &ResumeThreadParams) -> Result<Arc<dyn RolloutRecorder>> {
        if let Some(writer) = &self.final_writer {
            record(writer.as_ref(), "last-run", "last input");
            writer.shutdown().await?;
        }
        self.directory.resume_thread(params).await
    }

    async fn load_history(&self, params: &LoadThreadHistoryParams) -> Result<StoredThreadHistory> {
        if self.fail_history {
            return Err(Error::caller("history could not be read"));
        }
        if let Some(gate) = &self.history_gate {
            gate.notify_one();
            std::future::pending::<()>().await;
        }
        self.directory.load_history(params).await
    }

    async fn read_thread(&self, params: &ReadThreadParams) -> Result<StoredThread> {
        self.directory.read_thread(params).await
    }

    async fn list_threads(&self, params: &ListThreadsParams) -> Result<ThreadPage> {
        self.directory.list_threads(params).await
    }

    async fn update_thread_metadata(
        &self,
        params: &UpdateThreadMetadataParams,
    ) -> Result<Option<StoredThread>> {
        self.directory.update_thread_metadata(params).await
    }

    async fn archive_thread(&self, params: &ArchiveThreadParams) -> Result<()> {
        self.directory.archive_thread(params).await
    }

    async fn unarchive_thread(&self, params: &ArchiveThreadParams) -> Result<StoredThread> {
        self.directory.unarchive_thread(params).await
    }

    async fn delete_thread(&self, params: &DeleteThreadParams) -> Result<()> {
        self.directory.delete_thread(params).await
    }
}

#[tokio::test]
async fn the_resume_snapshot_includes_records_written_before_ownership_handoff() {
    let directory = directory("handoff_snapshot");
    let session_id = SessionId::new("worker");
    let owner = directory
        .create_thread(&session_id, &spawn())
        .await
        .unwrap();
    record(owner.as_ref(), "old-run", "old input");
    owner.flush().await.unwrap();
    let store = ResumeProbe {
        directory,
        final_writer: Some(owner),
        history_gate: None,
        fail_history: false,
    };
    let resumed = ResumedThread::resume(&store, &ResumeThreadParams::new(session_id.clone()))
        .await
        .unwrap();
    let current = store
        .directory
        .load_history(&LoadThreadHistoryParams::new(session_id))
        .await
        .unwrap();
    assert_eq!(resumed.history(), &current);
    assert_eq!(
        resumed.reconstruction().last_run().unwrap().run_id(),
        &RunId::new("last-run")
    );
    resumed.recorder().shutdown().await.unwrap();
}

#[tokio::test]
async fn a_failed_history_read_releases_the_reopened_writer() {
    let directory = directory("history_read_failure");
    let session_id = SessionId::new("worker");
    let owner = directory
        .create_thread(&session_id, &spawn())
        .await
        .unwrap();
    owner.persist().await.unwrap();
    owner.shutdown().await.unwrap();
    let store = ResumeProbe {
        directory,
        final_writer: None,
        history_gate: None,
        fail_history: true,
    };
    let params = ResumeThreadParams::new(session_id);
    let error = ResumedThread::resume(&store, &params).await.unwrap_err();
    assert_eq!(
        error.to_string(),
        Error::caller("history could not be read").to_string()
    );
    store
        .directory
        .resume_thread(&params)
        .await
        .unwrap()
        .shutdown()
        .await
        .unwrap();
}

#[tokio::test]
async fn cancelling_resume_while_loading_history_releases_ownership() {
    let directory = directory("cancel_resume");
    let session_id = SessionId::new("worker");
    let owner = directory
        .create_thread(&session_id, &spawn())
        .await
        .unwrap();
    owner.persist().await.unwrap();
    owner.shutdown().await.unwrap();
    let gate = Arc::new(tokio::sync::Notify::new());
    let store = Arc::new(ResumeProbe {
        directory,
        final_writer: None,
        history_gate: Some(Arc::clone(&gate)),
        fail_history: false,
    });
    let params = ResumeThreadParams::new(session_id);
    let working_store = Arc::clone(&store);
    let working_params = params.clone();
    let task = tokio::spawn(async move {
        ResumedThread::resume(working_store.as_ref(), &working_params).await
    });
    gate.notified().await;
    assert!(store.directory.resume_thread(&params).await.is_err());
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    let resumed = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Ok(recorder) = store.directory.resume_thread(&params).await {
                break recorder;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    resumed.shutdown().await.unwrap();
}
