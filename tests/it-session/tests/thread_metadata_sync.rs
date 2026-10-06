//! A live thread's metadata derived from what is recorded into it, as Codex's `LiveThread` and
//! `ThreadMetadataSync` keep it (`thread-store/src/live_thread.rs`,
//! `thread-store/src/thread_metadata_sync.rs`), over the in-memory store, which keeps every derived
//! field, and the rollout directory, which keeps none.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use ra_core::{
    agent::control::AgentPath,
    error::{Error, SessionErrorKind},
    event::EventTimestamp,
    item::{AgentId, ContentBlock, ImageBlock, ImageSource, Message, MessageRole, ModelInputItem},
    session::{
        SessionId,
        rollout::{
            PersistContext, RolloutItem, RolloutRecorder, RolloutRunStarted, RolloutThreadSpawn,
            RolloutThreadStore, RolloutTurnContext,
        },
    },
    state::RunId,
};
use ra_session::{
    CreateThreadParams, DeleteThreadParams, InMemoryThreadStore, ReadThreadParams,
    ResumeThreadParams, RolloutPayload, RolloutReader, RolloutRecord, RolloutSessionMeta,
    RolloutThreadDirectory, ThreadMetadataPatch, ThreadStore, UpdateThreadMetadataParams,
};

// ---------------------------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------------------------

fn directory(name: &str) -> RolloutThreadDirectory {
    let dir = std::env::temp_dir()
        .join("rusty_agent_tests")
        .join("thread_metadata_sync")
        .join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    RolloutThreadDirectory::new(dir)
}

fn meta(id: &str) -> RolloutSessionMeta {
    RolloutSessionMeta::new(SessionId::new(id))
        .with_created_at(EventTimestamp::from_millis(1_000))
        .with_model_provider("openai")
        .with_originator("test_originator")
        .with_cwd("/work")
}

fn run(input: Vec<ModelInputItem>) -> RolloutItem {
    RolloutItem::RunStarted(
        RolloutRunStarted::new(RunId::new("run-1"), AgentId::new("lead")).with_input(input),
    )
}

fn user(text: &str) -> ModelInputItem {
    ModelInputItem::Message(Message::user(text))
}

fn image() -> ModelInputItem {
    ModelInputItem::Message(Message::new(
        MessageRole::User,
        vec![ContentBlock::Image(ImageBlock::new(ImageSource::url(
            "https://example.com/a.png",
        )))],
    ))
}

/// A run start with nothing to derive: an append that only touches the update time.
fn touch() -> RolloutItem {
    run(Vec::new())
}

fn turn(model: &str, effort: Option<&str>, cwd: Option<&str>) -> RolloutItem {
    let mut context = RolloutTurnContext::new(RunId::new("run-1"), 0).with_model(model);
    if let Some(effort) = effort {
        context = context.with_effort(effort);
    }
    if let Some(cwd) = cwd {
        context = context.with_cwd(cwd);
    }
    RolloutItem::TurnContext(context)
}

async fn created(store: &InMemoryThreadStore, id: &str) -> Arc<dyn RolloutRecorder> {
    store
        .create_thread_with(&CreateThreadParams::new(meta(id)))
        .await
        .unwrap()
}

