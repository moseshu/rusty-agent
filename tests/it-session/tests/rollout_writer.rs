//! Integration tests for the minimal rollout append writer and reader.

use std::{collections::HashMap, path::PathBuf};

use ra_core::{
    error::{Error, SessionErrorKind},
    event::{
        EventTimestamp, HostEvent, HostEventBody,
        agent::AgentOperationId,
        exec::{ExecEvent, ExecSessionId, ExecStartedEvent},
    },
    item::{AgentId, Compaction, ItemId, Message, OutputPhase, RunItem, RunItemKind},
    session::SessionId,
    state::{RunId, RunState},
    usage::{RequestUsage, Usage},
};
use ra_session::{
    ChildAnchorKind, RolloutChildAnchor, RolloutModelUsage, RolloutPayload, RolloutReader,
    RolloutRecord, RolloutSessionMeta, RolloutSidecar, RolloutTurnContext, RolloutWriter,
    UnifiedReplayItem, graft_child_transcripts,
};
use serde_json::json;

fn temp_test_dir(test_name: &str) -> PathBuf {
    let dir = std::env::temp_dir()
        .join("rusty_agent_tests")
        .join(test_name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("failed to create temp test directory");
    dir
}

#[tokio::test]
async fn test_rollout_writer_lifecycle_and_timeline_seq_monotonicity() {
    let dir = temp_test_dir("lifecycle_and_monotonicity");
    let session_id = SessionId::generate();
    let run_id = RunId::generate();
    let agent_id = AgentId::new("primary-agent");

    let mut writer = RolloutWriter::create_for_session(&dir, session_id.clone())
        .await
        .expect("writer creation should succeed");

    assert_eq!(writer.session_id(), &session_id);
    assert_eq!(writer.next_timeline_seq(), 0);

    // 1. Append session meta (seq 0)
    let meta = RolloutSessionMeta::new(session_id.clone())
        .with_cwd("/workspace/test")
        .with_cli_version("0.1.0")
        .with_originator("ra-cli")
        .with_model_provider("openai");
    let r0 = writer
        .append_session_meta(meta)
        .await
        .expect("append session meta should succeed");
    assert_eq!(r0.timeline_seq(), 0);
    assert_eq!(writer.next_timeline_seq(), 1);

    // 2. Append turn context (seq 1)
    let ctx = RolloutTurnContext::new(run_id.clone(), 1)
        .with_cwd("/workspace/test")
        .with_model("gpt-4o")
        .with_approval_policy("auto");
    let r1 = writer
        .append_turn_context(ctx)
        .await
        .expect("append turn context should succeed");
    assert_eq!(r1.timeline_seq(), 1);
    assert_eq!(writer.next_timeline_seq(), 2);

    // 3. Append host event (seq 2)
    let state = RunState::start(run_id.clone());
    let allocator = state.restore_event_seq_allocator(None);
    let host_event = HostEvent::allocate(
        &allocator,
        agent_id.clone(),
        HostEventBody::Exec(ExecEvent::Started(ExecStartedEvent::new(
            ExecSessionId::generate(),
            "cargo test",
        ))),
    )
    .expect("allocate host event should succeed");
    let r2 = writer
        .append_event(host_event)
        .await
        .expect("append event should succeed");
    assert_eq!(r2.timeline_seq(), 2);
    assert_eq!(writer.next_timeline_seq(), 3);

    // 4. Append session item (seq 3)
    let item = RunItem::new(
        ItemId::new("msg-1"),
        RunItemKind::Message(Message::user("Please build project")),
    );
    let r3 = writer
        .append_item(item)
        .await
        .expect("append item should succeed");
    assert_eq!(r3.timeline_seq(), 3);
    assert_eq!(writer.next_timeline_seq(), 4);

    // 5. Append model usage (seq 4)
    let usage = Usage::from_request(
        RequestUsage::new(500, 100)
            .with_cached_input_tokens(50)
            .with_reasoning_tokens(20),
    );
    let model_usage = RolloutModelUsage::new(run_id.clone(), usage).with_turn_index(1);
    let r4 = writer
        .append_model_usage(model_usage)
        .await
        .expect("append model usage should succeed");
    assert_eq!(r4.timeline_seq(), 4);
    assert_eq!(writer.next_timeline_seq(), 5);

    // 6. Append child anchor (seq 5)
    let anchor = RolloutChildAnchor::new(
        run_id.clone(),
        AgentOperationId::new("subtask-planner"),
        AgentId::new("sub-planner"),
        ChildAnchorKind::Spawned,
        "subagent-subtask-planner.jsonl",
    );
    let r5 = writer
        .append_child_anchor(anchor)
        .await
        .expect("append child anchor should succeed");
    assert_eq!(r5.timeline_seq(), 5);
    assert_eq!(writer.next_timeline_seq(), 6);

    writer.flush().await.expect("flush should succeed");

    // Read back all records and verify sequences and payloads
    let reader = RolloutReader::open(writer.path());
    let records = reader.read_all().await.expect("read_all should succeed");
    assert_eq!(records.len(), 6);

    for (expected_seq, record) in records.iter().enumerate() {
        assert_eq!(record.timeline_seq(), expected_seq as u64);
    }

    let summary = reader
        .scan_summary()
        .await
        .expect("scan_summary should succeed");
    assert_eq!(summary.session_id(), Some(&session_id));
    assert_eq!(summary.record_count(), 6);
    assert_eq!(summary.last_timeline_seq(), Some(5));
    assert_eq!(summary.corrupted_trailing_bytes(), None);
}

#[tokio::test]
async fn test_rollout_writer_unprunable_usage_totals_across_compaction() {
    let dir = temp_test_dir("usage_compaction");
    let session_id = SessionId::generate();
    let run_id = RunId::generate();

    let mut writer = RolloutWriter::create_for_session(&dir, session_id.clone())
        .await
        .expect("writer creation should succeed");

    // Record usage for Turn 1
    let usage1 = Usage::from_request(
        RequestUsage::new(1000, 200)
            .with_cached_input_tokens(200)
            .with_cache_write_tokens(100)
            .with_reasoning_tokens(50),
    );
    writer
        .append_model_usage(
            RolloutModelUsage::new(run_id.clone(), usage1.clone()).with_turn_index(1),
        )
        .await
        .expect("append usage1 should succeed");

    // Record usage for Turn 2
    let usage2 = Usage::from_request(
        RequestUsage::new(1500, 300)
            .with_cached_input_tokens(800)
            .with_cache_write_tokens(0)
            .with_reasoning_tokens(80),
    );
    writer
        .append_model_usage(
            RolloutModelUsage::new(run_id.clone(), usage2.clone()).with_turn_index(2),
        )
        .await
        .expect("append usage2 should succeed");

    // Expected total usage. The running figure carries totals only: it is restated in every
    // checkpoint and in the sidecar, and repeating one entry per call in each of them would grow
    // the log with the square of the session. The entries stay in the records themselves.
    let expected_totals = usage1.accumulate(&usage2).without_entries();
    assert_eq!(writer.usage_totals(), &expected_totals);
    assert_eq!(writer.usage_totals().requests(), 2);
    assert!(writer.usage_totals().request_usage_entries().is_empty());
    assert_eq!(writer.usage_totals().input_tokens(), 2500);
    assert_eq!(writer.usage_totals().output_tokens(), 500);
    assert_eq!(writer.usage_totals().cached_input_tokens(), 1000);
    assert_eq!(writer.usage_totals().cache_write_tokens(), 100);
    assert_eq!(writer.usage_totals().reasoning_tokens(), 130);

    // Simulate session history compaction item recorded to the log
    let compaction_item = RunItem::new(
        ItemId::new("compact-1"),
        RunItemKind::Compaction(Compaction::new(
            "Summarized turns 1 and 2",
            vec![ItemId::new("msg-1"), ItemId::new("msg-2")],
        )),
    );
    writer
        .append_item(compaction_item)
        .await
        .expect("append compaction item should succeed");

    // Add another turn post-compaction
    let usage3 = Usage::from_request(
        RequestUsage::new(800, 150)
            .with_cached_input_tokens(600)
            .with_cache_write_tokens(50)
            .with_reasoning_tokens(20),
    );
    writer
        .append_model_usage(
            RolloutModelUsage::new(run_id.clone(), usage3.clone()).with_turn_index(3),
        )
        .await
        .expect("append usage3 should succeed");

    let final_expected = expected_totals.accumulate(&usage3.without_entries());
    assert_eq!(writer.usage_totals(), &final_expected);

    // Scan the log from reader to verify full unpruned accounting matches writer totals
    let reader = RolloutReader::open(writer.path());
    let summary = reader
        .scan_summary()
        .await
        .expect("scan_summary should succeed");
    assert_eq!(summary.usage_totals(), &final_expected);
    assert_eq!(summary.usage_totals().requests(), 3);

    // Compaction removed conversation items, and the per-request detail is still there: the
    // records are the ledger a total can be rebuilt from and checked against, which is the only
    // reason a checkpointed total is worth anything.
    let records = reader.read_all().await.expect("records should be readable");
    let per_request: Vec<(u64, u64)> = records
        .iter()
        .filter_map(|record| match record.payload() {
            Ok(RolloutPayload::ModelUsage(usage)) => Some(usage),
            _ => None,
        })
        .flat_map(|usage| {
            usage
                .usage()
                .request_usage_entries()
                .iter()
                .map(|entry| (entry.input_tokens(), entry.cached_input_tokens()))
                .collect::<Vec<_>>()
        })
        .collect();
    assert_eq!(per_request, vec![(1000, 200), (1500, 800), (800, 600)]);
}

#[tokio::test]
async fn test_rollout_writer_persisted_run_max_seq_sidecar_tracking() {
    let dir = temp_test_dir("sidecar_tracking");
    let session_id = SessionId::generate();
    let run_a = RunId::generate();
    let run_b = RunId::generate();
    let agent_id = AgentId::new("test-agent");

    let mut writer = RolloutWriter::create_for_session(&dir, session_id)
        .await
        .expect("writer creation should succeed");

    let state_a_init = RunState::start(run_a.clone());
    let allocator_a = state_a_init.restore_event_seq_allocator(None);
    let state_b_init = RunState::start(run_b.clone());
    let allocator_b = state_b_init.restore_event_seq_allocator(None);

    // Run A emits events with seq 0, 1, 2
    for _ in 0..3 {
        let event = HostEvent::allocate(
            &allocator_a,
            agent_id.clone(),
            HostEventBody::Exec(ExecEvent::Started(ExecStartedEvent::new(
                ExecSessionId::generate(),
                "echo A",
            ))),
        )
        .expect("allocate event should succeed");
        writer
            .append_event(event)
            .await
            .expect("append should succeed");
    }

    // Run B emits events with seq 0, 1
    for _ in 0..2 {
        let event = HostEvent::allocate(
            &allocator_b,
            agent_id.clone(),
            HostEventBody::Exec(ExecEvent::Started(ExecStartedEvent::new(
                ExecSessionId::generate(),
                "echo B",
            ))),
        )
        .expect("allocate event should succeed");
        writer
            .append_event(event)
            .await
            .expect("append should succeed");
    }

    assert_eq!(writer.persisted_run_max_seq(&run_a), Some(2));
    assert_eq!(writer.persisted_run_max_seq(&run_b), Some(1));

    // Verify RunState reconciliation using the sidecar max seq
    let state_a = RunState::start(run_a.clone()).with_next_host_event_seq(1); // checkpointed lag
    let restored_allocator_a =
        state_a.restore_event_seq_allocator(writer.persisted_run_max_seq(&run_a));

    // Restored next sequence should be max(checkpoint_next: 1, persisted_max + 1: 3) = 3
    assert_eq!(restored_allocator_a.current_next(), 3);
    let next_seq_a = restored_allocator_a
        .allocate()
        .expect("allocate should succeed");
    assert_eq!(next_seq_a, 3);
}

#[tokio::test]
async fn test_rollout_writer_crash_safety_and_truncated_last_line() {
    let dir = temp_test_dir("crash_safety");
    let session_id = SessionId::generate();
    let run_id = RunId::generate();

    let file_path = dir.join(format!("rollout-{}.jsonl", session_id.as_str()));

    // 1. Write 3 valid records
    {
        let mut writer = RolloutWriter::open(&file_path, session_id.clone())
            .await
            .expect("open writer should succeed");

        writer
            .append_session_meta(RolloutSessionMeta::new(session_id.clone()))
            .await
            .unwrap();
        writer
            .append_turn_context(RolloutTurnContext::new(run_id.clone(), 1))
            .await
            .unwrap();
        writer
            .append_item(RunItem::new(
                ItemId::new("msg-1"),
                RunItemKind::Message(Message::user("Hello")),
            ))
            .await
            .unwrap();
        writer.flush().await.unwrap();
    }

    // 2. Simulate an abrupt crash / power loss mid-write: append incomplete truncated JSON line without newline
    {
        use tokio::io::AsyncWriteExt;
        let mut file = tokio::fs::OpenOptions::new()
            .append(true)
            .open(&file_path)
            .await
            .expect("open file for corrupt append should succeed");
        let partial_line = b"{\"schema_version\":1,\"timeline_seq\":3,\"at\":1755570600000,\"type\":\"item\",\"payload\":{\"id\":\"corrupt_half_write\"";
        file.write_all(partial_line).await.unwrap();
        file.flush().await.unwrap();
    }

    // 3. Reader should parse all 3 prior valid records without failing
    let reader = RolloutReader::open(&file_path);
    let records = reader
        .read_all()
        .await
        .expect("read_all should succeed despite trailing corrupted line");
    assert_eq!(records.len(), 3);
    assert_eq!(records[0].timeline_seq(), 0);
    assert_eq!(records[1].timeline_seq(), 1);
    assert_eq!(records[2].timeline_seq(), 2);

    let summary = reader
        .scan_summary()
        .await
        .expect("scan_summary should succeed");
    assert_eq!(summary.record_count(), 3);
    assert_eq!(summary.last_timeline_seq(), Some(2));
    assert!(summary.corrupted_trailing_bytes().is_some());

    // 4. Re-opening RolloutWriter on this file truncates the partial corrupt line, recovers next_timeline_seq = 3,
    // and continues writing cleanly without corrupting previous or future writes!
    {
        let mut recovered_writer = RolloutWriter::open(&file_path, session_id.clone())
            .await
            .expect("re-open writer should succeed");

        assert_eq!(recovered_writer.next_timeline_seq(), 3);

        let final_item = RunItem::new(
            ItemId::new("msg-final"),
            RunItemKind::Message(Message::assistant("Done", OutputPhase::Final)),
        );
        let rec = recovered_writer
            .append_item(final_item)
            .await
            .expect("append to recovered writer should succeed");
        assert_eq!(rec.timeline_seq(), 3);
        assert_eq!(recovered_writer.next_timeline_seq(), 4);
    }

    // 5. CRITICAL: Re-read the file after recovery writes and verify ALL 4 records are readable and intact!
    let reader_post_recovery = RolloutReader::open(&file_path);
    let all_records = reader_post_recovery
        .read_all()
        .await
        .expect("read_all after recovery should succeed");
    assert_eq!(all_records.len(), 4);
    for (i, rec) in all_records.iter().enumerate() {
        assert_eq!(rec.timeline_seq(), i as u64);
    }
}

#[tokio::test]
async fn test_rollout_writer_crash_safety_with_cjk_multibyte_cut() {
    let dir = temp_test_dir("cjk_multibyte_cut");
    let session_id = SessionId::generate();
    let file_path = dir.join(format!("rollout-{}.jsonl", session_id.as_str()));

    // Write initial record containing Chinese text
    {
        let mut writer = RolloutWriter::open(&file_path, session_id.clone())
            .await
            .unwrap();
        writer
            .append_item(RunItem::new(
                ItemId::new("msg-cjk"),
                RunItemKind::Message(Message::user("你好世界，测试中文消息持久化")),
            ))
            .await
            .unwrap();
        writer.flush().await.unwrap();
    }

    // Append partial bytes cut in the middle of a 3-byte UTF-8 character (0xE4 0xBD without 3rd byte)
    {
        use tokio::io::AsyncWriteExt;
        let mut file = tokio::fs::OpenOptions::new()
            .append(true)
            .open(&file_path)
            .await
            .unwrap();
        // Cut in middle of Chinese character
        let corrupt_cjk_bytes = b"{\"schema_version\":1,\"timeline_seq\":1,\"at\":1755570600000,\"type\":\"item\",\"payload\":{\"id\":\"msg-2\",\"content\":\"\xE4\xBD";
        file.write_all(corrupt_cjk_bytes).await.unwrap();
        file.flush().await.unwrap();
    }

    // Reader must NOT fail with UTF-8 decode error; it should gracefully ignore incomplete trailing byte sequence
    let reader = RolloutReader::open(&file_path);
    let records = reader
        .read_all()
        .await
        .expect("reader must handle multibyte UTF-8 cut at EOF");
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].timeline_seq(), 0);

    // Writer reopens, truncates corrupted bytes, and continues writing
    {
        let mut writer = RolloutWriter::open(&file_path, session_id.clone())
            .await
            .unwrap();
        assert_eq!(writer.next_timeline_seq(), 1);

        writer
            .append_item(RunItem::new(
                ItemId::new("msg-next"),
                RunItemKind::Message(Message::assistant("恢复正常", OutputPhase::Final)),
            ))
            .await
            .unwrap();
    }

    // Verify both records read cleanly
    let final_records = RolloutReader::open(&file_path).read_all().await.unwrap();
    assert_eq!(final_records.len(), 2);
    assert_eq!(final_records[0].timeline_seq(), 0);
    assert_eq!(final_records[1].timeline_seq(), 1);
}

