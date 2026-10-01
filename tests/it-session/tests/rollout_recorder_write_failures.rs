//! Rollout writes that fail on an open file, provoked with a file-size limit.
//!
//! Linux takes a write up to the process's file-size limit and fails the rest with `EFBIG` (once
//! `SIGXFSZ` is ignored). A limit at the file's current size fails the next record outright; a
//! limit one byte short of the record's line leaves the line complete but for its newline and fails
//! the write all the same. The limit applies to the whole process, so both cases run one after the
//! other in a single test, in a binary of their own. macOS takes a write that starts below the
//! limit whole, so it cannot produce the second case, and the test is Linux-only.
//!
//! `rollout_recorder.rs` provokes the first case with `/dev/full` as well; only a regular file can
//! hold a line that landed, which the second case needs.
#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};

use ra_core::{
    event::EventTimestamp,
    item::{AgentId, ItemId, Message, ModelInputItem, OutputPhase, RunItem, RunItemKind},
    session::{
        SessionId,
        rollout::{RolloutItem, RolloutRecorder, RolloutRunStarted},
    },
    state::RunId,
};
use ra_session::{
    RolloutFileRecorder, RolloutPayload, RolloutReader, RolloutRecord, RolloutWriter,
    reconstruct_history,
};

fn set_file_size_limit(limit: libc::rlim_t) {
    let mut current = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: both calls only read or write the `rlimit` passed by reference.
    unsafe {
        assert_eq!(libc::getrlimit(libc::RLIMIT_FSIZE, &mut current), 0);
        current.rlim_cur = limit.min(current.rlim_max);
        assert_eq!(libc::setrlimit(libc::RLIMIT_FSIZE, &current), 0);
    }
}

fn temp_rollout(name: &str) -> PathBuf {
    let dir = std::env::temp_dir()
        .join("rusty_agent_tests")
        .join("rollout_recorder_write_failures")
        .join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("rollout.jsonl")
}

fn file_len(path: &Path) -> u64 {
    std::fs::metadata(path).unwrap().len()
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

/// A recorder over a rollout that already holds one record, so the next sequence number is known.
async fn recorder_with_a_record(path: &Path) -> (RolloutFileRecorder, u64) {
    let writer = RolloutWriter::open(path, SessionId::new("session-1"))
        .await
        .unwrap();
    let recorder = RolloutFileRecorder::spawn(writer);
    recorder.record(RolloutItem::Item(said("a-0", "earlier")));
    recorder.flush().await.unwrap();
    let records = RolloutReader::open(path).read_all().await.unwrap();
    (recorder, records.last().unwrap().timeline_seq() + 1)
}

#[tokio::test]
async fn writes_that_fail_on_an_open_file_are_retried_once_each() {
    // SAFETY: ignoring a signal installs no handler code; the write then fails with `EFBIG`.
    unsafe {
        libc::signal(libc::SIGXFSZ, libc::SIG_IGN);
    }
    a_record_that_did_not_land_is_kept_for_the_reopened_file().await;
    a_run_start_that_landed_although_its_write_failed_is_recorded_once().await;
}

/// As Codex's writer state keeps the unwritten suffix, a record whose write failed stays queued
/// and is written once the next barrier reopens the file.
async fn a_record_that_did_not_land_is_kept_for_the_reopened_file() {
    let path = temp_rollout("not_landed");
    let (recorder, _) = recorder_with_a_record(&path).await;
    let on_disk = file_len(&path);

    set_file_size_limit(on_disk);
    recorder.record(RolloutItem::Item(said("a-1", "kept")));
    assert!(recorder.flush().await.is_err(), "the file is at its limit");
    assert_eq!(file_len(&path), on_disk, "nothing landed");
    set_file_size_limit(libc::RLIM_INFINITY);

    recorder.flush().await.unwrap();
    let records = RolloutReader::open(&path).read_all().await.unwrap();
    let history = reconstruct_history(&records).unwrap().into_history();
    assert_eq!(
        history,
        [
            said("a-0", "earlier").to_model_input().unwrap(),
            said("a-1", "kept").to_model_input().unwrap(),
        ]
    );
}

/// A record whose write failed after its line reached the file is not written a second time.
async fn a_run_start_that_landed_although_its_write_failed_is_recorded_once() {
    let path = temp_rollout("landed");
    let (recorder, next_seq) = recorder_with_a_record(&path).await;
    let on_disk = file_len(&path);

    let started = RolloutRunStarted::new(RunId::new("run-1"), AgentId::new("lead"))
        .with_input(vec![user("user 1")]);
    let line_len = serde_json::to_string(
        &RolloutRecord::new(
            next_seq,
            EventTimestamp::now(),
            RolloutPayload::RunStarted(started.clone()),
        )
        .unwrap(),
    )
    .unwrap()
    .len() as u64
        + 1;

    // Room for the record's line but not for its newline.
    set_file_size_limit(on_disk + line_len - 1);
    recorder.record(RolloutItem::RunStarted(started));
    assert!(
        recorder.flush().await.is_err(),
        "the newline cannot be written"
    );
    assert_eq!(
        file_len(&path),
        on_disk + line_len - 1,
        "the record landed without its newline"
    );
    set_file_size_limit(libc::RLIM_INFINITY);

    recorder.record(RolloutItem::Item(said("a-1", "answer 1")));
    recorder.flush().await.unwrap();

    let records = RolloutReader::open(&path).read_all().await.unwrap();
    let starts = records
        .iter()
        .filter(|record| record.type_name() == "run_started")
        .count();
    assert_eq!(starts, 1);
    assert_eq!(
        reconstruct_history(&records).unwrap().history()[1..],
        [
            user("user 1"),
            said("a-1", "answer 1").to_model_input().unwrap()
        ]
    );
}
