//! Rebuilding a session's history from its rollout, as Codex rebuilds a thread's history, and
//! cutting a rollout at a run boundary, as Codex cuts one at a turn.

use std::path::PathBuf;

use ra_core::{
    error::Error,
    event::{EventTimestamp, HostEvent},
    finish::FinishReason,
    item::{
        AgentId, CallId, Compaction, ItemId, Message, ModelInputItem, OutputPhase,
        ProviderCompaction, RunItem, RunItemKind, ToolApproval, ToolCall, ToolCallOutput,
    },
    session::{
        SessionId,
        rollout::{
            RolloutModelUsage, RolloutRunEnd, RolloutRunEnded, RolloutRunStarted,
            RolloutTurnContext,
        },
    },
    state::{RunId, RunState},
    usage::{RequestUsage, Usage},
};
use ra_session::{
    RolloutPayload, RolloutReader, RolloutRecord, RolloutSessionMeta, RolloutWriter,
    reconstruct_history, truncate_rollout_after_run, truncate_rollout_before_run,
};
use serde_json::json;

// ---------------------------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------------------------

/// Records in the order a session wrote them.
#[derive(Default)]
struct Rollout(Vec<RolloutRecord>);

impl Rollout {
    fn push(&mut self, payload: impl Into<RolloutPayload>) -> &mut Self {
        let seq = self.0.len() as u64;
        self.0.push(
            RolloutRecord::new(seq, EventTimestamp::from_millis(seq), payload.into()).unwrap(),
        );
        self
    }

