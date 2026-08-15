//! Resumable run state, identity binding, event sequence allocation, and extension slots.

use std::collections::HashSet;

use ra_core::{
    agent::AgentSpec,
    budget::BudgetSnapshot,
    compat::SchemaVersion,
    context::RunContext,
    finish::FinishReason,
    item::{AgentId, CallId},
    state::{
        GraphCursor, NestedRunRef, PendingControlRequest,
        RUN_STATE_SCHEMA_VERSION, RunId, RunState, ToolUse, ToolUseAttempt, WorkStateRef,
        WorkspaceLeaseRef,
    },
    tool::ToolLookupKey,
    usage::Usage,
};
use serde_json::json;

fn record_call(state: &mut RunState, agent: &AgentId, identity: &ToolUse, call: &str) {
    let attempt = ToolUseAttempt::new(identity.clone(), CallId::new(call), &json!({"path":"a"}));
    state.tool_use_mut().record_turn(agent, [attempt]);
}

#[test]
fn test_run_state_01() {
    let run_id = RunId::new("run-init-test");
    let state = RunState::start(run_id.clone());

    assert_eq!(state.schema_version(), RUN_STATE_SCHEMA_VERSION);
    assert_eq!(state.run_id(), &run_id);
    assert_eq!(state.next_host_event_seq(), 0);
    assert_eq!(state.tool_use().agents().count(), 0);
    assert_eq!(state.tool_failure().agents().count(), 0);
    assert_eq!(state.budget().turns_used(), 0);
    assert_eq!(state.finish_reason(), None);
    assert!(state.nested_runs().is_empty());
    assert!(state.workspace_lease().is_none());
    assert!(state.work_state_ref().is_none());
    assert!(state.graph_cursor().is_none());
    assert!(state.usage_totals().is_none());
    assert!(state.pending_control_requests().is_empty());
    assert!(state.unknown().is_empty());
}

#[test]
fn test_run_state_02() {
    let agent = AgentId::new("coder");
    let identity = ToolUse::Tool(ToolLookupKey::bare("write_file").unwrap());
    let mut budget = BudgetSnapshot::new();
    budget.record_turn();
    budget.record_usage(&Usage::new(3, 4));

    let mut state = RunState::start(RunId::new("run-accounting"))
        .with_budget(budget)
        .with_next_host_event_seq(5);
    record_call(&mut state, &agent, &identity, "call-1");

    let serialized = serde_json::to_string(&state).expect("run state must serialize");
    let restored: RunState = serde_json::from_str(&serialized).expect("run state must deserialize");

    assert_eq!(restored.run_id().as_str(), "run-accounting");
    assert_eq!(restored.next_host_event_seq(), 5);
    assert_eq!(restored.tool_use().repeat_streak(&agent, &identity), 1);
    assert_eq!(restored.budget().turns_used(), 1);
    assert_eq!(restored.budget().tokens_used(), 7);
}

#[test]
fn test_run_state_03() {
    let stored = r#"{
        "schema_version": 7,
        "run_id": "run-future",
        "next_host_event_seq": 12,
        "future_policy": { "enabled": true }
    }"#;
    let state: RunState = serde_json::from_str(stored).expect("newer state must remain readable");

    let agent = AgentId::new("coder");
    let identity = ToolUse::Tool(ToolLookupKey::bare("write_file").unwrap());
    let mut carried = RunState::start(RunId::new("run-carried"));
    record_call(&mut carried, &agent, &identity, "call-1");
    let state = state.with_tool_use(carried.tool_use().clone());

    assert_eq!(state.tool_use().repeat_streak(&agent, &identity), 1);
    assert_eq!(state.schema_version(), SchemaVersion::new(7));
    let written = serde_json::to_value(&state).expect("run state must serialize");
    assert_eq!(written["future_policy"]["enabled"], true);
    assert_eq!(written["run_id"], "run-future");
    assert_eq!(written["next_host_event_seq"], 12);
}

#[test]
fn test_run_state_04() {
    let stored = r#"{
        "schema_version": 7,
        "run_id": "run-future-04",
        "next_host_event_seq": 3,
        "future_policy": { "enabled": true }
    }"#;
    let state: RunState = serde_json::from_str(stored).expect("newer state must remain readable");
    let written = serde_json::to_value(&state).expect("run state must serialize");

    assert_eq!(state.schema_version(), SchemaVersion::new(7));
    assert_eq!(state.run_id().as_str(), "run-future-04");
    assert_eq!(state.next_host_event_seq(), 3);
    assert_eq!(state.tool_use().agents().count(), 0);
    assert_eq!(written["future_policy"]["enabled"], true);
}

#[test]
fn test_run_state_identity_equality() {
    let state1 = RunState::start(RunId::new("run-fixed"));
    let state2 = RunState::start(RunId::new("run-fixed"));
    let state3 = RunState::start(RunId::new("run-different"));

    assert_eq!(state1, state2);
    assert_ne!(state1, state3);
}