#[tokio::test]
async fn test_rollout_writer_child_run_anchor_and_transcript_grafting() {
    let dir = temp_test_dir("child_anchor_grafting");
    let session_id = SessionId::generate();
    let root_run_id = RunId::generate();
    let child_run_id = RunId::generate();
    let op_id = AgentOperationId::new("subtask-code-edit");
    let child_agent_id = AgentId::new("sub-coder");

    let mut root_writer = RolloutWriter::create_for_session(&dir, session_id.clone())
        .await
        .expect("root writer creation should succeed");

    // Root thread emits session meta and user request
    root_writer
        .append_session_meta(RolloutSessionMeta::new(session_id.clone()))
        .await
        .unwrap();
    root_writer
        .append_item(RunItem::new(
            ItemId::new("user-req"),
            RunItemKind::Message(Message::user("Refactor module")),
        ))
        .await
        .unwrap();

    // Spawn child anchor on root timeline
    let spawn_anchor = RolloutChildAnchor::new(
        root_run_id.clone(),
        op_id.clone(),
        child_agent_id.clone(),
        ChildAnchorKind::Spawned,
        "transcript-sub-coder.jsonl",
    )
    .with_child_run_id(child_run_id.clone())
    .with_summary("Refactor subtask started");

    root_writer
        .append_child_anchor(spawn_anchor)
        .await
        .expect("append spawn anchor should succeed");

    // Child completes anchor on root timeline
    let complete_anchor = RolloutChildAnchor::new(
        root_run_id.clone(),
        op_id.clone(),
        child_agent_id.clone(),
        ChildAnchorKind::Completed,
        "transcript-sub-coder.jsonl",
    )
    .with_child_run_id(child_run_id.clone())
    .with_summary("Refactor completed with 2 files modified");

    root_writer
        .append_child_anchor(complete_anchor)
        .await
        .expect("append complete anchor should succeed");

    // Root final message
    root_writer
        .append_item(RunItem::new(
            ItemId::new("assistant-final"),
            RunItemKind::Message(Message::assistant("All tasks complete", OutputPhase::Final)),
        ))
        .await
        .unwrap();

    // Independent child transcript records (simulated)
    let child_record_1 = RolloutRecord::new(
        0,
        EventTimestamp::now(),
        RolloutPayload::Item(RunItem::new(
            ItemId::new("child-item-1"),
            RunItemKind::Message(Message::assistant(
                "Analyzing files...",
                OutputPhase::Commentary,
            )),
        )),
    )
    .unwrap();
    let child_record_2 = RolloutRecord::new(
        1,
        EventTimestamp::now(),
        RolloutPayload::Item(RunItem::new(
            ItemId::new("child-item-2"),
            RunItemKind::Message(Message::assistant(
                "Modified file A and B",
                OutputPhase::Final,
            )),
        )),
    )
    .unwrap();

    let mut child_transcripts = HashMap::new();
    child_transcripts.insert(op_id.clone(), vec![child_record_1, child_record_2]);

    let root_records = RolloutReader::open(root_writer.path())
        .read_all()
        .await
        .unwrap();
    assert_eq!(root_records.len(), 5);

    let unified = graft_child_transcripts(&root_records, &child_transcripts);
    assert_eq!(unified.len(), 7); // 5 root records + 2 grafted child records

    // Verify ordering: root meta -> root user req -> spawn anchor -> child rec 1 -> child rec 2 -> complete anchor -> final msg
    match &unified[0] {
        UnifiedReplayItem::Root(r) => assert_eq!(r.timeline_seq(), 0),
        _ => panic!("expected root item at 0"),
    }
    match &unified[1] {
        UnifiedReplayItem::Root(r) => assert_eq!(r.timeline_seq(), 1),
        _ => panic!("expected root item at 1"),
    }
    match &unified[2] {
        UnifiedReplayItem::Root(r) => assert_eq!(r.timeline_seq(), 2),
        _ => panic!("expected spawn anchor at 2"),
    }
    match &unified[3] {
        UnifiedReplayItem::Child {
            operation_id,
            record,
        } => {
            assert_eq!(operation_id, &op_id);
            assert_eq!(record.timeline_seq(), 0);
        }
        _ => panic!("expected child record 1 at index 3"),
    }
    match &unified[4] {
        UnifiedReplayItem::Child {
            operation_id,
            record,
        } => {
            assert_eq!(operation_id, &op_id);
            assert_eq!(record.timeline_seq(), 1);
        }
        _ => panic!("expected child record 2 at index 4"),
    }
    match &unified[5] {
        UnifiedReplayItem::Root(r) => assert_eq!(r.timeline_seq(), 3),
        _ => panic!("expected complete anchor at 5"),
    }
    match &unified[6] {
        UnifiedReplayItem::Root(r) => assert_eq!(r.timeline_seq(), 4),
        _ => panic!("expected root final message at 6"),
    }
}