    fn start(&mut self, run: &str, input: &[&str]) -> &mut Self {
        self.push(
            RolloutRunStarted::new(RunId::new(run), AgentId::new("lead"))
                .with_input(input.iter().map(|text| user(text)).collect()),
        )
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

fn call(id: &str, call_id: &str) -> RunItem {
    RunItem::new(
        ItemId::new(id),
        RunItemKind::ToolCall(ToolCall::new(CallId::new(call_id), "deploy", json!({}))),
    )
}

fn output(id: &str, call_id: &str) -> RunItem {
    RunItem::new(
        ItemId::new(id),
        RunItemKind::ToolCallOutput(ToolCallOutput::new(CallId::new(call_id), json!("done"))),
    )
}

fn compaction(id: &str, summary: &str, replaces: &[&str]) -> RunItem {
    RunItem::new(
        ItemId::new(id),
        RunItemKind::Compaction(Compaction::new(
            summary,
            replaces.iter().map(|id| ItemId::new(*id)).collect(),
        )),
    )
}

fn provider_compaction(id: &str) -> RunItem {
    RunItem::new(
        ItemId::new(id),
        RunItemKind::ProviderCompaction(ProviderCompaction::new(
            "test-provider",
            json!({"type": "compaction", "encrypted_content": "opaque"}),
        )),
    )
}

fn model_input(items: &[RunItem]) -> Vec<ModelInputItem> {
    items.iter().filter_map(RunItem::to_model_input).collect()
}

fn history(records: &[RolloutRecord]) -> Vec<ModelInputItem> {
    reconstruct_history(records).unwrap().into_history()
}

fn temp_test_dir(test_name: &str) -> PathBuf {
    let dir = std::env::temp_dir()
        .join("rusty_agent_tests")
        .join("rollout_reconstruction")
        .join(test_name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("failed to create temp test directory");
    dir
}

// ---------------------------------------------------------------------------------------------
// History
// ---------------------------------------------------------------------------------------------

#[test]
fn history_is_each_runs_new_input_followed_by_its_records() {
    let mut rollout = Rollout::default();
    rollout
        .push(RolloutSessionMeta::new(SessionId::new("sess-1")))
        .start("run-1", &["first"])
        .push(RolloutTurnContext::new(RunId::new("run-1"), 0).with_model("model-a"))
        .item(call("a-1", "c-1"))
        .item(output("a-2", "c-1"))
        .item(said("a-3", "one"))
        .push(RolloutModelUsage::new(
            RunId::new("run-1"),
            Usage::from_request(RequestUsage::new(10, 1)),
        ))
        .push(
            RolloutRunEnded::new(RunId::new("run-1"), RolloutRunEnd::Completed)
                .with_finish_reason(FinishReason::Final),
        )
        .start("run-2", &["second"])
        .push(RolloutTurnContext::new(RunId::new("run-2"), 0).with_model("model-b"))
        .item(said("b-1", "two"))
        .end("run-2", RolloutRunEnd::Completed);

    let rebuilt = reconstruct_history(&rollout.records()).unwrap();

    let mut expected = vec![user("first")];
    expected.extend(model_input(&[
        call("a-1", "c-1"),
        output("a-2", "c-1"),
        said("a-3", "one"),
    ]));
    expected.push(user("second"));
    expected.extend(model_input(&[said("b-1", "two")]));
    assert_eq!(rebuilt.history(), expected);

    let last = rebuilt.last_run().unwrap();
    assert_eq!(last.run_id(), &RunId::new("run-2"));
    assert_eq!(last.agent_id(), &AgentId::new("lead"));
    assert_eq!(last.end().unwrap().end(), RolloutRunEnd::Completed);
    assert_eq!(rebuilt.turn_context().unwrap().model(), Some("model-b"));
}

#[test]
fn an_empty_rollout_has_no_history_and_no_run() {
    let rebuilt = reconstruct_history(&[]).unwrap();
    assert!(rebuilt.history().is_empty());
    assert!(rebuilt.last_run().is_none());
    assert!(rebuilt.turn_context().is_none());
}

#[test]
fn a_record_recorded_again_under_its_id_replaces_the_first_copy_where_it_was() {
    let commentary = RunItem::new(
        ItemId::new("a-1"),
        RunItemKind::Message(Message::assistant("checking", OutputPhase::Commentary)),
    );
    let mut rollout = Rollout::default();
    rollout
        .start("run-1", &["go"])
        // Recorded before the tool started, then again once settlement fixed its phase.
        .item(commentary)
        .item(call("a-2", "c-1"))
        .item(said("a-1", "checking"))
        .item(output("a-3", "c-1"))
        // A write retried after it had reached the file.
        .item(output("a-3", "c-1"))
        .end("run-1", RolloutRunEnd::Completed);

    assert_eq!(
        history(&rollout.records())[1..],
        model_input(&[
            said("a-1", "checking"),
            call("a-2", "c-1"),
            output("a-3", "c-1"),
        ])
    );
}

#[test]
fn item_ids_are_only_unique_within_a_run() {
    let mut rollout = Rollout::default();
    rollout
        .start("run-1", &["one"])
        .item(said("context-0.0", "first run"))
        .end("run-1", RolloutRunEnd::Completed)
        .start("run-2", &["two"])
        .item(said("context-0.0", "second run"))
        .end("run-2", RolloutRunEnd::Completed);

    assert_eq!(
        history(&rollout.records()),
        vec![
            user("one"),
            model_input(&[said("context-0.0", "first run")])[0].clone(),
            user("two"),
            model_input(&[said("context-0.0", "second run")])[0].clone(),
        ]
    );
}

#[test]
fn a_continuation_base_replaces_what_its_run_recorded_and_keeps_earlier_runs() {
    let mut rollout = Rollout::default();
    rollout
        .start("run-0", &["earlier"])
        .item(said("a-1", "earlier answer"))
        .end("run-0", RolloutRunEnd::Completed)
        .start("run-1", &["user 1"])
        .item(said("b-1", "answer 1"))
        .end("run-1", RolloutRunEnd::Completed);
    // Continued from its checkpoint on the caller's projection of the run plus a new message.
    let mut base = vec![user("user 1")];
    base.extend(model_input(&[said("b-1", "answer 1")]));
    base.push(user("user 2"));
    rollout
        .push(
            RolloutRunStarted::new(RunId::new("run-1"), AgentId::new("lead"))
                .with_continuation_base(base),
        )
        .item(said("b-2", "answer 2"))
        .end("run-1", RolloutRunEnd::Completed);

    let mut expected = vec![user("earlier")];
    expected.extend(model_input(&[said("a-1", "earlier answer")]));
    expected.push(user("user 1"));
    expected.extend(model_input(&[said("b-1", "answer 1")]));
    expected.push(user("user 2"));
    expected.extend(model_input(&[said("b-2", "answer 2")]));
    assert_eq!(history(&rollout.records()), expected);
}

#[test]
fn a_continuation_base_is_marked_on_the_wire_and_plain_input_is_not() {
    let base = RolloutRecord::new(
        0,
        EventTimestamp::from_millis(0),
        RolloutRunStarted::new(RunId::new("run-1"), AgentId::new("lead"))
            .with_continuation_base(vec![user("all of it")])
            .into(),
    )
    .unwrap();
    assert_eq!(base.payload_value()["continuation_base"], json!(true));
    let RolloutPayload::RunStarted(read) = base.payload().unwrap() else {
        panic!("a run start");
    };
    assert!(read.input_is_continuation_base());

    let plain = RolloutRecord::new(
        0,
        EventTimestamp::from_millis(0),
        RolloutRunStarted::new(RunId::new("run-1"), AgentId::new("lead"))
            .with_input(vec![user("new")])
            .into(),
    )
    .unwrap();
    assert!(plain.payload_value().get("continuation_base").is_none());
    let RolloutPayload::RunStarted(read) = plain.payload().unwrap() else {
        panic!("a run start");
    };
    assert!(!read.input_is_continuation_base());
}

// ---------------------------------------------------------------------------------------------
// Compaction
// ---------------------------------------------------------------------------------------------

#[test]
fn a_compaction_drops_the_records_it_names_from_its_own_run() {
    let mut rollout = Rollout::default();
    rollout
        .start("run-1", &["one"])
        .item(said("x-1", "first run, kept"))
        .end("run-1", RolloutRunEnd::Completed)
        .start("run-2", &["two"])
        .item(call("x-1", "c-1"))
        .item(output("x-2", "c-1"))
        .item(compaction("x-3", "earlier work", &["x-1", "x-2"]))
        .item(said("x-4", "after"))
        .end("run-2", RolloutRunEnd::Completed);

    let mut expected = vec![user("one")];
    expected.extend(model_input(&[said("x-1", "first run, kept")]));
    expected.push(user("two"));
    expected.extend(model_input(&[
        compaction("x-3", "earlier work", &["x-1", "x-2"]),
        said("x-4", "after"),
    ]));
    assert_eq!(history(&rollout.records()), expected);
}

#[test]
fn a_compaction_that_replaces_an_earlier_one_drops_it_too() {
    let mut rollout = Rollout::default();
    rollout
        .start("run-1", &["go"])
        .item(said("m-1", "one"))
        .item(compaction("s-1", "first summary", &["m-1"]))
        .item(said("m-2", "two"))
        .item(compaction("s-2", "second summary", &["m-1", "s-1", "m-2"]))
        .item(said("m-3", "three"))
        .end("run-1", RolloutRunEnd::Completed);

    let mut expected = vec![user("go")];
    expected.extend(model_input(&[
        compaction("s-2", "second summary", &["m-1", "s-1", "m-2"]),
        said("m-3", "three"),
    ]));
    assert_eq!(history(&rollout.records()), expected);
}

#[test]
fn history_starts_at_the_last_provider_compaction() {
    let mut rollout = Rollout::default();
    rollout
        .start("run-1", &["one"])
        .item(said("p-1", "one"))
        .item(provider_compaction("p-2"))
        .item(said("p-3", "after the first"))
        .end("run-1", RolloutRunEnd::Completed)
        .start("run-2", &["two"])
        .item(said("q-1", "two"))
        .item(provider_compaction("q-2"))
        .item(said("q-3", "after the second"))
        .end("run-2", RolloutRunEnd::Completed);

    assert_eq!(
        history(&rollout.records()),
        model_input(&[provider_compaction("q-2"), said("q-3", "after the second")])
    );
}

// ---------------------------------------------------------------------------------------------
// Runs that did not complete
// ---------------------------------------------------------------------------------------------

#[test]
fn a_run_cancelled_while_its_tool_ran_keeps_the_call_it_made() {
    let mut rollout = Rollout::default();
    rollout
        .start("run-1", &["deploy it"])
        .item(said("a-1", "deploying"))
        .item(call("a-2", "c-1"))
        .end("run-1", RolloutRunEnd::Cancelled);

    let rebuilt = reconstruct_history(&rollout.records()).unwrap();
    let mut expected = vec![user("deploy it")];
    expected.extend(model_input(&[said("a-1", "deploying"), call("a-2", "c-1")]));
    assert_eq!(rebuilt.history(), expected);
    assert_eq!(
        rebuilt.last_run().unwrap().end().unwrap().end(),
        RolloutRunEnd::Cancelled
    );
}

#[test]
fn a_failed_run_keeps_its_records_and_its_error() {
    let mut rollout = Rollout::default();
    rollout
        .start("run-1", &["go"])
        .item(said("a-1", "partial"))
        .push(
            RolloutRunEnded::new(RunId::new("run-1"), RolloutRunEnd::Failed)
                .with_error("provider went away"),
        );

    let rebuilt = reconstruct_history(&rollout.records()).unwrap();
    assert_eq!(rebuilt.history().len(), 2);
    let end = rebuilt.last_run().unwrap().end().unwrap();
    assert_eq!(end.end(), RolloutRunEnd::Failed);
    assert_eq!(end.error(), Some("provider went away"));
}

#[test]
fn a_run_that_recorded_no_end_reports_none_and_keeps_its_records() {
    let mut rollout = Rollout::default();
    rollout
        .start("run-1", &["one"])
        .item(said("a-1", "done"))
        .end("run-1", RolloutRunEnd::Completed)
        .start("run-2", &["two"])
        .item(call("b-1", "c-1"));

    let rebuilt = reconstruct_history(&rollout.records()).unwrap();
    let last = rebuilt.last_run().unwrap();
    assert_eq!(last.run_id(), &RunId::new("run-2"));
    assert!(last.end().is_none());
    assert_eq!(
        rebuilt.history().last(),
        model_input(&[call("b-1", "c-1")]).last()
    );
}

#[test]
fn a_run_paused_for_approval_and_continued_is_one_run_without_its_approval_record() {
    let approval = RunItem::new(
        ItemId::new("a-2"),
        RunItemKind::ToolApproval(ToolApproval::new(CallId::new("c-1"), "deploy", json!({}))),
    );
    let mut rollout = Rollout::default();
    rollout
        .start("run-1", &["deploy it"])
        .item(call("a-1", "c-1"))
        .item(approval)
        .end("run-1", RolloutRunEnd::Interrupted);

    let paused = reconstruct_history(&rollout.records()).unwrap();
    assert_eq!(
        paused.last_run().unwrap().end().unwrap().end(),
        RolloutRunEnd::Interrupted
    );
    assert_eq!(paused.history().len(), 2, "the approval is not model input");

    rollout
        .start("run-1", &[])
        .item(output("a-3", "c-1"))
        .item(said("a-4", "shipped"))
        .end("run-1", RolloutRunEnd::Completed);

    let continued = reconstruct_history(&rollout.records()).unwrap();
    let mut expected = vec![user("deploy it")];
    expected.extend(model_input(&[
        call("a-1", "c-1"),
        output("a-3", "c-1"),
        said("a-4", "shipped"),
    ]));
    assert_eq!(continued.history(), expected);
    assert_eq!(
        continued.last_run().unwrap().end().unwrap().end(),
        RolloutRunEnd::Completed
    );
}

// ---------------------------------------------------------------------------------------------
// What is not history
// ---------------------------------------------------------------------------------------------

#[test]
fn events_usage_and_unknown_records_are_not_history() {
    let allocator = RunState::start(RunId::new("run-1")).restore_event_seq_allocator(None);
    let event = HostEvent::allocate(
        &allocator,
        AgentId::new("lead"),
        ra_core::event::file::FileEvent::Changed(ra_core::event::file::FileChangedEvent::new(
            CallId::new("c-1"),
            "notes.md",
            ra_core::event::file::FileChangeKind::Updated,
        ))
        .into(),
    )
    .unwrap();
    let mut rollout = Rollout::default();
    rollout
        .start("run-1", &["go"])
        .push(event)
        .push(RolloutPayload::Unknown {
            type_name: "from_a_newer_build".to_owned(),
            data: json!({"anything": true}),
        })
        .push(RolloutModelUsage::new(
            RunId::new("run-1"),
            Usage::from_request(RequestUsage::new(3, 1)),
        ))
        .item(said("a-1", "done"))
        .end("run-1", RolloutRunEnd::Completed);

    let mut expected = vec![user("go")];
    expected.extend(model_input(&[said("a-1", "done")]));
    assert_eq!(history(&rollout.records()), expected);
}

#[test]
fn a_session_record_that_cannot_be_read_fails_the_rebuild() {
    let mut rollout = Rollout::default();
    rollout
        .start("run-1", &["go"])
        .push(RolloutPayload::Unknown {
            type_name: "item".to_owned(),
            data: json!({"not": "a run item"}),
        });

    let error: Error = reconstruct_history(&rollout.records()).unwrap_err();
    assert!(
        error.to_string().contains("corrupted item payload"),
        "{error}"
    );
}

// ---------------------------------------------------------------------------------------------
// Truncation at a run boundary
// ---------------------------------------------------------------------------------------------

fn three_runs() -> Rollout {
    let mut rollout = Rollout::default();
    rollout
        .push(RolloutSessionMeta::new(SessionId::new("sess-1")))
        .start("run-1", &["one"])
        .item(said("a-1", "first"))
        .end("run-1", RolloutRunEnd::Completed)
        .start("run-2", &["two"])
        .item(call("b-1", "c-1"))
        .end("run-2", RolloutRunEnd::Interrupted)
        .start("run-2", &[])
        .item(output("b-2", "c-1"))
        .item(said("b-3", "second"))
        .end("run-2", RolloutRunEnd::Completed)
        .start("run-3", &["three"])
        .item(said("c-1", "third"))
        .end("run-3", RolloutRunEnd::Completed);
    rollout
}

#[test]
fn truncating_before_a_run_rebuilds_the_session_as_it_stood_before_it_started() {
    let records = three_runs().records();
    let before = truncate_rollout_before_run(records, &RunId::new("run-2")).unwrap();

    assert_eq!(before.len(), 4, "session meta and the whole of run-1");
    let rebuilt = reconstruct_history(&before).unwrap();
    let mut expected = vec![user("one")];
    expected.extend(model_input(&[said("a-1", "first")]));
    assert_eq!(rebuilt.history(), expected);
    assert_eq!(rebuilt.last_run().unwrap().run_id(), &RunId::new("run-1"));
}

#[test]
fn truncating_after_a_run_keeps_every_segment_of_it_and_cuts_at_the_next_run() {
    let mut rollout = three_runs();
    let records = rollout.records();
    let after = truncate_rollout_after_run(records.clone(), &RunId::new("run-2")).unwrap();
    assert_eq!(after, records[..11]);
    let rebuilt = reconstruct_history(&after).unwrap();
    assert_eq!(rebuilt.last_run().unwrap().run_id(), &RunId::new("run-2"));
    assert_eq!(
        history(&after).last(),
        model_input(&[said("b-3", "second")]).last()
    );

    // An event of the run recorded after it ended is still the run's.
    rollout.0.truncate(11);
    let allocator = RunState::start(RunId::new("run-2")).restore_event_seq_allocator(None);
    let late = HostEvent::allocate(
        &allocator,
        AgentId::new("lead"),
        ra_core::event::file::FileEvent::Changed(ra_core::event::file::FileChangedEvent::new(
            CallId::new("c-1"),
            "late.md",
            ra_core::event::file::FileChangeKind::Updated,
        ))
        .into(),
    )
    .unwrap();
    rollout.push(late).start("run-3", &["three"]);
    let after = truncate_rollout_after_run(rollout.records(), &RunId::new("run-2")).unwrap();
    assert_eq!(after.len(), 12);
}

#[test]
fn truncating_after_the_last_run_keeps_everything() {
    let records = three_runs().records();
    assert_eq!(
        truncate_rollout_after_run(records.clone(), &RunId::new("run-3")).unwrap(),
        records
    );
}

#[test]
fn truncating_after_a_run_that_recorded_no_end_is_refused() {
    let mut rollout = Rollout::default();
    rollout
        .start("run-1", &["one"])
        .item(said("a-1", "partial"))
        // A later run does not end it: Codex keeps an unterminated turn in progress too.
        .start("run-2", &["two"])
        .end("run-2", RolloutRunEnd::Completed);

    let error = truncate_rollout_after_run(rollout.records(), &RunId::new("run-1")).unwrap_err();
    assert!(error.to_string().contains("in progress"), "{error}");

    // A run whose earlier segment ended but whose latest did not is in progress as well.
    let mut rollout = Rollout::default();
    rollout
        .start("run-1", &["one"])
        .end("run-1", RolloutRunEnd::Interrupted)
        .start("run-1", &[]);
    let error = truncate_rollout_after_run(rollout.records(), &RunId::new("run-1")).unwrap_err();
    assert!(error.to_string().contains("in progress"), "{error}");
}

#[test]
fn truncating_at_a_run_the_rollout_does_not_hold_is_refused() {
    let records = three_runs().records();
    for error in [
        truncate_rollout_before_run(records.clone(), &RunId::new("run-9")).unwrap_err(),
        truncate_rollout_after_run(records, &RunId::new("run-9")).unwrap_err(),
    ] {
        assert!(error.to_string().contains("was not found"), "{error}");
    }
}

// ---------------------------------------------------------------------------------------------
// From a file
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_reader_rebuilds_the_history_written_to_a_rollout_file() {
    let dir = temp_test_dir("reader");
    let path = dir.join("rollout.jsonl");
    let mut writer = RolloutWriter::open(&path, SessionId::new("sess-1"))
        .await
        .unwrap();
    for payload in three_runs()
        .0
        .iter()
        .map(|record| record.payload().unwrap())
    {
        writer.append(payload).await.unwrap();
    }
    writer.flush().await.unwrap();

    let rebuilt = RolloutReader::open(&path)
        .reconstruct_history()
        .await
        .unwrap();
    assert_eq!(
        rebuilt.history(),
        reconstruct_history(&three_runs().records())
            .unwrap()
            .history()
    );
    assert_eq!(rebuilt.history().len(), 8);
}