#[test]
fn test_run_state_rejects_missing_run_id() {
    let stored_without_id = r#"{
        "schema_version": 1,
        "next_host_event_seq": 0
    }"#;
    let result = serde_json::from_str::<RunState>(stored_without_id);
    assert!(
        result.is_err(),
        "deserialization without run_id must fail rather than minting a fresh identity"
    );
}

#[test]
fn test_run_state_rejects_missing_next_host_event_seq() {
    let stored_without_seq = r#"{
        "schema_version": 1,
        "run_id": "run-x"
    }"#;
    let result = serde_json::from_str::<RunState>(stored_without_seq);
    assert!(
        result.is_err(),
        "deserialization without next_host_event_seq must fail rather than resetting to zero"
    );
}

#[test]
fn test_run_state_extension_slots_roundtrip() {
    let run_id = RunId::new("run-ext-slots");
    let finish_reason = FinishReason::Final;
    let nested_run = NestedRunRef::new("scope-1", CallId::new("call-child"))
        .with_signature("agent-worker-signature");
    let lease = WorkspaceLeaseRef::new("lease-42").with_path("/tmp/workspace");
    let task_ref = WorkStateRef::new("task-root-100");
    let cursor = GraphCursor::new("node-plan").with_edge_id("edge-step-1");
    let usage = Usage::new(100, 50);
    let control_req = PendingControlRequest::new("approval-1")
        .with_call_id(CallId::new("call-exec"))
        .with_description("approve dangerous action");

    let state = RunState::start(run_id.clone())
        .with_next_host_event_seq(8)
        .with_finish_reason(finish_reason)
        .with_nested_runs(vec![nested_run.clone()])
        .with_workspace_lease(lease.clone())
        .with_work_state_ref(task_ref.clone())
        .with_graph_cursor(cursor.clone())
        .with_usage_totals(usage.clone())
        .with_pending_control_requests(vec![control_req.clone()]);

    let serialized = serde_json::to_string(&state).expect("state with all slots must serialize");
    let restored: RunState = serde_json::from_str(&serialized).expect("state must deserialize");

    assert_eq!(restored.run_id(), &run_id);
    assert_eq!(restored.next_host_event_seq(), 8);
    assert_eq!(restored.finish_reason(), Some(finish_reason));
    assert_eq!(restored.nested_runs(), &[nested_run]);
    assert_eq!(restored.workspace_lease(), Some(&lease));
    assert_eq!(restored.work_state_ref(), Some(&task_ref));
    assert_eq!(restored.graph_cursor(), Some(&cursor));
    assert_eq!(restored.usage_totals(), Some(&usage));
    assert_eq!(restored.pending_control_requests(), &[control_req]);
}

#[test]
fn test_event_seq_allocator_concurrent_monotonicity() {
    let run_id = RunId::new("run-seq-test");
    let allocator = RunState::start(run_id.clone())
        .with_next_host_event_seq(100)
        .restore_event_seq_allocator(None);
    let num_threads = 8;
    let allocations_per_thread = 250;

    let mut handles = Vec::new();
    for _ in 0..num_threads {
        let alloc = allocator.clone();
        handles.push(std::thread::spawn(move || {
            let mut seqs = Vec::with_capacity(allocations_per_thread);
            for _ in 0..allocations_per_thread {
                seqs.push(alloc.allocate().expect("allocation must succeed"));
            }
            seqs
        }));
    }

    let mut all_seqs = Vec::new();
    for handle in handles {
        let thread_seqs = handle.join().expect("thread should finish");
        all_seqs.extend(thread_seqs);
    }

    let total_count = num_threads * allocations_per_thread;
    assert_eq!(all_seqs.len(), total_count);

    let unique_set: HashSet<u64> = all_seqs.iter().copied().collect();
    assert_eq!(
        unique_set.len(),
        total_count,
        "concurrent sequence allocation must have no duplicate sequence numbers"
    );

    for seq in &all_seqs {
        assert!(
            *seq >= 100 && *seq < 100 + total_count as u64,
            "allocated sequence {seq} must be within the allocated interval"
        );
    }

    assert_eq!(allocator.current_next(), 100 + total_count as u64);
    assert_eq!(allocator.run_id(), &run_id);
}

#[test]
fn test_event_seq_allocator_exhaustion_rejected() {
    let allocator = RunState::start(RunId::new("run-exhaust"))
        .with_next_host_event_seq(u64::MAX)
        .restore_event_seq_allocator(None);
    let result = allocator.allocate();
    assert!(
        result.is_err(),
        "allocating past u64::MAX must return an error rather than wrapping to zero"
    );
}