#[tokio::test]
async fn test_rollout_record_serde_forward_compatibility_and_unknown_fields() {
    let raw_json = json!({
        "schema_version": 1,
        "timeline_seq": 42,
        "at": 1755570600000_u64,
        "type": "custom_extension_event",
        "payload": {
            "custom_key": "custom_value",
            "nested": { "count": 10 }
        },
        "future_envelope_field": "preserved_val"
    });

    let record: RolloutRecord = serde_json::from_value(raw_json.clone()).unwrap();
    assert_eq!(record.timeline_seq(), 42);
    assert_eq!(record.type_name(), "custom_extension_event");
    assert!(record.unknown().get("future_envelope_field").is_some());
    // Crucial: unknown must NOT capture type or payload as duplicate keys!
    assert!(record.unknown().get("type").is_none());
    assert!(record.unknown().get("payload").is_none());

    let payload = record.payload().expect("payload should parse");
    match payload {
        RolloutPayload::Unknown { type_name, data } => {
            assert_eq!(type_name, "custom_extension_event");
            assert_eq!(data["custom_key"], "custom_value");
            assert_eq!(data["nested"]["count"], 10);
        }
        _ => panic!("expected RolloutPayload::Unknown"),
    }

    let reserialized = serde_json::to_value(&record).unwrap();
    assert_eq!(reserialized["timeline_seq"], 42);
    assert_eq!(reserialized["future_envelope_field"], "preserved_val");
    assert_eq!(reserialized["type"], "custom_extension_event");
    assert_eq!(reserialized["payload"]["custom_key"], "custom_value");

    // Verify raw JSON string has no duplicate keys
    let serialized_str = serde_json::to_string(&record).unwrap();
    let count_type_key = serialized_str.matches("\"type\":").count();
    assert_eq!(count_type_key, 1, "type key must not be duplicated");
}

#[tokio::test]
async fn test_rollout_record_payload_rejects_corrupted_known_type() {
    let corrupted_item_json = json!({
        "schema_version": 1,
        "timeline_seq": 1,
        "at": 1755570600000_u64,
        "type": "item",
        "payload": {
            "invalid_structure_for_item": true
        }
    });

    let record: RolloutRecord = serde_json::from_value(corrupted_item_json).unwrap();
    let result = record.payload();
    assert!(result.is_err(), "corrupted known type must return Err");
    let err = result.unwrap_err();
    assert!(matches!(
        err,
        Error::Session {
            kind: SessionErrorKind::Corrupted,
            ..
        }
    ));
}

#[tokio::test]
async fn test_usage_accumulate_preserves_unknown_fields() {
    let u1_json = json!({
        "input_tokens": 100,
        "output_tokens": 50,
        "future_hardware_tokens": 12
    });
    let u2_json = json!({
        "input_tokens": 200,
        "output_tokens": 100,
        "future_cost_multiplier": "1.5"
    });

    let u1: Usage = serde_json::from_value(u1_json).unwrap();
    let u2: Usage = serde_json::from_value(u2_json).unwrap();

    let combined = u1.accumulate(&u2);
    assert_eq!(combined.input_tokens(), 300);
    assert_eq!(combined.output_tokens(), 150);
    assert_eq!(
        combined.unknown().get("future_hardware_tokens"),
        Some(&json!(12))
    );
    assert_eq!(
        combined.unknown().get("future_cost_multiplier"),
        Some(&json!("1.5"))
    );
}

#[tokio::test]
async fn test_rollout_writer_sidecar_file_persistence() {
    let dir = temp_test_dir("sidecar_file_persistence");
    let session_id = SessionId::generate();
    let run_id = RunId::generate();

    let mut writer = RolloutWriter::create_for_session(&dir, session_id.clone())
        .await
        .unwrap();

    let usage = Usage::from_request(RequestUsage::new(500, 100));
    writer
        .append_model_usage(RolloutModelUsage::new(run_id.clone(), usage).with_turn_index(1))
        .await
        .unwrap();

    writer.flush().await.unwrap();

    // Verify sidecar file exists on disk
    let sidecar_path = writer.sidecar_path();
    assert!(sidecar_path.exists());

    let sidecar_bytes = tokio::fs::read(sidecar_path).await.unwrap();
    let sidecar: RolloutSidecar = serde_json::from_slice(&sidecar_bytes).unwrap();
    assert_eq!(sidecar.session_id(), &session_id);
    assert_eq!(sidecar.next_timeline_seq(), 1);
    assert_eq!(sidecar.usage_totals().input_tokens(), 500);
}

