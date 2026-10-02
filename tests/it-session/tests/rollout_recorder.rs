//! The rollout file recorder: Codex's recorder and its persistence policy over a `RolloutWriter`.

use std::{path::PathBuf, sync::Arc};

use ra_core::{
    agent::control::AgentPath,
    event::{
        AgentEvent, HostEvent, HostEventBody, SubAgentActivityEvent, SubAgentActivityKind,
        exec::{ExecEvent, ExecSessionId, ExecStartedEvent},
        file::{FileChangeKind, FileChangedEvent, FileEvent, FileReadEvent},
    },
    finish::FinishReason,
    hook::{HookDecision, HookEventName, HookReport, HookRunStatus},
    item::{AgentId, CallId, ItemId, Message, ModelInputItem, OutputPhase, RunItem, RunItemKind},
    session::{
        SessionId,
        rollout::{
            RolloutItem, RolloutModelUsage, RolloutRecorder, RolloutRunEnd, RolloutRunEnded,
            RolloutRunStarted, RolloutThreadSpawn, RolloutThreadStore, RolloutTurnContext,
        },
    },
    state::{EventSeqAllocator, RunId, RunState},
    usage::{RequestUsage, Usage},
};
use ra_session::{
    RolloutFileRecorder, RolloutPayload, RolloutReader, RolloutSessionMeta, RolloutThreadDirectory,
    RolloutWriter, is_persisted_rollout_item,
};
use serde_json::json;

