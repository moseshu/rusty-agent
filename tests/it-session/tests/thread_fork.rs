//! Forking a thread, as Codex forks one: cutting the source's records before a user message
//! (`core/src/thread_rollout_truncation_tests.rs`), choosing the snapshot from whether the source's
//! newest run is still in progress (`core/src/thread_manager_tests.rs`), and copying the snapshot
//! into a new thread that names its source (`core/tests/suite/fork_thread.rs`).

use std::path::PathBuf;

use ra_core::{
    agent::control::{AgentPath, InterAgentCommunication, MessageDeliveryMode},
    error::{Error, SessionErrorKind},
    event::{
        EventTimestamp, HostEvent, HostEventBody,
        exec::{ExecEvent, ExecSessionId, ExecStartedEvent},
    },
    item::{
        AgentId, CallId, ItemId, Message, MessageRole, ModelInputItem, OutputPhase, Reasoning,
        RunItem, RunItemKind, ToolCall,
    },
    session::{
        InterruptedTurnHistoryMarker, SessionId,
        rollout::{
            RolloutItem, RolloutRecorder, RolloutRunEnd, RolloutRunEnded, RolloutRunStarted,
        },
    },
    state::{RunId, RunState},
};
use ra_session::{
    ForkSnapshot, ForkThreadParams, ForkedThread, InMemoryThreadStore, LoadThreadHistoryParams,
    ReadThreadParams, RolloutFileRecorder, RolloutPayload, RolloutReader, RolloutRecord,
    RolloutSessionMeta, RolloutThreadDirectory, RolloutWriter, StoredThreadHistory, ThreadStore,
    reconstruct_history, truncate_rollout_before_nth_user_message,
    user_message_positions_in_rollout,
};
use serde_json::json;

// ---------------------------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------------------------

/// Records in the order a session wrote them.
#[derive(Default, Clone)]
struct Rollout(Vec<RolloutRecord>);

impl Rollout {
    fn push(&mut self, payload: impl Into<RolloutPayload>) -> &mut Self {
        let seq = self.0.len() as u64;
        self.0.push(
            RolloutRecord::new(seq, EventTimestamp::from_millis(seq), payload.into()).unwrap(),
        );
        self
    }

    fn start_with(&mut self, run: &str, input: Vec<ModelInputItem>) -> &mut Self {
        self.push(RolloutRunStarted::new(RunId::new(run), AgentId::new("lead")).with_input(input))
    }

    fn start(&mut self, run: &str, input: &[&str]) -> &mut Self {
        self.start_with(run, input.iter().map(|text| user(text)).collect())
    }

    fn item(&mut self, item: RunItem) -> &mut Self {
        self.push(item)
    }

    fn end(&mut self, run: &str, end: RolloutRunEnd) -> &mut Self {
        self.push(RolloutRunEnded::new(RunId::new(run), end))
    }

    fn records(&self) -> Vec<RolloutRecord> {
        self.0.clone()
    }
}

fn user(text: &str) -> ModelInputItem {
    ModelInputItem::Message(Message::user(text))
}

fn said(id: &str, text: &str) -> RunItem {
    RunItem::new(
        ItemId::new(id),
        RunItemKind::Message(Message::assistant(text, OutputPhase::Final)),
    )
}

fn source_history(rollout: &Rollout) -> StoredThreadHistory {
    StoredThreadHistory::new(SessionId::new("source"), rollout.records())
}

/// The payloads of `records`, which is what a copy keeps of them; sequence numbers and times are
/// the writer's.
fn payloads(records: &[RolloutRecord]) -> Vec<RolloutPayload> {
    records
        .iter()
        .map(|record| record.payload().unwrap())
        .collect()
}