/// A record from a newer build — complete, newline-terminated, payload this build cannot parse —
/// must survive a reopen untouched. Truncating it would be a silent downgrade delete.
#[tokio::test]
async fn test_rollout_writer_preserves_terminated_record_with_unreadable_payload() {
    let dir = temp_test_dir("unreadable_payload_preserved");
    let session_id = SessionId::generate();
    let file_path = dir.join(format!("rollout-{}.jsonl", session_id.as_str()));

    {
        let mut writer = RolloutWriter::open(&file_path, session_id.clone())
            .await
            .unwrap();
        writer
            .append_item(RunItem::new(
                ItemId::new("known"),
                RunItemKind::Message(Message::user("readable by this build")),
            ))
            .await
            .unwrap();
    }

    // A newer build appends an `item` carrying a RunItemKind variant this build does not have.
    let from_newer_build = concat!(
        r#"{"schema_version":2,"timeline_seq":1,"at":1755570600000,"type":"item","#,
        r#""payload":{"schema_version":2,"id":"from-newer-build","#,
        r#""kind":{"type":"brand_new_variant","data":{"x":1}}}}"#,
        "\n"
    );
    {
        use tokio::io::AsyncWriteExt;
        let mut file = tokio::fs::OpenOptions::new()
            .append(true)
            .open(&file_path)
            .await
            .unwrap();
        file.write_all(from_newer_build.as_bytes()).await.unwrap();
        file.flush().await.unwrap();
    }
    let bytes_before = tokio::fs::read(&file_path).await.unwrap().len();

    // The reader hands the record back rather than dropping or rejecting it.
    let records = RolloutReader::open(&file_path).read_all().await.unwrap();
    assert_eq!(records.len(), 2);
    assert_eq!(records[1].type_name(), "item");
    assert_eq!(records[1].payload_value()["id"], json!("from-newer-build"));
    assert!(
        records[1].payload().is_err(),
        "this build cannot type the payload, and must say so rather than guess"
    );

    // Reopening must not erase it, and must not rewind over its sequence number.
    {
        let mut writer = RolloutWriter::open(&file_path, session_id.clone())
            .await
            .unwrap();
        assert_eq!(
            writer.next_timeline_seq(),
            2,
            "the unreadable record still occupies seq 1"
        );
        writer
            .append_item(RunItem::new(
                ItemId::new("after"),
                RunItemKind::Message(Message::user("written after the reopen")),
            ))
            .await
            .unwrap();
    }

    let after = tokio::fs::read_to_string(&file_path).await.unwrap();
    assert!(
        after.contains("from-newer-build"),
        "the newer build's record was destroyed on reopen"
    );
    assert!(after.len() > bytes_before);

    let final_records = RolloutReader::open(&file_path).read_all().await.unwrap();
    assert_eq!(final_records.len(), 3);
    let seqs: Vec<u64> = final_records
        .iter()
        .map(RolloutRecord::timeline_seq)
        .collect();
    assert_eq!(seqs, vec![0, 1, 2]);
}

/// A torn write can stop on a boundary that still parses. The record is kept, but the writer has
/// to close the line before appending or the next record is spliced onto its tail.
#[tokio::test]
async fn test_rollout_writer_terminates_unterminated_last_line_before_appending() {
    let dir = temp_test_dir("unterminated_last_line");
    let session_id = SessionId::generate();
    let file_path = dir.join(format!("rollout-{}.jsonl", session_id.as_str()));

    {
        let mut writer = RolloutWriter::open(&file_path, session_id.clone())
            .await
            .unwrap();
        writer
            .append_item(RunItem::new(
                ItemId::new("first"),
                RunItemKind::Message(Message::user("first")),
            ))
            .await
            .unwrap();
    }

    // Strip the trailing newline, as a write cut exactly at the record boundary would.
    let body = tokio::fs::read_to_string(&file_path).await.unwrap();
    tokio::fs::write(&file_path, body.trim_end_matches('\n'))
        .await
        .unwrap();

    {
        let mut writer = RolloutWriter::open(&file_path, session_id.clone())
            .await
            .unwrap();
        assert_eq!(writer.next_timeline_seq(), 1);
        writer
            .append_item(RunItem::new(
                ItemId::new("second"),
                RunItemKind::Message(Message::user("second")),
            ))
            .await
            .unwrap();
    }

    let records = RolloutReader::open(&file_path).read_all().await.unwrap();
    assert_eq!(records.len(), 2, "the two records must not be spliced");
    assert_eq!(records[0].payload_value()["id"], json!("first"));
    assert_eq!(records[1].payload_value()["id"], json!("second"));
}

/// The sidecar is a derived cache; losing it must not turn a durable append into a reported
/// failure, which a retrying caller would answer by writing the record twice.
#[tokio::test]
async fn test_rollout_writer_survives_unwritable_sidecar() {
    let dir = temp_test_dir("unwritable_sidecar");
    let session_id = SessionId::generate();
    let file_path = dir.join(format!("rollout-{}.jsonl", session_id.as_str()));

    let mut writer = RolloutWriter::open(&file_path, session_id.clone())
        .await
        .unwrap();
    writer
        .append_item(RunItem::new(
            ItemId::new("before"),
            RunItemKind::Message(Message::user("before")),
        ))
        .await
        .unwrap();
    assert!(!writer.sidecar_is_stale());

    // Make the sidecar path unwritable mid-session (stand-in for ENOSPC / EACCES / EIO).
    let sidecar_path = writer.sidecar_path().to_path_buf();
    std::fs::remove_file(&sidecar_path).unwrap();
    std::fs::create_dir_all(&sidecar_path).unwrap();

    let record = writer
        .append_item(RunItem::new(
            ItemId::new("during"),
            RunItemKind::Message(Message::user("during")),
        ))
        .await
        .expect("a durable append must not fail because its derived cache could not be written");
    assert_eq!(record.timeline_seq(), 1);

    // The sidecar is refreshed on flush rather than on every append, so that is where the
    // failure surfaces — and it must surface as a flag, not as a failed flush.
    writer
        .flush()
        .await
        .expect("flushing the log must not fail because its derived cache could not be written");
    assert!(
        writer.sidecar_is_stale(),
        "the failure has to be observable somewhere"
    );
    drop(writer);

    // The rollout file is the source of truth and is intact; recovery still works without a
    // usable sidecar, falling back to the full scan rather than refusing to open.
    let records = RolloutReader::open(&file_path).read_all().await.unwrap();
    assert_eq!(records.len(), 2);

    let reopened = RolloutWriter::open(&file_path, session_id).await.unwrap();
    assert_eq!(reopened.next_timeline_seq(), 2);
    assert!(reopened.sidecar_is_stale());
}

/// Two writers on one file used to overwrite each other's records and leave the log unreadable.
/// The second open has to be refused while the first is alive.
#[tokio::test]
async fn test_rollout_writer_refuses_concurrent_second_writer() {
    let dir = temp_test_dir("concurrent_writers");
    let session_id = SessionId::generate();
    let file_path = dir.join(format!("rollout-{}.jsonl", session_id.as_str()));

    let mut first = RolloutWriter::open(&file_path, session_id.clone())
        .await
        .unwrap();
    first
        .append_item(RunItem::new(
            ItemId::new("first"),
            RunItemKind::Message(Message::user("first")),
        ))
        .await
        .unwrap();

    let second = RolloutWriter::open(&file_path, session_id.clone()).await;
    assert!(
        second.is_err(),
        "a second writer must not be able to open a locked rollout"
    );
    assert!(
        matches!(
            second.unwrap_err(),
            Error::Session {
                kind: SessionErrorKind::Io,
                ..
            }
        ),
        "the refusal has to name the contention, not surface as corruption later"
    );

    // The holder keeps working, and the log stays readable.
    first
        .append_item(RunItem::new(
            ItemId::new("second"),
            RunItemKind::Message(Message::user("second")),
        ))
        .await
        .unwrap();
    let records = RolloutReader::open(&file_path).read_all().await.unwrap();
    assert_eq!(records.len(), 2);

    // Releasing the writer releases the lock.
    drop(first);
    let third = RolloutWriter::open(&file_path, session_id).await.unwrap();
    assert_eq!(third.next_timeline_seq(), 2);
}