fn temp_test_dir(test_name: &str) -> PathBuf {
    let dir = std::env::temp_dir()
        .join("rusty_agent_tests")
        .join("rollout_recorder")
        .join(test_name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("failed to create temp test directory");
    dir
}

fn allocator(run_id: &str) -> EventSeqAllocator {
    RunState::start(RunId::new(run_id)).restore_event_seq_allocator(None)
}

fn event(allocator: &EventSeqAllocator, body: impl Into<HostEventBody>) -> HostEvent {
    HostEvent::allocate(allocator, AgentId::new("lead"), body.into()).unwrap()
}

fn file_read(allocator: &EventSeqAllocator) -> HostEvent {
    event(
        allocator,
        FileEvent::Read(FileReadEvent::new(CallId::new("c-1"), "notes.md", 12)),
    )
}

fn file_changed(allocator: &EventSeqAllocator) -> HostEvent {
    event(
        allocator,
        FileEvent::Changed(FileChangedEvent::new(
            CallId::new("c-1"),
            "notes.md",
            FileChangeKind::Updated,
        )),
    )
}

fn exec_started(allocator: &EventSeqAllocator) -> HostEvent {
    event(
        allocator,
        ExecEvent::Started(ExecStartedEvent::new(ExecSessionId::generate(), "ls")),
    )
}

fn hook_report(allocator: &EventSeqAllocator) -> HostEvent {
    event(
        allocator,
        HookReport::new(
            "audit",
            HookEventName::SessionStart,
            HookRunStatus::Completed,
            HookDecision::Continue,
        ),
    )
}

fn sub_agent_started(allocator: &EventSeqAllocator) -> HostEvent {
    event(
        allocator,
        AgentEvent::SubAgentActivity(SubAgentActivityEvent::new(
            "c-2",
            AgentPath::root().join("worker").unwrap(),
            SubAgentActivityKind::Started,
        )),
    )
}

fn unknown_family(allocator: &EventSeqAllocator) -> HostEvent {
    event(
        allocator,
        HostEventBody::Unknown {
            family: "telemetry".to_owned(),
            data: json!({"kind": "sample"}),
        },
    )
}

fn message(id: &str, text: &str) -> RunItem {
    RunItem::new(
        ItemId::new(id),
        RunItemKind::Message(Message::assistant(text, OutputPhase::Final)),
    )
}

fn usage(input_tokens: u64) -> Usage {
    Usage::from_request(RequestUsage::new(input_tokens, 2))
}

#[test]
fn the_policy_keeps_records_and_drops_narration_as_codex_legacy_mode_does() {
    let seqs = allocator("run-1");
    let run = RunId::new("run-1");
    let kept = [
        RolloutItem::RunStarted(RolloutRunStarted::new(run.clone(), AgentId::new("lead"))),
        RolloutItem::TurnContext(RolloutTurnContext::new(run.clone(), 0)),
        RolloutItem::Item(message("m-1", "hello")),
        RolloutItem::ModelUsage(RolloutModelUsage::new(run.clone(), usage(1))),
        RolloutItem::RunEnded(RolloutRunEnded::new(run, RolloutRunEnd::Completed)),
        RolloutItem::Event(file_changed(&seqs)),
        RolloutItem::Event(sub_agent_started(&seqs)),
        RolloutItem::Event(unknown_family(&seqs)),
    ];
    for item in &kept {
        assert!(is_persisted_rollout_item(item), "{item:?}");
    }
    let dropped = [
        RolloutItem::Event(exec_started(&seqs)),
        RolloutItem::Event(file_read(&seqs)),
        RolloutItem::Event(hook_report(&seqs)),
    ];
    for item in &dropped {
        assert!(!is_persisted_rollout_item(item), "{item:?}");
    }
}

#[tokio::test]
async fn records_reach_the_file_in_order_once_flushed_and_narration_is_left_out() {
    let dir = temp_test_dir("order_and_policy");
    let session_id = SessionId::generate();
    let writer = RolloutWriter::create_for_session(&dir, session_id.clone())
        .await
        .unwrap();
    let path = writer.path().to_path_buf();
    let recorder = RolloutFileRecorder::spawn(writer);
    let seqs = allocator("run-1");
    let run = RunId::new("run-1");

    recorder.record(RolloutItem::RunStarted(
        RolloutRunStarted::new(run.clone(), AgentId::new("lead"))
            .with_input(vec![ModelInputItem::Message(Message::user("hi"))]),
    ));
    recorder.record(RolloutItem::TurnContext(
        RolloutTurnContext::new(run.clone(), 0).with_model("canonical-model"),
    ));
    recorder.record(RolloutItem::Event(exec_started(&seqs)));
    recorder.record(RolloutItem::ModelUsage(RolloutModelUsage::new(
        run.clone(),
        usage(10),
    )));
    recorder.record(RolloutItem::Event(file_read(&seqs)));
    recorder.record(RolloutItem::Event(file_changed(&seqs)));
    recorder.record(RolloutItem::Item(message("m-1", "done")));
    recorder.record(RolloutItem::Event(hook_report(&seqs)));
    recorder.record(RolloutItem::ModelUsage(RolloutModelUsage::new(
        run.clone(),
        usage(5),
    )));
    recorder.record(RolloutItem::RunEnded(
        RolloutRunEnded::new(run, RolloutRunEnd::Completed).with_finish_reason(FinishReason::Final),
    ));
    recorder.flush().await.unwrap();

    let reader = RolloutReader::open(&path);
    let records = reader.read_all().await.unwrap();
    let types: Vec<&str> = records.iter().map(|record| record.type_name()).collect();
    assert_eq!(
        types,
        vec![
            "run_started",
            "turn_context",
            "model_usage",
            "event",
            "item",
            "model_usage",
            "run_ended",
        ]
    );
    let seqs: Vec<u64> = records.iter().map(|record| record.timeline_seq()).collect();
    assert_eq!(seqs, (0..7).collect::<Vec<_>>());
    match records[0].payload().unwrap() {
        RolloutPayload::RunStarted(started) => {
            assert_eq!(started.agent_id().as_str(), "lead");
            assert_eq!(
                started.input(),
                &[ModelInputItem::Message(Message::user("hi"))]
            );
        }
        other => panic!("expected the run's start, got {other:?}"),
    }
    match records[6].payload().unwrap() {
        RolloutPayload::RunEnded(ended) => {
            assert_eq!(ended.end(), RolloutRunEnd::Completed);
            assert_eq!(ended.finish_reason(), Some(FinishReason::Final));
        }
        other => panic!("expected the run's end, got {other:?}"),
    }
    // The usage records are the ledger the reader totals.
    let summary = reader.scan_summary().await.unwrap();
    let totals = summary.usage_totals();
    assert_eq!(
        (
            totals.requests(),
            totals.input_tokens(),
            totals.output_tokens()
        ),
        (2, 15, 4)
    );
}

#[tokio::test]
async fn the_wire_names_of_run_records_are_stable() {
    let dir = temp_test_dir("wire_names");
    let writer = RolloutWriter::create_for_session(&dir, SessionId::generate())
        .await
        .unwrap();
    let path = writer.path().to_path_buf();
    let recorder = RolloutFileRecorder::spawn(writer);
    recorder.record(RolloutItem::RunStarted(
        RolloutRunStarted::new(RunId::new("run-2"), AgentId::new("lead"))
            .with_parent_run_id(RunId::new("run-1")),
    ));
    recorder.record(RolloutItem::RunEnded(
        RolloutRunEnded::new(RunId::new("run-2"), RolloutRunEnd::Failed).with_error("boom"),
    ));
    recorder.flush().await.unwrap();

    let lines: Vec<serde_json::Value> = std::fs::read_to_string(&path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(lines[0]["type"], "run_started");
    assert_eq!(
        lines[0]["payload"],
        json!({
            "schema_version": 1,
            "run_id": "run-2",
            "agent_id": "lead",
            "parent_run_id": "run-1"
        })
    );
    assert_eq!(lines[1]["type"], "run_ended");
    assert_eq!(
        lines[1]["payload"],
        json!({"schema_version": 1, "run_id": "run-2", "end": "failed", "error": "boom"})
    );
}

#[tokio::test]
async fn a_recorder_shared_by_clones_writes_one_file_and_a_reopen_continues_it() {
    let dir = temp_test_dir("clones_and_reopen");
    let session_id = SessionId::generate();
    let writer = RolloutWriter::create_for_session(&dir, session_id.clone())
        .await
        .unwrap();
    let path = writer.path().to_path_buf();
    let recorder = Arc::new(RolloutFileRecorder::spawn(writer));
    let clone = recorder.as_ref().clone();
    recorder.record(RolloutItem::Item(message("m-1", "one")));
    clone.record(RolloutItem::Item(message("m-2", "two")));
    clone.flush().await.unwrap();
    // Dropping every handle lets the writer task close the file and release its lock.
    drop(recorder);
    drop(clone);

    let reopened = loop {
        match RolloutWriter::open(&path, session_id.clone()).await {
            Ok(writer) => break writer,
            Err(_) => tokio::time::sleep(std::time::Duration::from_millis(10)).await,
        }
    };
    let recorder = RolloutFileRecorder::spawn(reopened);
    recorder.record(RolloutItem::Item(message("m-3", "three")));
    recorder.flush().await.unwrap();

    let records = RolloutReader::open(&path).read_all().await.unwrap();
    let texts: Vec<String> = records
        .iter()
        .filter_map(|record| match record.payload().unwrap() {
            RolloutPayload::Item(item) => match item.kind() {
                RunItemKind::Message(message) => Some(message.text_content()),
                _ => None,
            },
            _ => None,
        })
        .collect();
    assert_eq!(texts, vec!["one", "two", "three"]);
}

fn message_texts(records: &[ra_session::RolloutRecord]) -> Vec<String> {
    records
        .iter()
        .filter_map(|record| match record.payload().unwrap() {
            RolloutPayload::Item(item) => match item.kind() {
                RunItemKind::Message(message) => Some(message.text_content()),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

/// A rollout path whose directory cannot be created while a plain file stands where it should be,
/// as Codex's test blocks its sessions directory.
fn blocked_path(test_name: &str) -> (PathBuf, PathBuf) {
    let dir = temp_test_dir(test_name);
    let blocker = dir.join("sessions");
    std::fs::write(&blocker, b"not a directory").unwrap();
    (blocker.join("rollout.jsonl"), blocker)
}

#[tokio::test]
async fn a_failed_write_keeps_what_was_recorded_and_a_later_flush_writes_it_all() {
    let (path, blocker) = blocked_path("retry_after_failure");
    let recorder = RolloutFileRecorder::create(&path, SessionId::generate());
    recorder.record(RolloutItem::Item(message("m-1", "before the failure")));
    recorder.record(RolloutItem::Item(message("m-2", "during the failure")));
    let error = recorder
        .flush()
        .await
        .expect_err("a rollout that cannot be opened fails the flush");
    assert!(!path.exists());

    // Recorded while the file still cannot be written, and kept all the same.
    recorder.record(RolloutItem::Item(message("m-3", "still failing")));
    std::fs::remove_file(&blocker).unwrap();
    recorder.flush().await.unwrap();

    let records = RolloutReader::open(&path).read_all().await.unwrap();
    assert_eq!(
        message_texts(&records),
        vec!["before the failure", "during the failure", "still failing"],
        "nothing recorded before {error} was lost"
    );
    let seqs: Vec<u64> = records.iter().map(|record| record.timeline_seq()).collect();
    assert_eq!(seqs, vec![0, 1, 2]);
}

#[tokio::test]
async fn records_queued_when_the_last_handle_is_dropped_get_one_more_attempt() {
    let (path, blocker) = blocked_path("final_attempt");
    let recorder = RolloutFileRecorder::create(&path, SessionId::generate());
    recorder.record(RolloutItem::Item(message("m-1", "queued")));
    assert!(recorder.flush().await.is_err());
    std::fs::remove_file(&blocker).unwrap();
    drop(recorder);

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if path.exists()
            && message_texts(&RolloutReader::open(&path).read_all().await.unwrap())
                == vec!["queued".to_owned()]
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the queued record was never written"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn a_rollout_opened_on_first_write_continues_an_existing_file() {
    let dir = temp_test_dir("deferred_open_continues");
    let session_id = SessionId::generate();
    let path = dir.join("rollout.jsonl");
    let first = RolloutFileRecorder::create(&path, session_id.clone());
    first.record(RolloutItem::Item(message("m-1", "one")));
    first.flush().await.unwrap();
    drop(first);

    // The first task may hold the file's lock for a moment after its last handle went; the
    // recorder retries opening at every flush, so flushing again is all it takes.
    let second = RolloutFileRecorder::create(&path, session_id.clone());
    second.record(RolloutItem::Item(message("m-2", "two")));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while second.flush().await.is_err() {
        assert!(
            std::time::Instant::now() < deadline,
            "the rollout never reopened"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let records = RolloutReader::open(&path).read_all().await.unwrap();
    assert_eq!(message_texts(&records), vec!["one", "two"]);
}

/// A write that fails on an open file keeps its record queued for the file the next barrier
/// reopens, as Codex's writer state keeps the unwritten suffix.
///
/// The path is a symlink to `/dev/full`, which fails every write with `ENOSPC` and is the only
/// always-failing write target available without root; replacing the link with nothing lets the
/// reopen create a real file. Linux-only for that reason — macOS has no equivalent, and on it the
/// tests above only exercise a failure to open.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_write_that_fails_on_an_open_file_keeps_its_record_for_the_reopened_one() {
    let dir = temp_test_dir("append_failure");
    let path = dir.join("rollout.jsonl");
    std::os::unix::fs::symlink("/dev/full", &path).unwrap();
    let session_id = SessionId::generate();
    let Ok(writer) = RolloutWriter::open(&path, session_id).await else {
        // No flock on the device node, or the node is unavailable in this sandbox.
        return;
    };
    let recorder = RolloutFileRecorder::spawn(writer);
    recorder.record(RolloutItem::Item(message("m-1", "kept")));
    assert!(
        recorder.flush().await.is_err(),
        "every write to /dev/full fails"
    );

    std::fs::remove_file(&path).unwrap();
    recorder.flush().await.unwrap();
    let records = RolloutReader::open(&path).read_all().await.unwrap();
    assert_eq!(message_texts(&records), vec!["kept"]);
}

// ---------------------------------------------------------------------------------------------
// The threads of an agent tree
// ---------------------------------------------------------------------------------------------

fn thread_spawn(parent: &str, depth: u32, path: &AgentPath) -> RolloutThreadSpawn {
    RolloutThreadSpawn::new(
        SessionId::new("session-root"),
        SessionId::new(parent),
        depth,
        path.clone(),
    )
}

#[tokio::test]
async fn session_metadata_is_written_first_into_a_new_rollout_and_not_into_an_existing_one() {
    let dir = temp_test_dir("session_meta_first");
    let path = dir.join("rollout-session-1.jsonl");
    let meta = || RolloutSessionMeta::new(SessionId::new("session-1")).with_cwd("/work");

    let recorder = RolloutFileRecorder::create_with_session_meta(&path, meta());
    recorder.record(RolloutItem::RunStarted(RolloutRunStarted::new(
        RunId::new("run-1"),
        AgentId::new("lead"),
    )));
    recorder.flush().await.unwrap();
    drop(recorder);
    // Reopened, as a resumed session is: its metadata is already there.
    let recorder = RolloutFileRecorder::create_with_session_meta(&path, meta());
    recorder.record(RolloutItem::RunStarted(RolloutRunStarted::new(
        RunId::new("run-2"),
        AgentId::new("lead"),
    )));
    recorder.flush().await.unwrap();

    let records = RolloutReader::open(&path).read_all().await.unwrap();
    let types: Vec<&str> = records.iter().map(|record| record.type_name()).collect();
    assert_eq!(types, vec!["session_meta", "run_started", "run_started"]);
    let read = RolloutReader::open(&path)
        .session_meta()
        .await
        .unwrap()
        .unwrap();
    assert_eq!(read.session_id(), &SessionId::new("session-1"));
    assert_eq!(read.cwd(), Some("/work"));
    assert!(read.thread_spawn().is_none());
}

#[tokio::test]
async fn session_metadata_that_could_not_be_written_yet_is_still_written_first() {
    let (path, blocker) = blocked_path("session_meta_after_failure");
    let recorder = RolloutFileRecorder::create_with_session_meta(
        &path,
        RolloutSessionMeta::new(SessionId::new("session-1")),
    );
    recorder.record(RolloutItem::RunStarted(RolloutRunStarted::new(
        RunId::new("run-1"),
        AgentId::new("lead"),
    )));
    assert!(recorder.flush().await.is_err());
    std::fs::remove_file(&blocker).unwrap();
    recorder.flush().await.unwrap();

    let types: Vec<String> = RolloutReader::open(&path)
        .read_all()
        .await
        .unwrap()
        .iter()
        .map(|record| record.type_name().to_owned())
        .collect();
    assert_eq!(types, vec!["session_meta", "run_started"]);
}

#[tokio::test]
async fn only_a_complete_first_record_of_session_metadata_is_read_as_one() {
    let dir = temp_test_dir("session_meta_reads");
    let read = |name: &str| {
        let path = dir.join(name);
        async move { RolloutReader::open(path).session_meta().await }
    };
    assert_eq!(read("missing.jsonl").await.unwrap(), None);

    std::fs::write(dir.join("empty.jsonl"), "").unwrap();
    assert_eq!(read("empty.jsonl").await.unwrap(), None);

    let mut writer = RolloutWriter::open(dir.join("no-meta.jsonl"), SessionId::new("s"))
        .await
        .unwrap();
    writer.append_item(message("m-1", "hi")).await.unwrap();
    drop(writer);
    assert_eq!(read("no-meta.jsonl").await.unwrap(), None);

    // A first line still being written is not read as anything yet.
    std::fs::write(dir.join("torn.jsonl"), "{\"timeline_seq\":0,").unwrap();
    assert_eq!(read("torn.jsonl").await.unwrap(), None);
    // One that is complete and unreadable is corrupt.
    std::fs::write(dir.join("corrupt.jsonl"), "not json\n").unwrap();
    assert!(read("corrupt.jsonl").await.is_err());
}

#[test]
fn a_spawned_threads_metadata_says_where_it_was_spawned_from_on_the_wire() {
    let worker = AgentPath::root().join("worker").unwrap();
    let meta = RolloutSessionMeta::new(SessionId::new("sess-worker"))
        .with_created_at(ra_core::event::EventTimestamp::from_millis(7))
        .with_thread_spawn(
            thread_spawn("session-root", 1, &worker).with_agent_type(AgentId::new("explorer")),
        );
    let wire = serde_json::to_value(&meta).unwrap();
    assert_eq!(
        wire,
        json!({
            "schema_version": 1,
            "session_id": "sess-worker",
            "created_at": 7,
            "thread_spawn": {
                "schema_version": 1,
                "root_session_id": "session-root",
                "parent_session_id": "session-root",
                "depth": 1,
                "agent_path": "/root/worker",
                "agent_type": "explorer"
            }
        })
    );
    let restored: RolloutSessionMeta = serde_json::from_value(wire).unwrap();
    assert_eq!(restored, meta);
    assert_eq!(
        restored.parent_session_id(),
        Some(&SessionId::new("session-root"))
    );
    // A root's metadata, as earlier builds wrote it, names no parent.
    let root: RolloutSessionMeta =
        serde_json::from_value(json!({"schema_version": 1, "session_id": "session-root"})).unwrap();
    assert_eq!(root.parent_session_id(), None);
}

#[tokio::test]
async fn a_thread_directory_creates_each_threads_rollout_and_lists_a_sessions_direct_children() {
    let dir = temp_test_dir("thread_directory");
    let store = RolloutThreadDirectory::new(dir.join("threads"));
    assert!(
        store
            .children(&SessionId::new("session-root"))
            .await
            .unwrap()
            .is_empty(),
        "a directory not created yet holds no children"
    );
    let worker = AgentPath::root().join("worker").unwrap();
    let helper = worker.join("helper").unwrap();
    let other = AgentPath::root().join("other").unwrap();
    for (session, spawn) in [
        ("sess-1-worker", thread_spawn("session-root", 1, &worker)),
        ("sess-2-helper", thread_spawn("sess-1-worker", 2, &helper)),
        ("sess-3-other", thread_spawn("session-root", 1, &other)),
    ] {
        let recorder = store
            .create_thread(&SessionId::new(session), &spawn)
            .await
            .unwrap();
        recorder.record(RolloutItem::RunStarted(RolloutRunStarted::new(
            RunId::new(format!("run-{session}")),
            AgentId::new("agent"),
        )));
        recorder.flush().await.unwrap();
    }
    // Not listed: a thread whose first record is not written yet, a rollout that does not start
    // with metadata, one whose first record is corrupt, and files that are not rollouts.
    let _unwritten = store
        .create_thread(
            &SessionId::new("sess-4-unwritten"),
            &thread_spawn("session-root", 1, &AgentPath::root().join("late").unwrap()),
        )
        .await
        .unwrap();
    std::fs::write(store.path().join("rollout-sess-5.jsonl"), "not json\n").unwrap();
    let mut no_meta = RolloutWriter::open(
        store.path().join("rollout-sess-6.jsonl"),
        SessionId::new("sess-6"),
    )
    .await
    .unwrap();
    no_meta.append_item(message("m-1", "hi")).await.unwrap();
    std::fs::write(store.path().join("notes.txt"), "hello\n").unwrap();
    std::fs::create_dir(store.path().join("rollout-dir.jsonl")).unwrap();

    let listed = |parent: &'static str| {
        let store = store.clone();
        async move {
            store
                .children(&SessionId::new(parent))
                .await
                .unwrap()
                .into_iter()
                .map(|(reader, meta)| {
                    assert_eq!(
                        reader.path(),
                        store.rollout_path(meta.session_id()).unwrap()
                    );
                    meta.session_id().as_str().to_owned()
                })
                .collect::<Vec<_>>()
        }
    };
    assert_eq!(
        listed("session-root").await,
        vec!["sess-1-worker", "sess-3-other"]
    );
    assert_eq!(listed("sess-1-worker").await, vec!["sess-2-helper"]);
    assert!(listed("sess-2-helper").await.is_empty());

    let meta = RolloutReader::open(
        store
            .rollout_path(&SessionId::new("sess-2-helper"))
            .unwrap(),
    )
    .session_meta()
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        meta.thread_spawn(),
        Some(&thread_spawn("sess-1-worker", 2, &helper))
    );
    assert!(store.rollout_path(&SessionId::new("../escape")).is_err());
}