#[test]
fn test_event_seq_allocator_restore_exhaustion_rejected() {
    // Restoring against an exhausted log saturates rather than wrapping, and the run stops at the
    // one place that hands out numbers instead of on the resume path.
    let allocator = RunState::start(RunId::new("run-restore-exhaust"))
        .with_next_host_event_seq(10)
        .restore_event_seq_allocator(Some(u64::MAX));

    assert_eq!(allocator.current_next(), u64::MAX);
    assert!(
        allocator.allocate().is_err(),
        "an exhausted run must refuse to issue a number rather than wrap onto its own"
    );
}

#[test]
fn test_event_seq_allocator_restore() {
    let run_id = RunId::new("run-restore");

    // 1. Without persisted log sidecar, fallback to checkpoint_next
    let restored1 = RunState::start(run_id.clone())
        .with_next_host_event_seq(10)
        .restore_event_seq_allocator(None);
    assert_eq!(restored1.current_next(), 10);
    assert_eq!(restored1.allocate().unwrap(), 10);

    // 2. When checkpoint is ahead of persisted max (normal steady state)
    let restored2 = RunState::start(run_id.clone())
        .with_next_host_event_seq(20)
        .restore_event_seq_allocator(Some(15));
    assert_eq!(restored2.current_next(), 20);
    assert_eq!(restored2.allocate().unwrap(), 20);

    // 3. When events were persisted after the last saved checkpoint before restart
    let restored3 = RunState::start(run_id.clone())
        .with_next_host_event_seq(10)
        .restore_event_seq_allocator(Some(18));
    assert_eq!(restored3.current_next(), 19);
    assert_eq!(restored3.allocate().unwrap(), 19);
}

#[test]
fn test_run_state_restore_event_seq_allocator() {
    let mut state = RunState::start(RunId::new("run-alloc-test")).with_next_host_event_seq(15);
    let allocator = state.restore_event_seq_allocator(Some(20));
    assert_eq!(allocator.current_next(), 21);

    let allocated1 = allocator.allocate().unwrap();
    let allocated2 = allocator.allocate().unwrap();
    assert_eq!(allocated1, 21);
    assert_eq!(allocated2, 22);

    state.snapshot_event_seq(&allocator);
    assert_eq!(state.next_host_event_seq(), 23);
}

#[test]
fn test_run_state_snapshot_event_seq_anti_regression_and_run_binding() {
    let mut state = RunState::start(RunId::new("run-bound-1")).with_next_host_event_seq(100);

    // 1. A stale allocator with a lower sequence bound must not regress the state
    let stale_allocator = RunState::start(RunId::new("run-bound-1"))
        .with_next_host_event_seq(20)
        .restore_event_seq_allocator(None);
    state.snapshot_event_seq(&stale_allocator);
    assert_eq!(
        state.next_host_event_seq(),
        100,
        "snapshot must preserve maximum bound and prevent regression"
    );

    // 2. A matching allocator with a higher sequence advances the state
    let advanced_allocator = RunState::start(RunId::new("run-bound-1"))
        .with_next_host_event_seq(150)
        .restore_event_seq_allocator(None);
    state.snapshot_event_seq(&advanced_allocator);
    assert_eq!(state.next_host_event_seq(), 150);
}

#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "event sequence allocator belongs to a different run")]
fn test_run_state_snapshot_event_seq_rejects_a_foreign_allocator() {
    // Skipping the snapshot instead would leave a checkpoint whose bound never advances, and the
    // resume after it would re-issue numbers the run had already written under.
    let mut state = RunState::start(RunId::new("run-bound-1")).with_next_host_event_seq(100);
    let foreign_allocator = RunState::start(RunId::new("run-foreign"))
        .with_next_host_event_seq(500)
        .restore_event_seq_allocator(None);

    state.snapshot_event_seq(&foreign_allocator);
}

#[test]
fn test_run_state_next_event_seq_never_regresses() {
    let state = RunState::start(RunId::new("run-monotonic-bound"))
        .with_next_host_event_seq(100)
        .with_next_host_event_seq(20);

    assert_eq!(state.next_host_event_seq(), 100);
}

#[test]
fn test_pending_control_requests_context_projection() {
    let run_id = RunId::new("run-ctx-test");
    let agent_spec = AgentSpec::builder()
        .id(AgentId::new("tester"))
        .name("Tester")
        .build()
        .expect("agent spec must build");
    let request = PendingControlRequest::new("req-1")
        .with_call_id(CallId::new("call-1"))
        .with_description("approval required");

    let state = RunState::start(run_id.clone())
        .with_pending_control_requests(vec![request.clone()]);

    let context = RunContext::new(run_id.clone(), &agent_spec)
        .with_pending_control_requests(state.pending_control_requests().to_vec());

    assert_eq!(context.pending_control_requests().len(), 1);
    assert_eq!(context.pending_control_requests()[0], request);
    assert_eq!(
        context.pending_control_requests()[0].request_id(),
        "req-1"
    );
    assert_eq!(
        context.pending_control_requests()[0].call_id(),
        Some(&CallId::new("call-1"))
    );
    assert_eq!(
        context.pending_control_requests()[0].description(),
        Some("approval required")
    );
}