/// A damaged sidecar must not be able to steer recovery, however plausible it looks.
///
/// This is the inverse of what an earlier revision asserted. That version treated the sidecar as
/// authoritative once a SHA-256 over the log matched, and this very scenario was written as proof
/// the fast path worked. It is not proof of a fast path; it is the bug. A digest establishes that
/// the log did not change — it cannot establish that the totals beside it were computed from that
/// log, because those totals are not among the bytes it covers. One plausible hit to the sidecar
/// (`next_timeline_seq` 3 becoming 1) sails through such a check, and resuming from it appends a
/// duplicate sequence number that leaves the log permanently unopenable.
#[tokio::test]
async fn test_rollout_writer_ignores_a_damaged_sidecar_during_recovery() {
    let dir = temp_test_dir("sidecar_not_authoritative");
    let session_id = SessionId::generate();
    let run_id = RunId::generate();
    let file_path = dir.join(format!("rollout-{}.jsonl", session_id.as_str()));
    let sidecar_path = dir.join(format!(
        "rollout-{}.jsonl.sidecar.json",
        session_id.as_str()
    ));

    {
        let mut writer = RolloutWriter::open(&file_path, session_id.clone())
            .await
            .unwrap();
        writer
            .append_session_meta(RolloutSessionMeta::new(session_id.clone()))
            .await
            .unwrap();
        writer
            .append_model_usage(RolloutModelUsage::new(
                run_id.clone(),
                Usage::from_request(RequestUsage::new(700, 300)),
            ))
            .await
            .unwrap();
        writer
            .append_item(RunItem::new(
                ItemId::new("last"),
                RunItemKind::Message(Message::user("last")),
            ))
            .await
            .unwrap();
        writer.flush().await.unwrap();
    }

    let genuine: RolloutSidecar =
        serde_json::from_slice(&tokio::fs::read(&sidecar_path).await.unwrap()).unwrap();
    assert_eq!(genuine.next_timeline_seq(), 3);
    assert_eq!(genuine.usage_totals().input_tokens(), 700);

    // Rewrite the summary with values no scan of this log would produce. The log is untouched, so
    // any integrity check over its bytes still passes.
    let damaged = RolloutSidecar::new(
        session_id.clone(),
        1,
        HashMap::new(),
        Usage::from_request(RequestUsage::new(999_000, 0)),
    );
    tokio::fs::write(&sidecar_path, serde_json::to_vec(&damaged).unwrap())
        .await
        .unwrap();

    let mut writer = RolloutWriter::open(&file_path, session_id.clone())
        .await
        .unwrap();
    assert_eq!(
        writer.next_timeline_seq(),
        3,
        "recovery took its sequence from the sidecar instead of the log"
    );
    assert_eq!(
        writer.usage_totals().input_tokens(),
        700,
        "recovery took its usage ledger from the sidecar instead of the log"
    );

    // Appending therefore continues the real sequence, and the log stays readable.
    writer
        .append_item(RunItem::new(
            ItemId::new("after"),
            RunItemKind::Message(Message::user("after")),
        ))
        .await
        .unwrap();
    drop(writer);

    let records = RolloutReader::open(&file_path).read_all().await.unwrap();
    let seqs: Vec<u64> = records.iter().map(RolloutRecord::timeline_seq).collect();
    assert_eq!(seqs, vec![0, 1, 2, 3]);
    RolloutWriter::open(&file_path, session_id)
        .await
        .expect("the log must still be openable");
}

/// An in-place rewrite of the log keeps its length identical, so nothing about the file's size
/// can be used to wave the scan through. Recovery has to report the damage rather than build on
/// top of it.
#[tokio::test]
async fn test_rollout_writer_rejects_same_length_log_corruption() {
    let dir = temp_test_dir("sidecar_same_length_corruption");
    let session_id = SessionId::generate();
    let file_path = dir.join(format!("rollout-{}.jsonl", session_id.as_str()));

    {
        let mut writer = RolloutWriter::open(&file_path, session_id.clone())
            .await
            .unwrap();
        writer
            .append_session_meta(RolloutSessionMeta::new(session_id.clone()))
            .await
            .unwrap();
        writer
            .append_item(RunItem::new(
                ItemId::new("real"),
                RunItemKind::Message(Message::user("real")),
            ))
            .await
            .unwrap();
    }

    // Same byte count, different content — what an in-place rewrite looks like.
    let intact = tokio::fs::read_to_string(&file_path).await.unwrap();
    let garbage = format!("{}\n", "x".repeat(intact.len() - 1));
    assert_eq!(garbage.len(), intact.len());
    tokio::fs::write(&file_path, &garbage).await.unwrap();

    // The sidecar still sits there claiming a healthy summary; recovery ignores it and the scan
    // honours the reader's corruption contract.
    let err = RolloutWriter::open(&file_path, session_id)
        .await
        .expect_err("a corrupted log must not be resumed");
    assert!(
        matches!(
            err,
            Error::Session {
                kind: SessionErrorKind::Corrupted,
                ..
            }
        ),
        "expected Corrupted, got: {err}"
    );
}

/// Opening a rollout under the wrong identity used to succeed and stamp the sidecar with the
/// wrong session while the log still named the real one.
#[tokio::test]
async fn test_rollout_writer_rejects_mismatched_session_identity() {
    let dir = temp_test_dir("session_identity");
    let owner = SessionId::new("sess-owner");
    let intruder = SessionId::new("sess-intruder");
    let file_path = dir.join("rollout-sess-owner.jsonl");

    {
        let mut writer = RolloutWriter::open(&file_path, owner.clone())
            .await
            .unwrap();
        writer
            .append_session_meta(RolloutSessionMeta::new(owner.clone()))
            .await
            .unwrap();
    }
    // Drop the sidecar so recovery goes through the scan, where identity is established.
    tokio::fs::remove_file(dir.join("rollout-sess-owner.jsonl.sidecar.json"))
        .await
        .unwrap();

    let result = RolloutWriter::open(&file_path, intruder).await;
    assert!(result.is_err(), "the wrong session must not adopt this log");
    assert!(matches!(result.unwrap_err(), Error::Caller { .. }));

    // The owner still opens it, and the log still names the owner.
    let reopened = RolloutWriter::open(&file_path, owner.clone())
        .await
        .unwrap();
    assert_eq!(reopened.session_id(), &owner);
}

/// Duplicate or backwards `timeline_seq` means two writers interleaved; recovery must refuse
/// rather than pick a number that collides with records already on disk.
#[tokio::test]
async fn test_rollout_writer_rejects_non_monotonic_timeline_seq() {
    let dir = temp_test_dir("non_monotonic_seq");
    let session_id = SessionId::generate();
    let file_path = dir.join(format!("rollout-{}.jsonl", session_id.as_str()));

    let line = |seq: u64, id: &str| {
        format!(
            r#"{{"schema_version":1,"timeline_seq":{seq},"at":1755570600000,"type":"item","payload":{{"schema_version":1,"id":"{id}","kind":{{"type":"message","data":{{"schema_version":1,"role":"user","content":[{{"type":"text","data":{{"schema_version":1,"text":"x"}}}}]}}}}}}}}"#
        )
    };

    // Gaps stay legal: truncating a corrupt tail leaves one behind.
    tokio::fs::write(&file_path, format!("{}\n{}\n", line(0, "a"), line(7, "b")))
        .await
        .unwrap();
    let gapped = RolloutWriter::open(&file_path, session_id.clone())
        .await
        .expect("a hole in the sequence is not corruption");
    assert_eq!(gapped.next_timeline_seq(), 8);
    drop(gapped);

    for (label, body) in [
        ("duplicate", format!("{}\n{}\n", line(3, "a"), line(3, "b"))),
        ("backwards", format!("{}\n{}\n", line(3, "a"), line(1, "b"))),
    ] {
        tokio::fs::write(&file_path, body).await.unwrap();
        tokio::fs::remove_file(dir.join(format!(
            "rollout-{}.jsonl.sidecar.json",
            session_id.as_str()
        )))
        .await
        .ok();

        let summary = RolloutReader::open(&file_path)
            .scan_summary()
            .await
            .unwrap();
        assert!(
            summary.non_monotonic_timeline_seq().is_some(),
            "{label} sequence must be reported"
        );
        let result = RolloutWriter::open(&file_path, session_id.clone()).await;
        assert!(result.is_err(), "{label} sequence must block recovery");
        assert!(matches!(
            result.unwrap_err(),
            Error::Session {
                kind: SessionErrorKind::Corrupted,
                ..
            }
        ));
    }
}

/// A write whose outcome is unknown must stop the writer rather than let a caller stack a retry
/// behind a possibly-partial line, which would fuse the two into one unparsable record.
///
/// `/dev/full` fails every write with `ENOSPC`, which is the only always-failing write target
/// available without root. It is Linux-only — macOS has no equivalent, and the alternatives
/// (`RLIMIT_FSIZE`, filesystem size limits) either kill the process with `SIGXFSZ` or depend on
/// the filesystem's maximum file size. On other platforms the poison logic is unexercised here.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn test_rollout_writer_poisons_itself_when_a_write_fails() {
    let session_id = SessionId::new("sess-poison-probe");

    // The sidecar lands next to the device node and needs root, but a sidecar failure is
    // non-fatal by design, so the writer still opens.
    let Ok(mut writer) = RolloutWriter::open("/dev/full", session_id).await else {
        // No flock on the device node, or the node is unavailable in this sandbox.
        return;
    };
    assert!(!writer.is_poisoned());

    let failed = writer
        .append_item(RunItem::new(
            ItemId::new("doomed"),
            RunItemKind::Message(Message::user("doomed")),
        ))
        .await;
    let err = failed.expect_err("writing to /dev/full must fail with ENOSPC");
    assert!(
        err.to_string().contains("poisoned"),
        "the error must say the commit outcome is unknown, got: {err}"
    );
    assert!(writer.is_poisoned());
    assert_eq!(
        writer.next_timeline_seq(),
        0,
        "a failed append must not consume its sequence number"
    );

    // The natural next move — retry — has to be refused rather than splice onto a partial line.
    let retried = writer
        .append_item(RunItem::new(
            ItemId::new("retry"),
            RunItemKind::Message(Message::user("retry")),
        ))
        .await;
    assert!(
        retried
            .expect_err("a poisoned writer must refuse further appends")
            .to_string()
            .contains("poisoned")
    );
}