/// Forks `rollout`, held by an in-memory store as the thread `source`, and returns the records
/// the fork's thread starts with, after its session metadata.
async fn fork_in_memory(
    rollout: &Rollout,
    snapshot: ForkSnapshot,
) -> (ForkedThread, Vec<RolloutRecord>) {
    let store = InMemoryThreadStore::new();
    let forked = ForkedThread::fork_from_history(
        &store,
        source_history(rollout),
        &ForkThreadParams::new(snapshot),
    )
    .await
    .unwrap();
    let records = store
        .load_history(&LoadThreadHistoryParams::new(forked.session_id().clone()))
        .await
        .unwrap()
        .into_records();
    assert_eq!(records[0].type_name(), "session_meta");
    (forked, records[1..].to_vec())
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir()
        .join("rusty_agent_tests")
        .join("thread_fork")
        .join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// The `(record, input)` pairs `user_message_positions_in_rollout` finds.
fn positions(records: &[RolloutRecord]) -> Vec<(usize, usize)> {
    user_message_positions_in_rollout(records)
        .unwrap()
        .iter()
        .map(|position| (position.record(), position.input()))
        .collect()
}

fn is_kind(error: &Error, kind: SessionErrorKind) -> bool {
    matches!(error, Error::Session { kind: found, .. } if *found == kind)
}

/// Two completed runs: the first answers twice, the second reasons, calls a tool and answers.
fn two_runs() -> Rollout {
    let mut rollout = Rollout::default();
    rollout
        .push(RolloutSessionMeta::new(SessionId::new("source")))
        .start("run-1", &["u1"])
        .item(said("a-1", "a1"))
        .item(said("a-2", "a2"))
        .end("run-1", RolloutRunEnd::Completed)
        .start("run-2", &["u2"])
        .item(said("a-3", "a3"))
        .item(RunItem::new(
            ItemId::new("r-1"),
            RunItemKind::Reasoning(Reasoning::new().with_summary(vec!["s".to_owned()])),
        ))
        .item(RunItem::new(
            ItemId::new("c-1"),
            RunItemKind::ToolCall(ToolCall::new(CallId::new("call-1"), "tool", json!({}))),
        ))
        .item(said("a-4", "a4"))
        .end("run-2", RolloutRunEnd::Completed);
    rollout
}

// ---------------------------------------------------------------------------------------------
// Truncation before a user message
// ---------------------------------------------------------------------------------------------

#[test]
fn truncating_before_the_nth_user_message_cuts_before_the_run_that_carries_it() {
    let records = two_runs().records();

    let truncated = truncate_rollout_before_nth_user_message(records.clone(), 1).unwrap();
    assert_eq!(truncated, records[..5].to_vec());

    let truncated = truncate_rollout_before_nth_user_message(records.clone(), 2).unwrap();
    assert_eq!(truncated, records);

    let truncated = truncate_rollout_before_nth_user_message(records.clone(), 0).unwrap();
    assert_eq!(
        truncated,
        records[..1].to_vec(),
        "only the session metadata"
    );
}

#[test]
fn truncating_before_usize_max_keeps_the_full_rollout() {
    let records = two_runs().records();
    assert_eq!(
        truncate_rollout_before_nth_user_message(records.clone(), usize::MAX).unwrap(),
        records
    );
}

#[test]
fn only_messages_from_the_user_that_runs_start_on_count_as_user_messages() {
    let envelope = InterAgentCommunication::from_agent(
        AgentPath::root(),
        AgentPath::root().join("worker").unwrap(),
        "go",
        MessageDeliveryMode::TriggerTurn,
    )
    .to_message();
    let mut rollout = Rollout::default();
    rollout
        .push(RolloutSessionMeta::new(SessionId::new("source")))
        // A run started with no new input, as a continuation from a checkpoint is.
        .start("run-0", &[])
        .end("run-0", RolloutRunEnd::Completed)
        // A system message alone is context, not a user message.
        .start_with(
            "run-1",
            vec![ModelInputItem::Message(Message::text(
                MessageRole::System,
                "context",
            ))],
        )
        .end("run-1", RolloutRunEnd::Completed)
        // Mail from another agent is not a user message, as Codex gives it an item of its own.
        .start_with("run-2", vec![ModelInputItem::Message(envelope)])
        .end("run-2", RolloutRunEnd::Completed)
        .start("run-3", &["feature request"])
        // A stop hook's continuation is recorded as a run item, not as input the run started on.
        .item(RunItem::new(
            ItemId::new("hook-continuation-1"),
            RunItemKind::Message(Message::text(MessageRole::User, "keep going")),
        ))
        .end("run-3", RolloutRunEnd::Completed)
        // A continuation base is the run's history supplied again, not new input.
        .push(
            RolloutRunStarted::new(RunId::new("run-3"), AgentId::new("lead"))
                .with_continuation_base(vec![user("feature request"), user("more")]),
        )
        .end("run-3", RolloutRunEnd::Completed)
        // The marker an interrupted run leaves is context, as Codex's `<turn_aborted>` fragment is.
        .start_with(
            "run-4",
            vec![ModelInputItem::Message(
                InterruptedTurnHistoryMarker::ContextualUser
                    .message()
                    .unwrap(),
            )],
        )
        .end("run-4", RolloutRunEnd::Completed)
        // Every user message counts, however many one run start carries.
        .start("run-5", &["second question", "and a third"])
        .end("run-5", RolloutRunEnd::Completed);
    let records = rollout.records();

    assert_eq!(positions(&records), vec![(7, 0), (14, 0), (14, 1)]);
    assert_eq!(
        truncate_rollout_before_nth_user_message(records.clone(), 1).unwrap(),
        records[..14].to_vec()
    );
}

/// Codex cuts before the user message itself, keeping what its turn recorded before it; a run's
/// start here carries its input, so a cut inside it keeps the start with the input before the
/// message.
#[test]
fn a_cut_inside_a_run_starts_input_keeps_the_input_before_the_message() {
    let context = ModelInputItem::Message(Message::text(MessageRole::System, "context"));
    let mut rollout = Rollout::default();
    rollout
        .push(RolloutSessionMeta::new(SessionId::new("source")))
        .start_with("run-1", vec![context.clone(), user("u1"), user("u2")])
        .item(said("a-1", "a1"))
        .end("run-1", RolloutRunEnd::Completed);
    let records = rollout.records();
    assert_eq!(positions(&records), vec![(1, 1), (1, 2)]);

    let before_second = truncate_rollout_before_nth_user_message(records.clone(), 1).unwrap();
    assert_eq!(before_second.len(), 2);
    assert_eq!(before_second[0], records[0]);
    let RolloutPayload::RunStarted(started) = before_second[1].payload().unwrap() else {
        panic!("the run start is kept");
    };
    assert_eq!(started.run_id(), &RunId::new("run-1"));
    assert_eq!(started.input(), [context.clone(), user("u1")]);
    assert_eq!(before_second[1].timeline_seq(), records[1].timeline_seq());
    assert_eq!(
        reconstruct_history(&before_second).unwrap().history(),
        [context.clone(), user("u1")]
    );

    let before_first = truncate_rollout_before_nth_user_message(records.clone(), 0).unwrap();
    let RolloutPayload::RunStarted(started) = before_first[1].payload().unwrap() else {
        panic!("the run start is kept");
    };
    assert_eq!(started.input(), [context]);

    // A message that opens the input leaves nothing of the start to keep.
    let mut rollout = Rollout::default();
    rollout
        .start("run-1", &["u1", "u2"])
        .end("run-1", RolloutRunEnd::Completed);
    assert!(
        truncate_rollout_before_nth_user_message(rollout.records(), 0)
            .unwrap()
            .is_empty()
    );
}

// ---------------------------------------------------------------------------------------------
// Snapshots
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_truncation_in_range_forks_the_history_before_that_user_message() {
    let rollout = two_runs();
    let (forked, copied) =
        fork_in_memory(&rollout, ForkSnapshot::TruncateBeforeNthUserMessage(1)).await;
    assert_eq!(payloads(&copied), payloads(&rollout.records()[..5]));
    let last = forked.reconstruction().last_run().unwrap();
    assert_eq!(last.run_id(), &RunId::new("run-1"));
    assert_eq!(
        forked.reconstruction().history(),
        reconstruct_history(&rollout.records()[..5])
            .unwrap()
            .history()
    );
}

#[tokio::test]
async fn a_truncation_out_of_range_at_a_run_boundary_keeps_the_whole_history() {
    let rollout = two_runs();
    let (_, copied) = fork_in_memory(&rollout, usize::MAX.into()).await;
    assert_eq!(payloads(&copied), payloads(&rollout.records()));
}

#[tokio::test]
async fn a_truncation_out_of_range_mid_run_drops_only_the_unfinished_run() {
    let mut rollout = Rollout::default();
    rollout
        .start("run-1", &["u1"])
        .item(said("a-1", "a1"))
        .end("run-1", RolloutRunEnd::Completed)
        .start("run-2", &["u2"])
        .item(said("a-2", "partial"));

    let (forked, copied) = fork_in_memory(&rollout, usize::MAX.into()).await;
    assert_eq!(payloads(&copied), payloads(&rollout.records()[..3]));
    assert_eq!(
        forked.reconstruction().last_run().unwrap().run_id(),
        &RunId::new("run-1")
    );

    // An index past the user messages there are behaves the same.
    let (_, copied) = fork_in_memory(&rollout, ForkSnapshot::TruncateBeforeNthUserMessage(2)).await;
    assert_eq!(payloads(&copied), payloads(&rollout.records()[..3]));
}

#[tokio::test]
async fn a_run_paused_for_approval_is_in_progress_from_its_first_segment() {
    let mut rollout = Rollout::default();
    rollout
        .start("run-1", &["u1"])
        .end("run-1", RolloutRunEnd::Completed)
        .start("run-2", &["u2"])
        .item(said("a-1", "asks"))
        .end("run-2", RolloutRunEnd::Interrupted)
        .start("run-2", &[])
        .item(said("a-2", "carries on"))
        .end("run-2", RolloutRunEnd::Interrupted);

    let (_, copied) = fork_in_memory(&rollout, usize::MAX.into()).await;
    assert_eq!(payloads(&copied), payloads(&rollout.records()[..2]));

    let (forked, copied) = fork_in_memory(&rollout, ForkSnapshot::Interrupted).await;
    let mut expected = payloads(&rollout.records());
    expected.push(RolloutPayload::Item(
        InterruptedTurnHistoryMarker::ContextualUser.item().unwrap(),
    ));
    expected.push(RolloutPayload::RunEnded(RolloutRunEnded::new(
        RunId::new("run-2"),
        RolloutRunEnd::Cancelled,
    )));
    assert_eq!(payloads(&copied), expected);
    assert_eq!(
        forked
            .reconstruction()
            .last_run()
            .unwrap()
            .end()
            .unwrap()
            .end(),
        RolloutRunEnd::Cancelled
    );
}

#[tokio::test]
async fn an_interrupted_fork_of_a_run_in_progress_marks_and_ends_it_once() {
    let mut rollout = Rollout::default();
    rollout
        .push(RolloutSessionMeta::new(SessionId::new("source")))
        .start("run-1", &["hello"])
        .item(said("a-1", "partial"));

    let store = InMemoryThreadStore::new();
    let forked = ForkedThread::fork_from_history(
        &store,
        source_history(&rollout),
        &ForkThreadParams::new(ForkSnapshot::Interrupted),
    )
    .await
    .unwrap();
    let copy = store
        .load_history(&LoadThreadHistoryParams::new(forked.session_id().clone()))
        .await
        .unwrap();
    // As Codex's `append_interrupted_boundary`: the same marker a real interrupt records, then the
    // end a cancelled run records.
    let marker = InterruptedTurnHistoryMarker::ContextualUser.item().unwrap();
    let cancelled = RolloutPayload::RunEnded(RolloutRunEnded::new(
        RunId::new("run-1"),
        RolloutRunEnd::Cancelled,
    ));
    let copied = payloads(copy.records());
    assert_eq!(
        copied[copied.len() - 2..],
        [RolloutPayload::Item(marker.clone()), cancelled.clone()]
    );
    let mut expected = reconstruct_history(&rollout.records())
        .unwrap()
        .into_history();
    expected.extend(marker.to_model_input());
    assert_eq!(forked.reconstruction().history(), expected);

    // The fork is at a run boundary, so forking it again copies it as it stands.
    let reforked = ForkedThread::fork(
        &store,
        forked.session_id(),
        &ForkThreadParams::new(ForkSnapshot::Interrupted),
    )
    .await
    .unwrap();
    let recopy = payloads(
        store
            .load_history(&LoadThreadHistoryParams::new(reforked.session_id().clone()))
            .await
            .unwrap()
            .records(),
    );
    for once in [RolloutPayload::Item(marker), cancelled] {
        assert_eq!(recopy.iter().filter(|payload| **payload == once).count(), 1);
    }
}

#[tokio::test]
async fn the_marker_an_interrupted_fork_records_is_the_one_its_params_pick() {
    let mut rollout = Rollout::default();
    rollout.start("run-1", &["hello"]);
    let fork = |marker| {
        let store = InMemoryThreadStore::new();
        let history = source_history(&rollout);
        async move {
            let forked = ForkedThread::fork_from_history(
                &store,
                history,
                &ForkThreadParams::new(ForkSnapshot::Interrupted)
                    .with_interrupted_turn_marker(marker),
            )
            .await
            .unwrap();
            let records = store
                .load_history(&LoadThreadHistoryParams::new(forked.session_id().clone()))
                .await
                .unwrap();
            payloads(&records.records()[2..])
        }
    };
    let cancelled = RolloutPayload::RunEnded(RolloutRunEnded::new(
        RunId::new("run-1"),
        RolloutRunEnd::Cancelled,
    ));

    // Codex's `disabled_interrupted_fork_snapshot_appends_only_interrupt_event`.
    assert_eq!(
        fork(InterruptedTurnHistoryMarker::Disabled).await,
        [cancelled.clone()]
    );
    let developer = InterruptedTurnHistoryMarker::Developer.item().unwrap();
    let RunItemKind::Message(message) = developer.kind() else {
        panic!("the marker is a message");
    };
    assert_eq!(message.role(), MessageRole::System);
    assert_eq!(
        fork(InterruptedTurnHistoryMarker::Developer).await,
        [RolloutPayload::Item(developer), cancelled]
    );
}

#[tokio::test]
async fn an_interrupted_fork_at_a_run_boundary_copies_the_history_as_it_stands() {
    for end in [
        RolloutRunEnd::Completed,
        RolloutRunEnd::Cancelled,
        RolloutRunEnd::Failed,
    ] {
        let mut rollout = Rollout::default();
        rollout
            .start("run-1", &["hello"])
            .item(said("a-1", "partial"))
            .end("run-1", end);
        let (_, copied) = fork_in_memory(&rollout, ForkSnapshot::Interrupted).await;
        assert_eq!(payloads(&copied), payloads(&rollout.records()), "{end:?}");
    }

    let (forked, copied) = fork_in_memory(&Rollout::default(), ForkSnapshot::Interrupted).await;
    assert!(copied.is_empty());
    assert!(forked.reconstruction().history().is_empty());
}

// ---------------------------------------------------------------------------------------------
// The new thread
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_fork_gets_a_new_session_that_names_its_source() {
    let store = InMemoryThreadStore::new();
    let generated = ForkedThread::fork_from_history(
        &store,
        source_history(&two_runs()),
        &ForkThreadParams::new(ForkSnapshot::Interrupted),
    )
    .await
    .unwrap();
    assert_ne!(generated.session_id(), &SessionId::new("source"));
    assert_eq!(generated.forked_from_id(), &SessionId::new("source"));

    let reserved = ForkedThread::fork_from_history(
        &store,
        source_history(&two_runs()),
        &ForkThreadParams::new(ForkSnapshot::Interrupted)
            .with_session_meta(RolloutSessionMeta::new(SessionId::new("fork")).with_cwd("/work")),
    )
    .await
    .unwrap();
    assert_eq!(reserved.session_id(), &SessionId::new("fork"));
    let thread = store
        .read_thread(&ReadThreadParams::new(SessionId::new("fork")))
        .await
        .unwrap();
    assert_eq!(thread.forked_from_id(), Some(&SessionId::new("source")));
    assert_eq!(thread.cwd(), Some("/work"));
    assert_eq!(thread.parent_session_id(), None);
}

#[tokio::test]
async fn forking_a_thread_the_store_does_not_hold_creates_nothing() {
    let directory = RolloutThreadDirectory::new(temp_dir("missing_source"));
    let error = ForkedThread::fork(
        &directory,
        &SessionId::new("absent"),
        &ForkThreadParams::new(ForkSnapshot::Interrupted)
            .with_session_meta(RolloutSessionMeta::new(SessionId::new("fork"))),
    )
    .await
    .unwrap_err();
    assert!(is_kind(&error, SessionErrorKind::NotFound), "{error}");
    assert!(
        !directory
            .rollout_path(&SessionId::new("fork"))
            .unwrap()
            .exists()
    );
}

#[tokio::test]
async fn a_fork_onto_a_session_the_store_holds_is_refused_before_anything_is_written() {
    // In memory: the source itself, closed or not, cannot be the fork's target.
    let store = InMemoryThreadStore::new();
    let source = ForkedThread::fork_from_history(
        &store,
        source_history(&two_runs()),
        &ForkThreadParams::new(ForkSnapshot::Interrupted)
            .with_session_meta(RolloutSessionMeta::new(SessionId::new("taken"))),
    )
    .await
    .unwrap();
    source.recorder().shutdown().await.unwrap();
    let before = store
        .load_history(&LoadThreadHistoryParams::new(SessionId::new("taken")))
        .await
        .unwrap();
    let error = ForkedThread::fork(
        &store,
        &SessionId::new("taken"),
        &ForkThreadParams::new(1)
            .with_session_meta(RolloutSessionMeta::new(SessionId::new("taken"))),
    )
    .await
    .unwrap_err();
    assert!(!is_kind(&error, SessionErrorKind::NotFound), "{error}");
    assert_eq!(
        store
            .load_history(&LoadThreadHistoryParams::new(SessionId::new("taken")))
            .await
            .unwrap(),
        before
    );

    // In the directory: a rollout already there, with no live writer, is refused the same way.
    let directory = RolloutThreadDirectory::new(temp_dir("taken"));
    let taken = SessionId::new("taken");
    let path = directory.rollout_path(&taken).unwrap();
    let recorder = RolloutFileRecorder::create_with_session_meta(
        &path,
        RolloutSessionMeta::new(taken.clone()),
    );
    recorder.record(RolloutItem::RunStarted(
        RolloutRunStarted::new(RunId::new("run-1"), AgentId::new("lead"))
            .with_input(vec![user("u1")]),
    ));
    recorder.shutdown().await.unwrap();
    let before = std::fs::read(&path).unwrap();
    let error = ForkedThread::fork(
        &directory,
        &taken,
        &ForkThreadParams::new(ForkSnapshot::Interrupted)
            .with_session_meta(RolloutSessionMeta::new(taken.clone())),
    )
    .await
    .unwrap_err();
    assert!(!is_kind(&error, SessionErrorKind::NotFound), "{error}");
    assert_eq!(std::fs::read(&path).unwrap(), before);
    // The refusal let go of the writer lock it took.
    directory
        .resume_thread(&ra_session::ResumeThreadParams::new(taken))
        .await
        .unwrap()
        .shutdown()
        .await
        .unwrap();
}

#[tokio::test]
async fn a_fork_in_the_directory_is_materialized_with_its_metadata_first_and_the_copy_after() {
    let directory = RolloutThreadDirectory::new(temp_dir("materialized"));
    let source = SessionId::new("source");
    let source_path = directory.rollout_path(&source).unwrap();

    // The source as its writer left it: with a checkpoint, and with a host event the persistence
    // policy would have dropped had a recorder written it.
    let mut writer = RolloutWriter::open(&source_path, source.clone())
        .await
        .unwrap();
    writer
        .append_session_meta(RolloutSessionMeta::new(source.clone()))
        .await
        .unwrap();
    writer
        .append(
            RolloutRunStarted::new(RunId::new("run-1"), AgentId::new("lead"))
                .with_input(vec![user("u1")]),
        )
        .await
        .unwrap();
    let allocator = RunState::start(RunId::new("run-1")).restore_event_seq_allocator(None);
    let exec = HostEvent::allocate(
        &allocator,
        AgentId::new("lead"),
        HostEventBody::Exec(ExecEvent::Started(ExecStartedEvent::new(
            ExecSessionId::generate(),
            "ls",
        ))),
    )
    .unwrap();
    writer.append_event(exec).await.unwrap();
    writer.append_item(said("a-1", "a1")).await.unwrap();
    writer.write_checkpoint().await.unwrap();
    writer
        .append(RolloutRunEnded::new(
            RunId::new("run-1"),
            RolloutRunEnd::Completed,
        ))
        .await
        .unwrap();
    writer.sync_all().await.unwrap();
    drop(writer);
    let source_records = RolloutReader::open(&source_path).read_all().await.unwrap();

    let forked = ForkedThread::fork(
        &directory,
        &source,
        &ForkThreadParams::new(ForkSnapshot::Interrupted)
            .with_session_meta(RolloutSessionMeta::new(SessionId::new("fork"))),
    )
    .await
    .unwrap();

    // Materialized before anything else is recorded, and readable as a thread of its own.
    let fork_path = directory.rollout_path(&SessionId::new("fork")).unwrap();
    let records = RolloutReader::open(&fork_path).read_all().await.unwrap();
    let RolloutPayload::SessionMeta(meta) = records[0].payload().unwrap() else {
        panic!("a rollout opens with its session metadata");
    };
    assert_eq!(meta.session_id(), &SessionId::new("fork"));
    assert_eq!(meta.forked_from_id(), Some(&source));
    let kept: Vec<_> = source_records
        .iter()
        .filter(|record| !matches!(record.type_name(), "checkpoint" | "event"))
        .cloned()
        .collect();
    assert_eq!(payloads(&records[1..]), payloads(&kept));
    assert!(
        records
            .windows(2)
            .all(|pair| pair[0].timeline_seq() < pair[1].timeline_seq())
    );
    assert_eq!(
        reconstruct_history(&records).unwrap(),
        reconstruct_history(&source_records).unwrap()
    );
    assert_eq!(
        forked.reconstruction(),
        &reconstruct_history(&source_records).unwrap()
    );
    let thread = directory
        .read_thread(&ReadThreadParams::new(SessionId::new("fork")))
        .await
        .unwrap();
    assert_eq!(thread.forked_from_id(), Some(&source));
    assert_eq!(
        RolloutReader::open(&source_path).read_all().await.unwrap(),
        source_records,
        "the source is left as it was"
    );

    // The fork's runs are appended after the copy, and it holds its rollout until it lets go.
    let run = RunId::new("run-2");
    forked.recorder().record(RolloutItem::RunStarted(
        RolloutRunStarted::new(run.clone(), AgentId::new("lead")).with_input(vec![user("u2")]),
    ));
    forked
        .recorder()
        .record(RolloutItem::RunEnded(RolloutRunEnded::new(
            run,
            RolloutRunEnd::Completed,
        )));
    forked.recorder().flush().await.unwrap();
    let error = directory
        .resume_thread(&ra_session::ResumeThreadParams::new(SessionId::new("fork")))
        .await
        .err()
        .expect("a live writer holds the fork");
    assert!(!is_kind(&error, SessionErrorKind::NotFound), "{error}");
    forked.recorder().shutdown().await.unwrap();
    let records = RolloutReader::open(&fork_path).read_all().await.unwrap();
    assert_eq!(positions(&records).len(), 2);
}

/// Codex's `fork_thread_twice_drops_to_first_message`: three runs, forked before the second user
/// message, then that fork forked before its first, leaves only what preceded it.
#[tokio::test]
async fn forking_twice_drops_to_the_first_message() {
    let directory = RolloutThreadDirectory::new(temp_dir("twice"));
    let source = SessionId::new("source");
    let recorder = RolloutFileRecorder::create_with_session_meta(
        directory.rollout_path(&source).unwrap(),
        RolloutSessionMeta::new(source.clone()),
    );
    for (run, text) in [("run-1", "first"), ("run-2", "second"), ("run-3", "third")] {
        let run = RunId::new(run);
        recorder.record(RolloutItem::RunStarted(
            RolloutRunStarted::new(run.clone(), AgentId::new("lead")).with_input(vec![user(text)]),
        ));
        recorder.record(RolloutItem::Item(said(&format!("{run}-answer"), text)));
        recorder.record(RolloutItem::RunEnded(RolloutRunEnded::new(
            run,
            RolloutRunEnd::Completed,
        )));
    }
    recorder.shutdown().await.unwrap();
    let base = RolloutReader::open(directory.rollout_path(&source).unwrap())
        .read_all()
        .await
        .unwrap();
    let base_positions = positions(&base);

    let fork1 = ForkedThread::fork(&directory, &source, &ForkThreadParams::new(1))
        .await
        .unwrap();
    fork1.recorder().shutdown().await.unwrap();
    let fork1_records = RolloutReader::open(directory.rollout_path(fork1.session_id()).unwrap())
        .read_all()
        .await
        .unwrap();
    assert_eq!(
        payloads(&fork1_records[1..]),
        payloads(&base[..base_positions[1].0])
    );

    let fork2 = ForkedThread::fork(&directory, fork1.session_id(), &ForkThreadParams::new(0))
        .await
        .unwrap();
    fork2.recorder().shutdown().await.unwrap();
    let fork2_records = RolloutReader::open(directory.rollout_path(fork2.session_id()).unwrap())
        .read_all()
        .await
        .unwrap();
    let fork1_positions = positions(&fork1_records);
    assert_eq!(
        payloads(&fork2_records[1..]),
        payloads(&fork1_records[..fork1_positions[0].0])
    );
    let RolloutPayload::SessionMeta(meta) = fork2_records[0].payload().unwrap() else {
        panic!("a rollout opens with its session metadata");
    };
    assert_eq!(meta.forked_from_id(), Some(fork1.session_id()));
    assert!(fork2.reconstruction().history().is_empty());
}
