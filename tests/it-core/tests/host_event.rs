//! Tests for HostEvent envelope, emission, sinks, forward compatibility, and attribution.

use std::{
    collections::HashSet,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use ra_core::{
    agent::AgentSpec,
    compat::SchemaVersion,
    context::RunContext,
    event::{
        AgentEvent, EventTimestamp, ExecEvent, FnHostEventSink, HOST_EVENT_SCHEMA_VERSION,
        HostEvent, HostEventBody, HostEventEmitter, HostEventSink, InMemoryHostEventSink,
        NoopHostEventSink,
        agent::{
            AgentClosedEvent, AgentCompletedEvent, AgentMessageSentEvent, AgentOperationId,
            AgentSpawnedEvent, AgentStatus, AgentStatusChangedEvent,
        },
        exec::{
            EXEC_EVENT_SCHEMA_VERSION, ExecEvictedEvent, ExecEvictionReason, ExecExitedEvent, ExecOutputEvent, ExecSessionId,
            ExecStartedEvent, ExecStreamKind, ExecYieldReason, ExecYieldedEvent,
            TerminalInteractionEvent,
        },
    },
    item::AgentId,
    state::{RunId, RunState},
    tool::ToolServices,
};
use serde_json::json;

#[test]
fn test_exec_session_id_uuid7_uniqueness() {
    let mut seen = HashSet::new();
    for _ in 0..10_000 {
        let id = ExecSessionId::generate();
        assert!(id.as_str().starts_with("exec-"));
        assert!(
            seen.insert(id.as_str().to_owned()),
            "collision detected in ExecSessionId::generate"
        );
    }
}

#[test]
fn test_host_event_allocate_with_allocator() {
    let run_id = RunId::new("run-evt-alloc");
    let state = RunState::start(run_id.clone());
    let allocator = state.restore_event_seq_allocator(None);
    let agent_id = AgentId::new("coder");

    let started = ExecStartedEvent::new(ExecSessionId::new("exec-100"), "cargo check")
        .with_args(["--workspace"])
        .with_cwd("/workspace")
        .with_pid(12345);

    let event = HostEvent::allocate(
        &allocator,
        agent_id.clone(),
        HostEventBody::Exec(ExecEvent::Started(started)),
    )
    .expect("must allocate host event");

    assert_eq!(event.schema_version(), HOST_EVENT_SCHEMA_VERSION);
    assert_eq!(event.seq(), 0);
    assert_eq!(event.run_id(), &run_id);
    assert_eq!(event.agent_id(), &agent_id);
    assert!(event.at().as_millis() > 0);
    assert!(event.unknown().is_empty());

    if let HostEventBody::Exec(ExecEvent::Started(evt)) = event.body() {
        assert_eq!(evt.session_id().as_str(), "exec-100");
        assert_eq!(evt.command(), "cargo check");
        assert_eq!(evt.args(), &["--workspace"]);
        assert_eq!(evt.cwd(), Some("/workspace"));
        assert_eq!(evt.pid(), Some(12345));
    } else {
        panic!("unexpected event body");
    }

    // Allocate second event -> seq must be 1
    let event2 = HostEvent::allocate(
        &allocator,
        agent_id,
        HostEventBody::Exec(ExecEvent::Evicted(ExecEvictedEvent::new(
            ExecSessionId::new("exec-100"),
            ExecEvictionReason::IdleTimeout,
        ))),
    )
    .expect("must allocate second event");

    assert_eq!(event2.seq(), 1);
    assert_eq!(event2.run_id(), &run_id);
}

#[test]
fn test_host_event_with_timestamp() {
    let run_id = RunId::new("run-ts-override");
    let state = RunState::start(run_id);
    let allocator = state.restore_event_seq_allocator(None);
    let agent_id = AgentId::new("coder");

    let custom_at = EventTimestamp::from_millis(1_700_000_000_000);
    let event = HostEvent::allocate(
        &allocator,
        agent_id,
        HostEventBody::Exec(ExecEvent::Started(ExecStartedEvent::new(
            ExecSessionId::new("s"),
            "true",
        ))),
    )
    .expect("must allocate")
    .with_timestamp(custom_at);

    assert_eq!(event.at(), custom_at);
}

#[test]
fn test_host_event_emitter_monotone_dispatch() {
    let run_id = RunId::new("run-emitter-1");
    let state = RunState::start(run_id.clone());
    let allocator = state.restore_event_seq_allocator(None);
    let sink = Arc::new(InMemoryHostEventSink::new());
    let agent_id = AgentId::new("worker");

    let emitter = HostEventEmitter::new(agent_id.clone(), allocator, sink.clone());
    assert_eq!(emitter.run_id(), &run_id);
    assert_eq!(emitter.agent_id(), &agent_id);

    let seq0 = emitter
        .emit_exec(ExecEvent::Started(ExecStartedEvent::new(
            ExecSessionId::new("s1"),
            "echo hello",
        )))
        .expect("must emit");
    let seq1 = emitter
        .emit_agent(AgentEvent::StatusChanged(AgentStatusChangedEvent::new(
            AgentStatus::Idle,
            AgentStatus::Running,
        )))
        .expect("must emit");
    let seq2 = emitter
        .emit_exec(ExecEvent::Exited(
            ExecExitedEvent::new(ExecSessionId::new("s1"), 250, 10, 0).with_exit_code(0),
        ))
        .expect("must emit");

    assert_eq!(seq0, 0);
    assert_eq!(seq1, 1);
    assert_eq!(seq2, 2);

    assert_eq!(sink.len(), 3);
    let events = sink.events();
    assert_eq!(events[0].seq(), 0);
    assert_eq!(events[0].run_id(), &run_id);
    assert_eq!(events[1].seq(), 1);
    assert_eq!(events[1].run_id(), &run_id);
    assert_eq!(events[2].seq(), 2);
    assert_eq!(events[2].run_id(), &run_id);
}

#[test]
fn test_all_exec_events_serde_roundtrip_equality() {
    let run_id = RunId::new("run-exec-all");
    let state = RunState::start(run_id);
    let allocator = state.restore_event_seq_allocator(None);
    let agent_id = AgentId::new("coder");
    let sid = ExecSessionId::new("exec-10");

    let exec_variants = vec![
        ExecEvent::Started(
            ExecStartedEvent::new(sid.clone(), "cargo test")
                .with_args(["--", "test_name"])
                .with_cwd("/src")
                .with_pid(42)
                .with_pty(true),
        ),
        ExecEvent::Output(
            ExecOutputEvent::new(sid.clone(), ExecStreamKind::Stdout, 0, 10, "output text")
                .with_truncated(false),
        ),
        ExecEvent::Yielded(ExecYieldedEvent::new(
            sid.clone(),
            10,
            0,
            ExecYieldReason::InitialTimeout,
        )),
        ExecEvent::TerminalInteraction(TerminalInteractionEvent::new(sid.clone(), 8, false)),
        ExecEvent::Exited(ExecExitedEvent::new(sid.clone(), 250, 100, 0).with_exit_code(0)),
        ExecEvent::Evicted(ExecEvictedEvent::new(sid, ExecEvictionReason::IdleTimeout)),
    ];

    for variant in exec_variants {
        let envelope =
            HostEvent::allocate(&allocator, agent_id.clone(), HostEventBody::Exec(variant))
                .expect("must allocate");

        let serialized = serde_json::to_string(&envelope).expect("must serialize envelope");
        let restored: HostEvent =
            serde_json::from_str(&serialized).expect("must deserialize envelope");

        assert_eq!(
            restored, envelope,
            "serde round-trip equality failed for ExecEvent"
        );
        assert!(
            restored.unknown().is_empty(),
            "envelope unknown must be empty"
        );

        match restored.body() {
            HostEventBody::Exec(ExecEvent::Started(e)) => assert!(e.unknown().is_empty()),
            HostEventBody::Exec(ExecEvent::Output(e)) => assert!(e.unknown().is_empty()),
            HostEventBody::Exec(ExecEvent::Yielded(e)) => assert!(e.unknown().is_empty()),
            HostEventBody::Exec(ExecEvent::TerminalInteraction(e)) => {
                assert!(e.unknown().is_empty())
            }
            HostEventBody::Exec(ExecEvent::Exited(e)) => assert!(e.unknown().is_empty()),
            HostEventBody::Exec(ExecEvent::Evicted(e)) => assert!(e.unknown().is_empty()),
            _ => panic!("unexpected event body"),
        }
    }
}

#[test]
fn test_all_agent_events_serde_roundtrip_equality() {
    let run_id = RunId::new("run-agent-all");
    let state = RunState::start(run_id);
    let allocator = state.restore_event_seq_allocator(None);
    let agent_id = AgentId::new("orchestrator");

    let agent_variants = vec![
        AgentEvent::Spawned(
            AgentSpawnedEvent::new(AgentId::new("sub-worker"))
                .with_child_run_id(RunId::new("run-child-1"))
                .with_operation_id(AgentOperationId::new("op-1")),
        ),
        AgentEvent::MessageSent(
            AgentMessageSentEvent::new(
                AgentId::new("orchestrator"),
                AgentId::new("sub-worker"),
                "please review",
            )
            .with_message_id("msg-100"),
        ),
        AgentEvent::StatusChanged(AgentStatusChangedEvent::new(
            AgentStatus::Idle,
            AgentStatus::Running,
        )),
        AgentEvent::Completed(AgentCompletedEvent::new("success")),
        AgentEvent::Closed(AgentClosedEvent::new("completed")),
    ];

    for variant in agent_variants {
        let envelope =
            HostEvent::allocate(&allocator, agent_id.clone(), HostEventBody::Agent(variant))
                .expect("must allocate");

        let serialized = serde_json::to_string(&envelope).expect("must serialize envelope");
        let restored: HostEvent =
            serde_json::from_str(&serialized).expect("must deserialize envelope");

        assert_eq!(
            restored, envelope,
            "serde round-trip equality failed for AgentEvent"
        );
        assert!(
            restored.unknown().is_empty(),
            "envelope unknown must be empty"
        );

        match restored.body() {
            HostEventBody::Agent(AgentEvent::Spawned(e)) => assert!(e.unknown().is_empty()),
            HostEventBody::Agent(AgentEvent::MessageSent(e)) => assert!(e.unknown().is_empty()),
            HostEventBody::Agent(AgentEvent::StatusChanged(e)) => assert!(e.unknown().is_empty()),
            HostEventBody::Agent(AgentEvent::Completed(e)) => assert!(e.unknown().is_empty()),
            HostEventBody::Agent(AgentEvent::Closed(e)) => assert!(e.unknown().is_empty()),
            _ => panic!("unexpected event body"),
        }
    }
}

#[test]
fn test_forward_compat_unknown_event_family() {
    let payload = json!({
        "schema_version": 1,
        "seq": 42,
        "run_id": "run-compat-1",
        "agent_id": "orchestrator",
        "at": 1700000000000_u64,
        "body": {
            "family": "job",
            "data": {
                "job_id": "job-999",
                "cluster": "k8s-prod"
            }
        },
        "tracing_sample_rate": 1.0
    });

    let event: HostEvent =
        serde_json::from_value(payload.clone()).expect("must deserialize unknown family");
    assert_eq!(event.seq(), 42);
    assert_eq!(event.run_id().as_str(), "run-compat-1");

    match event.body() {
        HostEventBody::Unknown { family, data } => {
            assert_eq!(family, "job");
            assert_eq!(data["job_id"], "job-999");
            assert_eq!(data["cluster"], "k8s-prod");
        }
        _ => panic!("expected unknown body family"),
    }

    // Must re-serialize byte-for-byte without losing unknown family or unknown top-level fields
    let reserialized = serde_json::to_value(&event).expect("must serialize");
    assert_eq!(reserialized["body"]["family"], "job");
    assert_eq!(reserialized["body"]["data"]["job_id"], "job-999");
    assert_eq!(reserialized["tracing_sample_rate"], 1.0);
}

#[test]
fn test_forward_compat_unknown_event_kind() {
    let payload = json!({
        "schema_version": 1,
        "seq": 43,
        "run_id": "run-compat-2",
        "agent_id": "coder",
        "at": 1700000000000_u64,
        "body": {
            "family": "exec",
            "data": {
                "kind": "paused",
                "session_id": "exec-1",
                "reason": "user_breakpoint"
            }
        }
    });

    let event: HostEvent =
        serde_json::from_value(payload).expect("must deserialize unknown kind in known family");
    match event.body() {
        HostEventBody::Exec(ExecEvent::Unknown(val)) => {
            assert_eq!(val["kind"], "paused");
            assert_eq!(val["reason"], "user_breakpoint");
        }
        _ => panic!("expected unknown exec event variant"),
    }

    let reserialized = serde_json::to_value(&event).expect("must serialize");
    assert_eq!(reserialized["body"]["family"], "exec");
    assert_eq!(reserialized["body"]["data"]["kind"], "paused");
    assert_eq!(reserialized["body"]["data"]["reason"], "user_breakpoint");
}

#[test]
fn test_forward_compat_payload_extra_fields() {
    let payload = json!({
        "schema_version": 1,
        "seq": 44,
        "run_id": "run-compat-3",
        "agent_id": "coder",
        "at": 1700000000000_u64,
        "body": {
            "family": "exec",
            "data": {
                "kind": "started",
                "session_id": "exec-101",
                "command": "cargo run",
                "pty": false,
                "container_id": "c-alpha-99",
                "cgroup_memory_limit": 1073741824
            }
        }
    });

    let event: HostEvent =
        serde_json::from_value(payload).expect("must deserialize event with extra payload fields");
    if let HostEventBody::Exec(ExecEvent::Started(started)) = event.body() {
        assert_eq!(started.session_id().as_str(), "exec-101");
        assert_eq!(started.command(), "cargo run");
        // This fixture carries no payload `schema_version`, the way a record written before the
        // field existed would not: it defaults rather than landing in the unknown bag.
        assert_eq!(started.schema_version(), EXEC_EVENT_SCHEMA_VERSION);
        assert!(started.unknown().get("schema_version").is_none());

        // Must contain exactly the 2 unknown fields, and NOT "kind"
        assert_eq!(started.unknown().len(), 2);
        assert!(started.unknown().get("kind").is_none());
        assert_eq!(
            started.unknown().get("container_id"),
            Some(&json!("c-alpha-99"))
        );
        assert_eq!(
            started.unknown().get("cgroup_memory_limit"),
            Some(&json!(1073741824))
        );
    } else {
        panic!("expected ExecStarted event");
    }

    let reserialized = serde_json::to_value(&event).expect("must serialize");
    assert_eq!(reserialized["body"]["data"]["container_id"], "c-alpha-99");
    assert_eq!(
        reserialized["body"]["data"]["cgroup_memory_limit"],
        1073741824
    );
}

#[test]
fn test_forward_compat_non_string_event_kind() {
    // 1. ExecEvent with non-string kind (e.g. integer)
    let exec_payload = json!({
        "schema_version": 1,
        "seq": 50,
        "run_id": "run-compat-nonstr",
        "agent_id": "coder",
        "at": 1700000000000_u64,
        "body": {
            "family": "exec",
            "data": {
                "kind": 1,
                "session_id": "exec-10"
            }
        }
    });

    let exec_event: HostEvent =
        serde_json::from_value(exec_payload.clone()).expect("must deserialize non-string kind");
    match exec_event.body() {
        HostEventBody::Exec(ExecEvent::Unknown(val)) => {
            assert_eq!(val["kind"], 1);
            assert_eq!(val["session_id"], "exec-10");
        }
        _ => panic!("expected unknown exec event variant"),
    }
    let exec_reserialized = serde_json::to_value(&exec_event).expect("must serialize");
    assert_eq!(exec_reserialized, exec_payload);

    // 2. AgentEvent with non-string kind (e.g. boolean)
    let agent_payload = json!({
        "schema_version": 1,
        "seq": 51,
        "run_id": "run-compat-nonstr-agent",
        "agent_id": "orchestrator",
        "at": 1700000000000_u64,
        "body": {
            "family": "agent",
            "data": {
                "kind": true,
                "custom_field": "test"
            }
        }
    });

    let agent_event: HostEvent = serde_json::from_value(agent_payload.clone())
        .expect("must deserialize non-string agent kind");
    match agent_event.body() {
        HostEventBody::Agent(AgentEvent::Unknown(val)) => {
            assert_eq!(val["kind"], true);
            assert_eq!(val["custom_field"], "test");
        }
        _ => panic!("expected unknown agent event variant"),
    }
    let agent_reserialized = serde_json::to_value(&agent_event).expect("must serialize");
    assert_eq!(agent_reserialized, agent_payload);
}

#[test]
fn test_single_source_of_attribution() {
    // AgentStatusChangedEvent, AgentCompletedEvent, AgentClosedEvent rely on envelope agent_id
    let run_id = RunId::new("run-attrib");
    let state = RunState::start(run_id);
    let allocator = state.restore_event_seq_allocator(None);
    let agent_id = AgentId::new("reviewer");

    let event = HostEvent::allocate(
        &allocator,
        agent_id.clone(),
        HostEventBody::Agent(AgentEvent::Completed(AgentCompletedEvent::new("success"))),
    )
    .expect("must allocate");

    assert_eq!(event.agent_id(), &agent_id);
    if let HostEventBody::Agent(AgentEvent::Completed(c)) = event.body() {
        assert_eq!(c.outcome(), "success");
    } else {
        panic!("expected AgentCompleted event");
    }
}

#[test]
fn test_event_sinks_behaviors() {
    let run_id = RunId::new("run-sinks");
    let state = RunState::start(run_id);
    let allocator = state.restore_event_seq_allocator(None);

    let event = HostEvent::allocate(
        &allocator,
        AgentId::new("agent-1"),
        HostEventBody::Agent(AgentEvent::Closed(AgentClosedEvent::new("disposed"))),
    )
    .expect("must allocate");

    // 1. NoopHostEventSink
    let noop = NoopHostEventSink;
    noop.emit(event.clone());

    // 2. InMemoryHostEventSink
    let mem = InMemoryHostEventSink::new();
    assert!(mem.is_empty());
    assert_eq!(mem.len(), 0);

    mem.emit(event.clone());
    assert!(!mem.is_empty());
    assert_eq!(mem.len(), 1);
    assert_eq!(mem.events().len(), 1);

    mem.clear();
    assert!(mem.is_empty());
    assert_eq!(mem.len(), 0);

    // 3. FnHostEventSink
    let count = Arc::new(AtomicUsize::new(0));
    let last_event = Arc::new(Mutex::new(None));

    let count_clone = Arc::clone(&count);
    let last_clone = Arc::clone(&last_event);
    let fn_sink = FnHostEventSink::new(move |evt: HostEvent| {
        count_clone.fetch_add(1, Ordering::SeqCst);
        *last_clone.lock().unwrap() = Some(evt);
    });

    fn_sink.emit(event.clone());
    assert_eq!(count.load(Ordering::SeqCst), 1);
    assert_eq!(last_event.lock().unwrap().as_ref(), Some(&event));
}

#[test]
fn test_run_context_and_tool_services_event_wiring() {
    let run_id = RunId::new("run-wiring");
    let state = RunState::start(run_id.clone());
    let allocator = state.restore_event_seq_allocator(None);
    let sink = Arc::new(InMemoryHostEventSink::new());

    let spec = AgentSpec::builder()
        .id(AgentId::new("coder"))
        .name("coder")
        .instructions("help")
        .build()
        .expect("must build spec");

    let context = RunContext::new(run_id.clone(), &spec).with_event_seq_allocator(allocator);

    let services = ToolServices::new().with_event_sink(Arc::clone(&sink) as Arc<dyn HostEventSink>);
    assert!(services.event_sink().is_some());

    // Emitter derived from context + services sink
    let emitter = context
        .event_emitter(sink.clone())
        .expect("must derive emitter");
    let seq = emitter
        .emit_exec(ExecEvent::Evicted(ExecEvictedEvent::new(
            ExecSessionId::new("s"),
            ExecEvictionReason::CapacityExceeded,
        )))
        .expect("must emit via derived emitter");

    assert_eq!(seq, 0);
    assert_eq!(sink.len(), 1);
    assert_eq!(sink.events()[0].seq(), 0);
    assert_eq!(sink.events()[0].run_id(), &run_id);
}

#[test]
fn test_schema_version_constants() {
    assert_eq!(HOST_EVENT_SCHEMA_VERSION, SchemaVersion::new(1));
}

#[test]
fn test_open_labels_serialize_as_bare_strings() {
    // Known variants and custom ones are indistinguishable on the wire: both are bare strings.
    assert_eq!(
        serde_json::to_string(&AgentStatus::Idle).unwrap(),
        "\"idle\""
    );
    assert_eq!(
        serde_json::to_string(&AgentStatus::WaitingForInput).unwrap(),
        "\"waiting_for_input\""
    );
    assert_eq!(
        serde_json::to_string(&AgentStatus::custom("compacting")).unwrap(),
        "\"compacting\""
    );
    assert_eq!(
        serde_json::to_string(&ExecEvictionReason::IdleTimeout).unwrap(),
        "\"idle_timeout\""
    );
    assert_eq!(
        serde_json::to_string(&ExecEvictionReason::custom("memory_pressure")).unwrap(),
        "\"memory_pressure\""
    );
    assert_eq!(
        serde_json::to_string(&ExecYieldReason::ExplicitYield).unwrap(),
        "\"explicit_yield\""
    );
    assert_eq!(
        serde_json::to_string(&ExecYieldReason::custom("sandbox_pause")).unwrap(),
        "\"sandbox_pause\""
    );

    // `as_str` is the same bytes the serializer writes.
    assert_eq!(AgentStatus::WaitingForInput.as_str(), "waiting_for_input");
    assert_eq!(ExecEvictionReason::HostShutdown.as_str(), "host_shutdown");
    assert_eq!(ExecYieldReason::BufferExceeded.as_str(), "buffer_exceeded");
    assert_eq!(AgentStatus::custom("compacting").as_str(), "compacting");
}

#[test]
fn test_open_labels_keep_unknown_names_instead_of_failing() {
    let status: AgentStatus =
        serde_json::from_str("\"paused\"").expect("unknown status must parse");
    assert_eq!(status, AgentStatus::custom("paused"));

    let reason: ExecEvictionReason =
        serde_json::from_str("\"memory_pressure\"").expect("unknown reason must parse");
    assert_eq!(reason, ExecEvictionReason::custom("memory_pressure"));

    let yielded: ExecYieldReason =
        serde_json::from_str("\"sandbox_pause\"").expect("unknown yield reason must parse");
    assert_eq!(yielded, ExecYieldReason::custom("sandbox_pause"));

    // A name that is already known normalizes rather than staying custom, so a label written by a
    // newer build and rewritten by an older one is read as the real variant again. Every sanctioned
    // way in normalizes, including the constructor: `custom("idle")` must not produce a value that
    // fails its own round trip.
    assert_eq!(AgentStatus::custom("idle"), AgentStatus::Idle);
    assert_eq!(AgentStatus::from("idle"), AgentStatus::Idle);
    assert_eq!(AgentStatus::from("idle".to_owned()), AgentStatus::Idle);
    assert_eq!(
        ExecEvictionReason::custom("idle_timeout"),
        ExecEvictionReason::IdleTimeout
    );
    assert_eq!(
        ExecEvictionReason::from("idle_timeout"),
        ExecEvictionReason::IdleTimeout
    );
    assert_eq!(
        ExecYieldReason::custom("wait_timeout"),
        ExecYieldReason::WaitTimeout
    );
    assert_eq!(
        ExecYieldReason::from("wait_timeout"),
        ExecYieldReason::WaitTimeout
    );
}

#[test]
fn test_unknown_label_inside_known_event_keeps_the_whole_envelope() {
    // A newer build's eviction reason must not cost an older reader the six envelope fields
    // around it.
    let payload = json!({
        "schema_version": 1,
        "seq": 51,
        "run_id": "run-label-1",
        "agent_id": "coder",
        "at": 1700000000000_u64,
        "body": {
            "family": "exec",
            "data": {
                "schema_version": 1,
                "kind": "evicted",
                "session_id": "exec-7",
                "reason": "memory_pressure"
            }
        }
    });

    let event: HostEvent =
        serde_json::from_value(payload.clone()).expect("unknown label must not fail the envelope");
    assert_eq!(event.seq(), 51);
    assert_eq!(event.run_id().as_str(), "run-label-1");

    match event.body() {
        HostEventBody::Exec(ExecEvent::Evicted(evicted)) => {
            assert_eq!(
                evicted.reason(),
                &ExecEvictionReason::custom("memory_pressure")
            );
            assert!(
                evicted.unknown().is_empty(),
                "a label is not an unknown field"
            );
        }
        _ => panic!("expected a typed Evicted event, not an Unknown fallback"),
    }

    // Written back verbatim, so a newer build reading it again sees its own variant.
    assert_eq!(
        serde_json::to_value(&event).expect("must serialize"),
        payload
    );
}

#[test]
fn test_unknown_agent_status_keeps_the_whole_envelope() {
    let payload = json!({
        "schema_version": 1,
        "seq": 52,
        "run_id": "run-label-2",
        "agent_id": "orchestrator",
        "at": 1700000000000_u64,
        "body": {
            "family": "agent",
            "data": {
                "schema_version": 1,
                "kind": "status_changed",
                "previous_status": "running",
                "new_status": "compacting"
            }
        }
    });

    let event: HostEvent =
        serde_json::from_value(payload.clone()).expect("unknown status must not fail the envelope");

    match event.body() {
        HostEventBody::Agent(AgentEvent::StatusChanged(changed)) => {
            assert_eq!(changed.previous_status(), &AgentStatus::Running);
            assert_eq!(changed.new_status(), &AgentStatus::custom("compacting"));
        }
        _ => panic!("expected a typed StatusChanged event, not an Unknown fallback"),
    }

    assert_eq!(
        serde_json::to_value(&event).expect("must serialize"),
        payload
    );
}

#[test]
fn test_stream_kind_stays_closed_and_rejects_unknown_names() {
    // The one label in this module that is routed on rather than rendered: an unknown name must
    // fail rather than fall into a catch-all that would misfile output.
    let parsed: Result<ExecStreamKind, _> = serde_json::from_str("\"stdout\"");
    assert_eq!(
        parsed.expect("known name must parse"),
        ExecStreamKind::Stdout
    );

    let rejected: Result<ExecStreamKind, _> = serde_json::from_str("\"tracefd\"");
    assert!(
        rejected.is_err(),
        "an unknown stream kind must be rejected, not guessed"
    );
}