/// Resume must read the checkpoint plus the tail, not the whole log.
///
/// Proven by making the head unreadable: everything before the last checkpoint is overwritten
/// with garbage of the same length, so a full scan would fail. Recovery succeeding *and* landing
/// on the right numbers means it started from the checkpoint.
#[tokio::test]
async fn test_rollout_writer_resumes_from_checkpoint_without_reading_the_head() {
    let dir = temp_test_dir("checkpoint_fast_resume");
    let session_id = SessionId::generate();
    let run_id = RunId::generate();
    let file_path = dir.join(format!("rollout-{}.jsonl", session_id.as_str()));

    let head_len;
    {
        let mut writer = RolloutWriter::open(&file_path, session_id.clone())
            .await
            .unwrap();
        writer.set_checkpoint_interval(4);
        writer
            .append_session_meta(RolloutSessionMeta::new(session_id.clone()))
            .await
            .unwrap();
        for _ in 0..3 {
            writer
                .append_model_usage(RolloutModelUsage::new(
                    run_id.clone(),
                    Usage::from_request(RequestUsage::new(100, 10)),
                ))
                .await
                .unwrap();
        }
        // The fourth append trips the interval and lays down a checkpoint behind it.
        let offset = writer
            .last_checkpoint_offset()
            .expect("a checkpoint must have been written");
        head_len = offset;

        // Two more records land after the checkpoint; they are the tail resume must re-read.
        writer
            .append_model_usage(RolloutModelUsage::new(
                run_id.clone(),
                Usage::from_request(RequestUsage::new(50, 5)),
            ))
            .await
            .unwrap();
        writer
            .append_item(RunItem::new(
                ItemId::new("tail"),
                RunItemKind::Message(Message::user("tail")),
            ))
            .await
            .unwrap();
        writer.flush().await.unwrap();
    }

    let intact = tokio::fs::read(&file_path).await.unwrap();
    let total_usage_before = RolloutReader::open(&file_path)
        .scan_summary()
        .await
        .unwrap()
        .usage_totals()
        .input_tokens();
    assert_eq!(total_usage_before, 350);

    // Destroy the head. Same length, so the file layout and every offset still line up.
    let mut damaged = intact.clone();
    let head = usize::try_from(head_len).unwrap();
    for byte in &mut damaged[..head - 1] {
        *byte = b'x';
    }
    damaged[head - 1] = b'\n';
    tokio::fs::write(&file_path, &damaged).await.unwrap();
    assert!(
        RolloutReader::open(&file_path).read_all().await.is_err(),
        "a full scan of the damaged file must fail, or this test proves nothing"
    );

    let writer = RolloutWriter::open(&file_path, session_id.clone())
        .await
        .expect("resume must be served from the checkpoint");
    assert_eq!(
        writer.usage_totals().input_tokens(),
        350,
        "checkpoint totals plus the tail must equal the full-scan total"
    );
    assert_eq!(writer.next_timeline_seq(), 7);
}

/// The checkpoint route and the full scan must agree, and losing the checkpoint must cost only
/// time. Each way of losing it falls back to the same answer.
#[tokio::test]
async fn test_rollout_writer_checkpoint_recovery_matches_full_scan() {
    let dir = temp_test_dir("checkpoint_matches_full_scan");
    let session_id = SessionId::generate();
    let run_a = RunId::generate();
    let run_b = RunId::generate();
    let agent_id = AgentId::new("agent");
    let file_path = dir.join(format!("rollout-{}.jsonl", session_id.as_str()));
    let sidecar_path = dir.join(format!(
        "rollout-{}.jsonl.sidecar.json",
        session_id.as_str()
    ));

    let allocator = RunState::start(run_a.clone()).restore_event_seq_allocator(None);
    {
        let mut writer = RolloutWriter::open(&file_path, session_id.clone())
            .await
            .unwrap();
        writer.set_checkpoint_interval(3);
        writer
            .append_session_meta(RolloutSessionMeta::new(session_id.clone()))
            .await
            .unwrap();
        for i in 0..10 {
            writer
                .append_model_usage(
                    RolloutModelUsage::new(
                        run_b.clone(),
                        Usage::from_request(RequestUsage::new(10, 1)),
                    )
                    .with_turn_index(i),
                )
                .await
                .unwrap();
            let event = HostEvent::allocate(
                &allocator,
                agent_id.clone(),
                HostEventBody::Exec(ExecEvent::Started(ExecStartedEvent::new(
                    ExecSessionId::generate(),
                    "echo",
                ))),
            )
            .unwrap();
            writer.append_event(event).await.unwrap();
        }
        writer.flush().await.unwrap();
    }

    let full = RolloutReader::open(&file_path)
        .scan_summary()
        .await
        .unwrap();
    let expected_seq = full.last_timeline_seq().unwrap() + 1;

    // Every way the index can be unusable must land on the same state as the full scan.
    let sidecar_bytes = tokio::fs::read(&sidecar_path).await.unwrap();
    let cases: Vec<(&str, Option<Vec<u8>>)> = vec![
        ("index present", Some(sidecar_bytes.clone())),
        ("index missing", None),
        ("index truncated", Some(b"{\"schema_version\":1".to_vec())),
        (
            "index points past EOF",
            Some(
                serde_json::to_vec(
                    &RolloutSidecar::new(
                        session_id.clone(),
                        999,
                        HashMap::new(),
                        Usage::from_request(RequestUsage::new(1, 1)),
                    )
                    .with_last_checkpoint_offset(u64::MAX / 2),
                )
                .unwrap(),
            ),
        ),
        (
            "index points at a non-checkpoint record",
            Some(
                serde_json::to_vec(
                    &RolloutSidecar::new(
                        session_id.clone(),
                        999,
                        HashMap::new(),
                        Usage::from_request(RequestUsage::new(1, 1)),
                    )
                    .with_last_checkpoint_offset(0),
                )
                .unwrap(),
            ),
        ),
    ];

    for (label, bytes) in cases {
        match bytes {
            Some(b) => tokio::fs::write(&sidecar_path, b).await.unwrap(),
            None => {
                tokio::fs::remove_file(&sidecar_path).await.ok();
            }
        }

        let writer = RolloutWriter::open(&file_path, session_id.clone())
            .await
            .unwrap_or_else(|e| panic!("{label}: recovery must succeed, got {e}"));
        assert_eq!(
            writer.next_timeline_seq(),
            expected_seq,
            "{label}: sequence must match the full scan"
        );
        assert_eq!(
            writer.usage_totals(),
            full.usage_totals(),
            "{label}: usage ledger must match the full scan"
        );
        assert_eq!(
            writer.persisted_run_max_seq(&run_a),
            full.persisted_run_max_seq().get(&run_a).copied(),
            "{label}: run max seq must match the full scan"
        );
    }
}

/// A checkpoint that names another session must not be believed.
///
/// The forged copy also inflates the usage ledger, so believing it and rebuilding from the log
/// give different answers — otherwise the test would pass either way.
#[tokio::test]
async fn test_rollout_writer_rejects_checkpoint_from_another_session() {
    let dir = temp_test_dir("checkpoint_foreign_session");
    let session_id = SessionId::generate();
    let run_id = RunId::generate();
    let file_path = dir.join(format!("rollout-{}.jsonl", session_id.as_str()));

    let checkpoint_offset;
    {
        let mut writer = RolloutWriter::open(&file_path, session_id.clone())
            .await
            .unwrap();
        writer.set_checkpoint_interval(2);
        for _ in 0..2 {
            writer
                .append_model_usage(RolloutModelUsage::new(
                    run_id.clone(),
                    Usage::from_request(RequestUsage::new(10, 1)),
                ))
                .await
                .unwrap();
        }
        checkpoint_offset = writer.last_checkpoint_offset().unwrap();
        writer
            .append_item(RunItem::new(
                ItemId::new("after"),
                RunItemKind::Message(Message::user("after")),
            ))
            .await
            .unwrap();
        writer.flush().await.unwrap();
    }

    rewrite_line_at(&file_path, checkpoint_offset, |record| {
        assert_eq!(record["type"], json!("checkpoint"));
        record["payload"]["session_id"] = json!("sess-someone-else");
        record["payload"]["usage_totals"]["input_tokens"] = json!(999_999);
    })
    .await;

    // The index still points at it, but it no longer vouches for this session, so the fast path
    // declines it and the scan runs — which then finds the inflated ledger contradicting the
    // records and says so. Believing the checkpoint instead would have returned 999_999 happily.
    let err = RolloutWriter::open(&file_path, session_id)
        .await
        .expect_err("a foreign checkpoint must not be adopted");
    assert!(
        matches!(
            err,
            Error::Session {
                kind: SessionErrorKind::Corrupted,
                ..
            }
        ) && err.to_string().contains("999999"),
        "expected the scan to reject the forged ledger, got: {err}"
    );
}