/// Waits for the thread's task to write what `done` is waiting for, without a barrier.
async fn eventually(
    store: &InMemoryThreadStore,
    id: &str,
    done: impl Fn(&ThreadMetadataPatch) -> bool,
) -> ThreadMetadataPatch {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(patch) = store.thread_metadata(&SessionId::new(id))
            && done(&patch)
        {
            return patch;
        }
        assert!(
            Instant::now() < deadline,
            "the thread's metadata was never written"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn metadata(store: &InMemoryThreadStore, id: &str) -> ThreadMetadataPatch {
    store.thread_metadata(&SessionId::new(id)).unwrap()
}

fn is_kind(error: &Error, kind: SessionErrorKind) -> bool {
    matches!(error, Error::Session { kind: found, .. } if *found == kind)
}

// ---------------------------------------------------------------------------------------------
// Deriving
// ---------------------------------------------------------------------------------------------

/// What is recorded reaches the store while the thread runs, with no barrier, as Codex writes it
/// after each append.
#[tokio::test]
async fn recorded_metadata_reaches_the_store_without_a_flush() {
    let store = InMemoryThreadStore::new();
    let recorder = created(&store, "live").await;
    recorder.record(run(vec![user("  hello metadata  ")]));
    recorder.record(turn("gpt-5", Some("high"), Some("/elsewhere")));

    let patch = eventually(&store, "live", |patch| patch.model().is_some()).await;
    assert_eq!(patch.preview(), Some("hello metadata"));
    assert_eq!(patch.first_user_message(), Some("hello metadata"));
    assert_eq!(patch.title(), Some("hello metadata"));
    assert_eq!(patch.model(), Some("gpt-5"));
    assert_eq!(patch.effort(), Some(Some("high")));
    // The session metadata named a working directory, so a turn's does not replace it.
    assert_eq!(patch.cwd(), Some("/work"));
    assert_eq!(patch.created_at(), Some(EventTimestamp::from_millis(1_000)));
    assert_eq!(patch.originator(), Some("test_originator"));
    assert_eq!(patch.model_provider(), Some("openai"));

    let thread = store
        .read_thread(&ReadThreadParams::new(SessionId::new("live")))
        .await
        .unwrap();
    assert_eq!(thread.preview(), "hello metadata");
    assert_eq!(thread.model(), Some("gpt-5"));
    assert!(thread.updated_at().is_some());
}

/// A message with only an image previews as one and is the first user message, but no title is
/// made of the placeholder; the first message with text gives it.
#[tokio::test]
async fn an_image_only_message_previews_but_gives_no_title() {
    let store = InMemoryThreadStore::new();
    let recorder = created(&store, "image").await;
    recorder.record(run(vec![image()]));
    recorder.flush().await.unwrap();
    let patch = metadata(&store, "image");
    assert_eq!(patch.preview(), Some("[Image]"));
    assert_eq!(patch.first_user_message(), Some("[Image]"));
    assert_eq!(patch.title(), None);

    recorder.record(run(vec![user("what is in it?")]));
    recorder.flush().await.unwrap();
    let patch = metadata(&store, "image");
    assert_eq!(patch.preview(), Some("[Image]"));
    assert_eq!(patch.first_user_message(), Some("[Image]"));
    assert_eq!(patch.title(), Some("what is in it?"));
}

/// Codex's `later_user_messages_do_not_emit_existing_preview_fields`.
#[tokio::test]
async fn later_messages_keep_the_first_preview_and_title() {
    let store = InMemoryThreadStore::new();
    let recorder = created(&store, "later").await;
    recorder.record(run(vec![user("first user text")]));
    recorder.record(run(vec![user("later user text")]));
    recorder.flush().await.unwrap();
    let patch = metadata(&store, "later");
    assert_eq!(patch.preview(), Some("first user text"));
    assert_eq!(patch.first_user_message(), Some("first user text"));
    assert_eq!(patch.title(), Some("first user text"));
}

/// A run start counts as a user message only when its new input holds one: a continuation base,
/// an inter-agent message and an empty input give no preview.
#[tokio::test]
async fn run_starts_without_a_user_message_give_no_preview() {
    let store = InMemoryThreadStore::new();
    let recorder = created(&store, "no_user").await;
    recorder.record(RolloutItem::RunStarted(
        RolloutRunStarted::new(RunId::new("run-1"), AgentId::new("lead"))
            .with_continuation_base(vec![user("replayed history")]),
    ));
    recorder.record(run(vec![user(
        "Message Type: MESSAGE\nTask name: t\nSender: /root\nPayload:\nfrom a peer",
    )]));
    recorder.record(touch());
    recorder.flush().await.unwrap();
    let patch = metadata(&store, "no_user");
    assert_eq!(patch.preview(), None);
    assert_eq!(patch.first_user_message(), None);
    assert_eq!(patch.title(), None);
    assert!(patch.updated_at().is_some());
}

/// Codex's `thread_settings_applied_updates_live_metadata` over turn contexts: the latest model
/// wins, a turn without an effort clears it, and the first turn's working directory is taken when
/// the session metadata names none.
#[tokio::test]
async fn turn_contexts_set_the_model_effort_and_a_missing_cwd() {
    let store = InMemoryThreadStore::new();
    let recorder = store
        .create_thread_with(&CreateThreadParams::new(RolloutSessionMeta::new(
            SessionId::new("turns"),
        )))
        .await
        .unwrap();
    recorder.record(turn("gpt-5", Some("high"), Some("/first")));
    recorder.flush().await.unwrap();
    let patch = metadata(&store, "turns");
    assert_eq!(patch.model(), Some("gpt-5"));
    assert_eq!(patch.effort(), Some(Some("high")));
    assert_eq!(patch.cwd(), Some("/first"));

    recorder.record(turn("gpt-5-mini", None, Some("/second")));
    recorder.flush().await.unwrap();
    let patch = metadata(&store, "turns");
    assert_eq!(patch.model(), Some("gpt-5-mini"));
    assert_eq!(patch.effort(), Some(None));
    assert_eq!(patch.cwd(), Some("/first"));
}

/// A spawned thread, created through the form the runtime uses, derives its metadata too.
#[tokio::test]
async fn a_spawned_thread_derives_its_metadata() {
    let store = InMemoryThreadStore::new();
    let root = SessionId::new("root");
    let recorder = RolloutThreadStore::create_thread(
        &store,
        &SessionId::new("child"),
        &RolloutThreadSpawn::new(
            root.clone(),
            root,
            1,
            AgentPath::root().join("worker").unwrap(),
        ),
    )
    .await
    .unwrap();
    recorder.record(run(vec![user("do the work")]));
    let patch = eventually(&store, "child", |patch| patch.preview().is_some()).await;
    assert_eq!(patch.preview(), Some("do the work"));
    assert!(patch.created_at().is_some());
}

/// A forked history is observed as Codex's session appends it, so the fork's metadata is written
/// before anything is recorded.
#[tokio::test]
async fn a_forked_history_is_written_at_creation() {
    let store = InMemoryThreadStore::new();
    let history = vec![
        RolloutRecord::new(
            0,
            EventTimestamp::from_millis(5),
            RolloutPayload::RunStarted(
                RolloutRunStarted::new(RunId::new("run-0"), AgentId::new("lead"))
                    .with_input(vec![user("forked question")]),
            ),
        )
        .unwrap(),
    ];
    let _recorder = store
        .create_thread_with(&CreateThreadParams::new(meta("fork")).with_history(history))
        .await
        .unwrap();
    let patch = eventually(&store, "fork", |patch| patch.preview().is_some()).await;
    assert_eq!(patch.preview(), Some("forked question"));
    assert_eq!(patch.title(), Some("forked question"));
    assert_eq!(patch.originator(), Some("test_originator"));
}

// ---------------------------------------------------------------------------------------------
// Writing and barriers
// ---------------------------------------------------------------------------------------------

/// Codex's `metadata_irrelevant_items_coalesce_updated_at_touches`: within the interval a touch
/// waits for the next barrier, which writes its time.
#[tokio::test]
async fn touches_within_the_interval_wait_for_a_barrier() {
    let store = InMemoryThreadStore::new();
    let recorder = created(&store, "touch").await;
    recorder.record(touch());
    let first = eventually(&store, "touch", |patch| patch.updated_at().is_some())
        .await
        .updated_at()
        .unwrap();

    tokio::time::sleep(Duration::from_millis(20)).await;
    recorder.record(touch());
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(metadata(&store, "touch").updated_at(), Some(first));

    recorder.flush().await.unwrap();
    let flushed = metadata(&store, "touch").updated_at().unwrap();
    assert!(flushed > first, "the barrier wrote the pending touch");
}

/// A commit signal left over after a batch was coalesced must not submit a later touch that was
/// throttled. The current-thread worker yields on its receive budget while signals remain queued.
#[tokio::test]
async fn queued_commit_signals_do_not_submit_a_later_throttled_touch() {
    let store = InMemoryThreadStore::new();
    let session_id = SessionId::new("queued_touch");
    let recorder = store
        .resume_thread(&ResumeThreadParams::new(session_id.clone()))
        .await
        .unwrap();

    // An empty resumed history has no metadata facts, so every record only touches the time.
    // Before the first acknowledgement, these records all request a commit.
    for _ in 0..1_024 {
        recorder.record(touch());
    }
    let recorded_at = EventTimestamp::now();
    tokio::task::yield_now().await;
    let first = metadata(&store, "queued_touch").updated_at().unwrap();
    assert!(first <= recorded_at);

    // Advance the wall clock without yielding to the worker and draining its remaining signals.
    std::thread::sleep(Duration::from_millis(2));
    recorder.record(touch());
    // Discard drains earlier signals but does not commit the throttled touch itself.
    recorder.discard().await.unwrap();
    assert_eq!(metadata(&store, "queued_touch").updated_at(), Some(first));
}

/// A created thread's initial metadata waits for its history: a flush before anything is recorded
/// writes nothing, while persisting the thread writes it, as Codex's barriers do.
#[tokio::test]
async fn a_created_thread_writes_its_initial_metadata_when_persisted() {
    let store = InMemoryThreadStore::new();
    let recorder = created(&store, "initial").await;
    recorder.flush().await.unwrap();
    assert_eq!(store.thread_metadata(&SessionId::new("initial")), None);

    recorder.persist(PersistContext::Standard).await.unwrap();
    let patch = metadata(&store, "initial");
    assert_eq!(patch.created_at(), Some(EventTimestamp::from_millis(1_000)));
    assert_eq!(patch.updated_at(), Some(EventTimestamp::from_millis(1_000)));
    assert_eq!(patch.cwd(), Some("/work"));
}

/// Every reason to persist writes what is pending, a created thread's initial metadata included,
/// as Codex's live thread does whether or not the store may defer the persistence itself.
#[tokio::test]
async fn every_persist_context_writes_a_created_threads_initial_metadata() {
    for (index, context) in [
        PersistContext::ThreadPreparation,
        PersistContext::SubagentSpawn,
        PersistContext::TurnStart,
        PersistContext::SteeredUserInput,
    ]
    .into_iter()
    .enumerate()
    {
        let store = InMemoryThreadStore::new();
        let name = format!("initial-{index}");
        let recorder = created(&store, &name).await;
        recorder.persist(context).await.unwrap();
        let patch = metadata(&store, &name);
        assert_eq!(
            patch.created_at(),
            Some(EventTimestamp::from_millis(1_000)),
            "{context:?}"
        );
        assert_eq!(patch.cwd(), Some("/work"), "{context:?}");
    }
}

/// A write that fails keeps its patch pending, and a later barrier reports its own attempt.
#[tokio::test]
async fn a_failed_write_stays_pending_and_is_retried() {
    let store = InMemoryThreadStore::new();
    let recorder = created(&store, "retry").await;
    store
        .delete_thread(&DeleteThreadParams::new(SessionId::new("retry")))
        .await
        .unwrap();
    let error = recorder
        .persist(PersistContext::Standard)
        .await
        .unwrap_err();
    assert!(is_kind(&error, SessionErrorKind::NotFound), "{error}");
    assert_eq!(store.thread_metadata(&SessionId::new("retry")), None);

    // Recording makes the in-memory thread exist again; the creation patch is still pending and is
    // written with what the record says.
    recorder.record(run(vec![user("back again")]));
    recorder.persist(PersistContext::Standard).await.unwrap();
    let patch = metadata(&store, "retry");
    assert_eq!(patch.originator(), Some("test_originator"));
    assert_eq!(patch.created_at(), Some(EventTimestamp::from_millis(1_000)));
    assert_eq!(patch.preview(), Some("back again"));
}

/// Records observed while an earlier write is in flight are not cleared when it lands: the last
/// model recorded is the one kept.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn appends_during_a_write_are_not_cleared_by_it() {
    let store = InMemoryThreadStore::new();
    let recorder = created(&store, "concurrent").await;
    // Each round ends at a barrier; a round whose last record lands while a write is in flight
    // loses that record if the write clears what was merged after its snapshot.
    for round in 0..50 {
        for index in 0..200 {
            recorder.record(turn(&format!("model-{round}-{index}"), None, None));
            if index % 3 == 0 {
                tokio::task::yield_now().await;
            }
        }
        recorder.flush().await.unwrap();
        let expected = format!("model-{round}-199");
        assert_eq!(
            metadata(&store, "concurrent").model(),
            Some(expected.as_str())
        );
    }
}

/// Discarding forgets the pending patch and discards the writer.
#[tokio::test]
async fn discarding_forgets_the_pending_patch() {
    let store = InMemoryThreadStore::new();
    let recorder = created(&store, "discard").await;
    recorder.discard().await.unwrap();
    recorder.persist(PersistContext::Standard).await.unwrap();
    assert_eq!(store.thread_metadata(&SessionId::new("discard")), None);
}

// ---------------------------------------------------------------------------------------------
// Resuming
// ---------------------------------------------------------------------------------------------

fn resumed_history(id: &str) -> Vec<RolloutRecord> {
    vec![
        RolloutRecord::new(
            0,
            EventTimestamp::from_millis(2_000),
            RolloutPayload::SessionMeta(
                RolloutSessionMeta::new(SessionId::new(id))
                    .with_created_at(EventTimestamp::from_millis(1_500))
                    .with_originator("resumed_originator"),
            ),
        )
        .unwrap(),
        RolloutRecord::new(
            1,
            EventTimestamp::from_millis(2_001),
            RolloutPayload::RunStarted(
                RolloutRunStarted::new(RunId::new("run-0"), AgentId::new("lead"))
                    .with_input(vec![user("hello metadata")]),
            ),
        )
        .unwrap(),
    ]
}

/// Codex's `resume_history_waits_for_append_before_flushing_metadata`: a resume alone writes
/// nothing; the first append writes what the history says with it.
#[tokio::test]
async fn a_resume_writes_nothing_until_something_is_appended() {
    let store = InMemoryThreadStore::new();
    let recorder = store
        .resume_thread(
            &ResumeThreadParams::new(SessionId::new("resumed"))
                .with_history(resumed_history("resumed")),
        )
        .await
        .unwrap();
    recorder.flush().await.unwrap();
    recorder.shutdown().await.unwrap();
    assert_eq!(store.thread_metadata(&SessionId::new("resumed")), None);

    recorder.record(touch());
    recorder.flush().await.unwrap();
    let patch = metadata(&store, "resumed");
    assert_eq!(patch.preview(), Some("hello metadata"));
    assert_eq!(patch.title(), Some("hello metadata"));
    assert_eq!(patch.originator(), Some("resumed_originator"));
    assert_eq!(patch.created_at(), Some(EventTimestamp::from_millis(1_500)));
}

/// A resumed thread's history has already given its preview, first message and title; later
/// messages do not replace them.
#[tokio::test]
async fn a_resumed_thread_keeps_its_display_fields() {
    let store = InMemoryThreadStore::new();
    let recorder = store
        .resume_thread(
            &ResumeThreadParams::new(SessionId::new("kept")).with_history(resumed_history("kept")),
        )
        .await
        .unwrap();
    recorder.record(run(vec![user("a later question")]));
    recorder.flush().await.unwrap();
    let patch = metadata(&store, "kept");
    assert_eq!(patch.preview(), Some("hello metadata"));
    assert_eq!(patch.first_user_message(), Some("hello metadata"));
    assert_eq!(patch.title(), Some("hello metadata"));
}

// ---------------------------------------------------------------------------------------------
// The directory
// ---------------------------------------------------------------------------------------------

/// The directory keeps only names, so a patch without one is not looked up — it may be written
/// before the rollout exists — while an explicit update still needs the thread.
#[tokio::test]
async fn the_directory_records_nameless_patches_without_looking_the_thread_up() {
    let directory = directory("nameless");
    let missing = SessionId::new("missing");
    let derived = UpdateThreadMetadataParams::new(
        missing.clone(),
        ThreadMetadataPatch::new()
            .with_preview("hello")
            .with_updated_at(EventTimestamp::now()),
    );
    directory.record_thread_metadata(&derived).await.unwrap();

    let error = directory
        .update_thread_metadata(&derived)
        .await
        .unwrap_err();
    assert!(is_kind(&error, SessionErrorKind::NotFound), "{error}");
    let named = UpdateThreadMetadataParams::new(
        missing,
        ThreadMetadataPatch::new().with_name(Some("a name".to_owned())),
    );
    let error = directory.record_thread_metadata(&named).await.unwrap_err();
    assert!(is_kind(&error, SessionErrorKind::NotFound), "{error}");
}

/// Shutting a deferred rollout down right after recording, with a touch still pending, settles the
/// metadata without reading the rollout, then writes and closes it.
#[tokio::test]
async fn a_directory_thread_shuts_down_right_after_recording() {
    let directory = directory("shutdown");
    let session_id = SessionId::new("quick");
    let recorder = directory
        .create_thread_with(&CreateThreadParams::new(meta("quick")))
        .await
        .unwrap();
    recorder.record(run(vec![user("hi")]));
    recorder.record(touch());
    recorder.shutdown().await.unwrap();

    let records = RolloutReader::open(directory.rollout_path(&session_id).unwrap())
        .read_all()
        .await
        .unwrap();
    assert_eq!(
        records
            .iter()
            .map(RolloutRecord::type_name)
            .collect::<Vec<_>>(),
        ["session_meta", "run_started", "run_started"]
    );
    // The writer let go of the rollout, so it can be resumed, and the resumed thread can record
    // and shut down again.
    let resumed = directory
        .resume_thread(&ResumeThreadParams::new(session_id))
        .await
        .unwrap();
    resumed.record(touch());
    resumed.shutdown().await.unwrap();
}

/// A spawned directory thread persisted before anything is recorded writes its initial metadata
/// after the rollout is created, and discarding a resumed writer lets go of the rollout.
#[tokio::test]
async fn directory_barriers_and_discard_reach_the_writer() {
    let directory = directory("barriers");
    let root = SessionId::new("root");
    let child = SessionId::new("child");
    let recorder = RolloutThreadStore::create_thread(
        &directory,
        &child,
        &RolloutThreadSpawn::new(
            root.clone(),
            root,
            1,
            AgentPath::root().join("worker").unwrap(),
        ),
    )
    .await
    .unwrap();
    recorder.persist(PersistContext::Standard).await.unwrap();
    recorder.shutdown().await.unwrap();
    assert!(directory.rollout_path(&child).unwrap().is_file());

    let resumed = directory
        .resume_thread(&ResumeThreadParams::new(child.clone()))
        .await
        .unwrap();
    resumed.discard().await.unwrap();
    directory
        .resume_thread(&ResumeThreadParams::new(child))
        .await
        .unwrap()
        .shutdown()
        .await
        .unwrap();
}