/// The tail scan cannot see the record before it, so a first tail record that fails to advance
/// past the checkpoint is invisible to the within-tail monotonicity check. The join has to be
/// checked separately or a backwards sequence slips through the fast path.
#[tokio::test]
async fn test_rollout_writer_rejects_non_monotonic_join_across_checkpoint() {
    let dir = temp_test_dir("checkpoint_join_monotonicity");
    let session_id = SessionId::generate();
    let run_id = RunId::generate();
    let file_path = dir.join(format!("rollout-{}.jsonl", session_id.as_str()));

    let tail_offset;
    {
        let mut writer = RolloutWriter::open(&file_path, session_id.clone())
            .await
            .unwrap();
        writer.set_checkpoint_interval(2);
        for _ in 0..2 {
            writer
                .append_model_usage(RolloutModelUsage::new(
                    run_id.clone(),
                    Usage::from_request(RequestUsage::new(10, 1)),
                ))
                .await
                .unwrap();
        }
        // seq 0 and 1 are the usage records; the checkpoint took seq 2.
        tail_offset = tokio::fs::metadata(&file_path).await.unwrap().len();
        writer
            .append_item(RunItem::new(
                ItemId::new("tail"),
                RunItemKind::Message(Message::user("tail")),
            ))
            .await
            .unwrap();
        writer.flush().await.unwrap();
    }

    // Drag the single tail record back to a sequence the checkpoint already covers.
    rewrite_line_at(&file_path, tail_offset, |record| {
        assert_eq!(record["timeline_seq"], json!(3));
        record["timeline_seq"] = json!(1);
    })
    .await;

    let err = RolloutWriter::open(&file_path, session_id)
        .await
        .expect_err("a backwards sequence across the checkpoint must be caught");
    assert!(
        matches!(
            err,
            Error::Session {
                kind: SessionErrorKind::Corrupted,
                ..
            }
        ),
        "expected Corrupted, got: {err}"
    );
}

/// Rewrites the single JSONL record starting at `offset`, keeping the file's line structure.
async fn rewrite_line_at(
    path: &std::path::Path,
    offset: u64,
    edit: impl FnOnce(&mut serde_json::Value),
) {
    let text = tokio::fs::read_to_string(path).await.unwrap();
    let idx = text[..usize::try_from(offset).unwrap()].lines().count();
    let mut lines: Vec<String> = text.lines().map(ToOwned::to_owned).collect();

    let mut record: serde_json::Value = serde_json::from_str(&lines[idx]).unwrap();
    edit(&mut record);
    lines[idx] = serde_json::to_string(&record).unwrap();

    tokio::fs::write(path, format!("{}\n", lines.join("\n")))
        .await
        .unwrap();
}

/// A checkpoint whose figures were edited in place, keeping the JSON, the session and the
/// sequence intact, is believed by the fast path — it never reads the records that would
/// contradict it. Any full scan must catch it, which is the only place the contradiction is
/// visible.
#[tokio::test]
async fn test_scan_catches_a_checkpoint_that_disagrees_with_its_records() {
    let dir = temp_test_dir("checkpoint_value_tampering");
    let session_id = SessionId::generate();
    let run_id = RunId::generate();
    let file_path = dir.join(format!("rollout-{}.jsonl", session_id.as_str()));

    let checkpoint_offset;
    {
        let mut writer = RolloutWriter::open(&file_path, session_id.clone())
            .await
            .unwrap();
        writer.set_checkpoint_interval(2);
        for _ in 0..2 {
            writer
                .append_model_usage(RolloutModelUsage::new(
                    run_id.clone(),
                    Usage::from_request(RequestUsage::new(10, 1)),
                ))
                .await
                .unwrap();
        }
        checkpoint_offset = writer.last_checkpoint_offset().unwrap();
        writer
            .append_item(RunItem::new(
                ItemId::new("tail"),
                RunItemKind::Message(Message::user("tail")),
            ))
            .await
            .unwrap();
        writer.flush().await.unwrap();
    }

    rewrite_line_at(&file_path, checkpoint_offset, |record| {
        assert_eq!(record["type"], json!("checkpoint"));
        record["payload"]["usage_totals"]["input_tokens"] = json!(999_999);
    })
    .await;

    let err = RolloutReader::open(&file_path)
        .scan_summary()
        .await
        .expect_err("a checkpoint contradicting its own records must be reported");
    assert!(
        matches!(
            err,
            Error::Session {
                kind: SessionErrorKind::Corrupted,
                ..
            }
        ),
        "expected Corrupted, got: {err}"
    );
    assert!(err.to_string().contains("999999"), "got: {err}");
}

/// The request count is checked alongside the token counters. Without it, a checkpoint claiming the
/// same tokens over a different number of calls passes reconciliation, and every per-request
/// average derived from it is quietly wrong.
#[tokio::test]
async fn test_scan_catches_a_checkpoint_that_miscounts_its_requests() {
    let dir = temp_test_dir("checkpoint_request_count_tampering");
    let session_id = SessionId::generate();
    let run_id = RunId::generate();
    let file_path = dir.join(format!("rollout-{}.jsonl", session_id.as_str()));

    let checkpoint_offset;
    {
        let mut writer = RolloutWriter::open(&file_path, session_id.clone())
            .await
            .unwrap();
        writer.set_checkpoint_interval(2);
        for _ in 0..2 {
            writer
                .append_model_usage(RolloutModelUsage::new(
                    run_id.clone(),
                    Usage::from_request(RequestUsage::new(10, 1)),
                ))
                .await
                .unwrap();
        }
        checkpoint_offset = writer.last_checkpoint_offset().unwrap();
        writer.flush().await.unwrap();
    }

    rewrite_line_at(&file_path, checkpoint_offset, |record| {
        assert_eq!(record["type"], json!("checkpoint"));
        assert_eq!(record["payload"]["usage_totals"]["requests"], json!(2));
        record["payload"]["usage_totals"]["requests"] = json!(1);
    })
    .await;

    let err = RolloutReader::open(&file_path)
        .scan_summary()
        .await
        .expect_err("a checkpoint miscounting its requests must be reported");
    assert!(
        matches!(
            err,
            Error::Session {
                kind: SessionErrorKind::Corrupted,
                ..
            }
        ),
        "expected Corrupted, got: {err}"
    );
}

/// The checkpoint interval has to survive restarts. Counting from zero on every open lets a
/// process that stops just short of the interval start over, so a session restarted often enough
/// would never checkpoint and the tail would grow without bound.
#[tokio::test]
async fn test_rollout_writer_checkpoint_interval_survives_restarts() {
    let dir = temp_test_dir("checkpoint_interval_restarts");
    let session_id = SessionId::generate();
    let file_path = dir.join(format!("rollout-{}.jsonl", session_id.as_str()));

    // Four rounds of four records, always stopping one short of the interval of five.
    for round in 0..4 {
        let mut writer = RolloutWriter::open(&file_path, session_id.clone())
            .await
            .unwrap();
        writer.set_checkpoint_interval(5);
        for i in 0..4 {
            writer
                .append_item(RunItem::new(
                    ItemId::new(format!("r{round}-{i}")),
                    RunItemKind::Message(Message::user("x")),
                ))
                .await
                .unwrap();
        }
        writer.flush().await.unwrap();
    }

    let records = RolloutReader::open(&file_path).read_all().await.unwrap();
    let checkpoints = records
        .iter()
        .filter(|r| r.type_name() == "checkpoint")
        .count();
    assert!(
        checkpoints >= 3,
        "16 records at an interval of 5 must produce at least 3 checkpoints, got {checkpoints}"
    );
}

/// A sidecar left pointing at an older checkpoint must not pin recovery there forever: the tail
/// scan passes newer checkpoints and the newest one has to become the index.
#[tokio::test]
async fn test_rollout_writer_index_advances_past_a_stale_checkpoint_offset() {
    let dir = temp_test_dir("checkpoint_index_advances");
    let session_id = SessionId::generate();
    let file_path = dir.join(format!("rollout-{}.jsonl", session_id.as_str()));
    let sidecar_path = dir.join(format!(
        "rollout-{}.jsonl.sidecar.json",
        session_id.as_str()
    ));

    let (first_offset, newest_offset) = {
        let mut writer = RolloutWriter::open(&file_path, session_id.clone())
            .await
            .unwrap();
        writer.set_checkpoint_interval(2);
        for _ in 0..2 {
            writer
                .append_item(RunItem::new(
                    ItemId::new("a"),
                    RunItemKind::Message(Message::user("a")),
                ))
                .await
                .unwrap();
        }
        let first = writer.last_checkpoint_offset().unwrap();
        for _ in 0..6 {
            writer
                .append_item(RunItem::new(
                    ItemId::new("b"),
                    RunItemKind::Message(Message::user("b")),
                ))
                .await
                .unwrap();
        }
        writer.flush().await.unwrap();
        (first, writer.last_checkpoint_offset().unwrap())
    };
    assert_ne!(first_offset, newest_offset);

    // Stand in for a sidecar update that failed after the first checkpoint.
    let current: RolloutSidecar =
        serde_json::from_slice(&tokio::fs::read(&sidecar_path).await.unwrap()).unwrap();
    let stale = RolloutSidecar::new(
        session_id.clone(),
        current.next_timeline_seq(),
        current.persisted_run_max_seq().clone(),
        current.usage_totals().clone(),
    )
    .with_last_checkpoint_offset(first_offset);
    tokio::fs::write(&sidecar_path, serde_json::to_vec(&stale).unwrap())
        .await
        .unwrap();

    let writer = RolloutWriter::open(&file_path, session_id).await.unwrap();
    assert_eq!(
        writer.last_checkpoint_offset(),
        Some(newest_offset),
        "recovery stayed pinned to the stale offset and will re-read the tail forever"
    );
}

/// Tampering with only the checkpoint's identity, leaving its figures correct, used to leave the
/// index permanently poisoned: the scan recorded the offset, recovery wrote it back, and the next
/// open refused it again — a full scan forever, with nothing ever reported.
///
/// A log that states its own identity now contradicts the checkpoint outright. A log that does
/// not is covered by recovery refusing to index a checkpoint it cannot use.
#[tokio::test]
async fn test_rollout_writer_does_not_index_an_unusable_checkpoint() {
    let dir = temp_test_dir("checkpoint_identity_only_tamper");
    let session_id = SessionId::new("sess-real");
    let run_id = RunId::generate();
    let file_path = dir.join("rollout-sess-real.jsonl");
    let sidecar_path = dir.join("rollout-sess-real.jsonl.sidecar.json");

    // No `session_meta` here on purpose: the log does not state its identity, so the checkpoint
    // cannot be contradicted and only the indexing rule protects recovery.
    let bad_offset;
    let good_offset;
    {
        let mut writer = RolloutWriter::open(&file_path, session_id.clone())
            .await
            .unwrap();
        writer.set_checkpoint_interval(2);
        for _ in 0..2 {
            writer
                .append_model_usage(RolloutModelUsage::new(
                    run_id.clone(),
                    Usage::from_request(RequestUsage::new(10, 1)),
                ))
                .await
                .unwrap();
        }
        good_offset = writer.last_checkpoint_offset().unwrap();
        for _ in 0..2 {
            writer
                .append_model_usage(RolloutModelUsage::new(
                    run_id.clone(),
                    Usage::from_request(RequestUsage::new(10, 1)),
                ))
                .await
                .unwrap();
        }
        bad_offset = writer.last_checkpoint_offset().unwrap();
        writer.flush().await.unwrap();
    }
    assert_ne!(good_offset, bad_offset);

    // Only the identity changes; the ledger stays truthful.
    rewrite_line_at(&file_path, bad_offset, |record| {
        assert_eq!(record["type"], json!("checkpoint"));
        record["payload"]["session_id"] = json!("sess-other");
    })
    .await;

    let mut writer = RolloutWriter::open(&file_path, session_id.clone())
        .await
        .unwrap();
    assert_eq!(
        writer.usage_totals().input_tokens(),
        40,
        "the ledger must still be right"
    );
    assert_ne!(
        writer.last_checkpoint_offset(),
        Some(bad_offset),
        "an offset the fast path will refuse must not be written back as the index"
    );

    // Recovery declining to index it is only half the requirement; the index also has to come
    // back on its own. Writing on restores it at the next interval.
    writer.set_checkpoint_interval(2);
    for i in 0..2 {
        writer
            .append_item(RunItem::new(
                ItemId::new(format!("heal-{i}")),
                RunItemKind::Message(Message::user("heal")),
            ))
            .await
            .unwrap();
    }
    let healed = writer
        .last_checkpoint_offset()
        .expect("a fresh checkpoint must restore the index");
    assert_ne!(healed, bad_offset);
    writer.flush().await.unwrap();
    drop(writer);

    let sidecar: RolloutSidecar =
        serde_json::from_slice(&tokio::fs::read(&sidecar_path).await.unwrap()).unwrap();
    assert_eq!(
        sidecar.last_checkpoint_offset(),
        Some(healed),
        "the healed offset must reach the sidecar"
    );

    // And the next open uses it rather than scanning from the top again.
    let reopened = RolloutWriter::open(&file_path, session_id).await.unwrap();
    assert_eq!(reopened.last_checkpoint_offset(), Some(healed));
    assert_eq!(reopened.usage_totals().input_tokens(), 40);
}

/// When the log does state its identity, a checkpoint naming another session is a contradiction
/// inside the log and must be reported rather than silently skipped.
#[tokio::test]
async fn test_scan_catches_a_checkpoint_naming_another_session() {
    let dir = temp_test_dir("checkpoint_identity_contradiction");
    let session_id = SessionId::new("sess-real");
    let run_id = RunId::generate();
    let file_path = dir.join("rollout-sess-real.jsonl");

    let checkpoint_offset;
    {
        let mut writer = RolloutWriter::open(&file_path, session_id.clone())
            .await
            .unwrap();
        writer.set_checkpoint_interval(3);
        writer
            .append_session_meta(RolloutSessionMeta::new(session_id.clone()))
            .await
            .unwrap();
        for _ in 0..2 {
            writer
                .append_model_usage(RolloutModelUsage::new(
                    run_id.clone(),
                    Usage::from_request(RequestUsage::new(10, 1)),
                ))
                .await
                .unwrap();
        }
        checkpoint_offset = writer.last_checkpoint_offset().unwrap();
        writer.flush().await.unwrap();
    }

    rewrite_line_at(&file_path, checkpoint_offset, |record| {
        assert_eq!(record["type"], json!("checkpoint"));
        record["payload"]["session_id"] = json!("sess-other");
    })
    .await;

    let err = RolloutReader::open(&file_path)
        .scan_summary()
        .await
        .expect_err("a checkpoint naming another session must be reported");
    assert!(
        matches!(
            err,
            Error::Session {
                kind: SessionErrorKind::Corrupted,
                ..
            }
        ) && err.to_string().contains("sess-other"),
        "expected Corrupted naming the foreign session, got: {err}"
    );
}

/// `read_all` checks envelopes; `scan_summary` checks meaning. The docs now say so, and this
/// pins the difference.
#[tokio::test]
async fn test_read_all_validates_envelopes_while_scan_summary_validates_content() {
    let dir = temp_test_dir("read_all_vs_scan_summary");
    let session_id = SessionId::generate();
    let run_id = RunId::generate();
    let file_path = dir.join(format!("rollout-{}.jsonl", session_id.as_str()));

    let checkpoint_offset;
    {
        let mut writer = RolloutWriter::open(&file_path, session_id.clone())
            .await
            .unwrap();
        writer.set_checkpoint_interval(2);
        for _ in 0..2 {
            writer
                .append_model_usage(RolloutModelUsage::new(
                    run_id.clone(),
                    Usage::from_request(RequestUsage::new(10, 1)),
                ))
                .await
                .unwrap();
        }
        checkpoint_offset = writer.last_checkpoint_offset().unwrap();
        writer.flush().await.unwrap();
    }

    rewrite_line_at(&file_path, checkpoint_offset, |record| {
        record["payload"]["usage_totals"]["input_tokens"] = json!(777_777);
    })
    .await;

    let reader = RolloutReader::open(&file_path);
    assert!(
        reader.read_all().await.is_ok(),
        "read_all only validates envelopes, so it cannot be the authoritative check"
    );
    assert!(
        reader.scan_summary().await.is_err(),
        "scan_summary is the check that reads the contents"
    );
}

#[tokio::test]
async fn test_rollout_writer_path_traversal_rejection() {
    let dir = temp_test_dir("path_traversal");
    let invalid_session_id = SessionId::new("../malicious_session");
    let result = RolloutWriter::create_for_session(&dir, invalid_session_id).await;
    assert!(result.is_err());
    assert!(matches!(result.unwrap_err(), Error::Caller { .. }));
}

#[tokio::test]
async fn test_rollout_writer_reopen_and_resume_preserves_sequences() {
    let dir = temp_test_dir("reopen_preserves_seq");
    let session_id = SessionId::generate();
    let file_path = dir.join(format!("rollout-{}.jsonl", session_id.as_str()));

    // Write initial 5 records
    {
        let mut writer = RolloutWriter::open(&file_path, session_id.clone())
            .await
            .unwrap();
        for i in 0..5 {
            writer
                .append_item(RunItem::new(
                    ItemId::new(format!("item-{i}")),
                    RunItemKind::Message(Message::user(format!("Message {i}"))),
                ))
                .await
                .unwrap();
        }
        assert_eq!(writer.next_timeline_seq(), 5);
    }

    // Reopen and write next 5 records
    {
        let mut writer = RolloutWriter::open(&file_path, session_id.clone())
            .await
            .unwrap();
        assert_eq!(writer.next_timeline_seq(), 5);

        for i in 5..10 {
            let rec = writer
                .append_item(RunItem::new(
                    ItemId::new(format!("item-{i}")),
                    RunItemKind::Message(Message::user(format!("Message {i}"))),
                ))
                .await
                .unwrap();
            assert_eq!(rec.timeline_seq(), i as u64);
        }
        assert_eq!(writer.next_timeline_seq(), 10);
    }

    let records = RolloutReader::open(&file_path).read_all().await.unwrap();
    assert_eq!(records.len(), 10);
    for (i, rec) in records.iter().enumerate() {
        assert_eq!(rec.timeline_seq(), i as u64);
    }
}
