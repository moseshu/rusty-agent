//! Resumable run state, identity binding, event sequence allocation, and extension slots.

use std::collections::HashSet;

use ra_core::{
    agent::AgentSpec,
    budget::{BudgetLimit, BudgetSnapshot},
    compat::SchemaVersion,
    context::RunContext,
    finish::FinishReason,
    item::{
        AgentId, CallId, ItemId, ItemProvenance, Message, ModelInputItem, ModelResponse,
        OutputPhase, RunItem, RunItemKind, ToolApproval,
    },
    permission::PermissionDecision,
    state::{
        GraphCursor, HandoffProjection, NestedRunRef, PendingControlRequest,
        RUN_STATE_SCHEMA_VERSION, RUN_STATE_SCHEMA_VERSION_SUMMARIES, RunId, RunState, ToolUse,
        ToolUseAttempt, WorkStateRef, WorkspaceLeaseRef,
    },
    tool::{ToolLookupKey, ToolNamespace, ToolOrigin},
    usage::{RequestUsage, Usage},
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
    assert_eq!(state.usage_totals().requests(), 0);
    assert_eq!(state.tokens_used(), 0);
    assert!(state.pending_control_requests().is_empty());
    assert!(state.starting_agent().is_none());
    assert!(state.current_agent().is_none());
    assert!(state.original_input().is_empty());
    assert!(state.generated_items().is_empty());
    assert!(state.model_responses().is_empty());
    assert!(state.pending_interruptions().is_empty());
    assert!(state.unknown().is_empty());
}

#[test]
fn test_run_state_02() {
    let agent = AgentId::new("coder");
    let identity = ToolUse::Tool(ToolLookupKey::bare("write_file").unwrap());
    let mut budget = BudgetSnapshot::new();
    budget.record_turn();

    let mut state = RunState::start(RunId::new("run-accounting"))
        .with_budget(budget)
        .with_next_host_event_seq(5);
    state.record_usage(&Usage::from_request(RequestUsage::new(3, 4)));
    record_call(&mut state, &agent, &identity, "call-1");

    let serialized = serde_json::to_string(&state).expect("run state must serialize");
    let restored: RunState = serde_json::from_str(&serialized).expect("run state must deserialize");

    assert_eq!(restored.run_id().as_str(), "run-accounting");
    assert_eq!(restored.next_host_event_seq(), 5);
    assert_eq!(restored.tool_use().repeat_streak(&agent, &identity), 1);
    assert_eq!(restored.budget().turns_used(), 1);
    assert_eq!(restored.tokens_used(), 7);
    assert_eq!(restored.usage_totals().request_usage_entries().len(), 1);
}

#[test]
fn test_run_state_03() {
    let stored = r#"{
        "schema_version": 999,
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
    assert_eq!(state.schema_version(), SchemaVersion::new(999));
    let written = serde_json::to_value(&state).expect("run state must serialize");
    assert_eq!(written["future_policy"]["enabled"], true);
    assert_eq!(written["run_id"], "run-future");
    assert_eq!(written["next_host_event_seq"], 12);
}

#[test]
fn test_run_state_04() {
    let stored = r#"{
        "schema_version": 999,
        "run_id": "run-future-04",
        "next_host_event_seq": 3,
        "future_policy": { "enabled": true }
    }"#;
    let state: RunState = serde_json::from_str(stored).expect("newer state must remain readable");
    let written = serde_json::to_value(&state).expect("run state must serialize");

    assert_eq!(state.schema_version(), SchemaVersion::new(999));
    assert_eq!(state.run_id().as_str(), "run-future-04");
    assert_eq!(state.next_host_event_seq(), 3);
    assert_eq!(state.tool_use().agents().count(), 0);
    assert_eq!(written["future_policy"]["enabled"], true);
}

#[test]
fn test_run_state_identity_equality() {
    fn requires_eq<T: Eq>() {}

    requires_eq::<RunState>();
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
    let usage = Usage::from_request(RequestUsage::new(100, 50));
    let control_req = PendingControlRequest::new("approval-1")
        .with_call_id(CallId::new("call-exec"))
        .with_description("approve dangerous action");

    let sandbox = serde_json::json!({
        "backend_id": "unix_local",
        "current_agent_key": "coder",
        "sessions_by_agent": {},
    });
    let mut state = RunState::start(run_id.clone())
        .with_next_host_event_seq(8)
        .with_finish_reason(finish_reason)
        .with_nested_runs(vec![nested_run.clone()])
        .with_workspace_lease(lease.clone())
        .with_work_state_ref(task_ref.clone())
        .with_graph_cursor(cursor.clone())
        .with_usage_totals(usage.clone())
        .with_pending_control_requests(vec![control_req.clone()]);
    state
        .set_sandbox_resume_state(Some(sandbox.clone()))
        .unwrap();

    let serialized = serde_json::to_string(&state).expect("state with all slots must serialize");
    let restored: RunState = serde_json::from_str(&serialized).expect("state must deserialize");

    assert_eq!(restored.run_id(), &run_id);
    assert_eq!(restored.next_host_event_seq(), 8);
    assert_eq!(restored.finish_reason(), Some(finish_reason));
    assert_eq!(restored.nested_runs(), &[nested_run]);
    assert_eq!(restored.workspace_lease(), Some(&lease));
    assert_eq!(restored.work_state_ref(), Some(&task_ref));
    assert_eq!(restored.graph_cursor(), Some(&cursor));
    assert_eq!(restored.usage_totals(), &usage);
    assert_eq!(restored.pending_control_requests(), &[control_req]);
    assert_eq!(restored.sandbox_resume_state(), Some(&sandbox));
}

/// The checkpoint records the trace group under the reference's `trace` key, exactly as configured,
/// and a later segment replaces it.
#[test]
fn test_run_state_trace_group_roundtrips_and_retains_unread_trace_settings() {
    let mut state = RunState::start(RunId::new("run-trace"));
    assert_eq!(state.trace_group_id(), None);
    assert!(serde_json::to_value(&state).unwrap().get("trace").is_none());
    for group in ["chat-1", "", " \t "] {
        state.set_trace_group_id(Some(group.to_owned()));
        let value = serde_json::to_value(&state).unwrap();
        assert_eq!(value["trace"], json!({"group_id": group}));
        let restored: RunState = serde_json::from_value(value).unwrap();
        assert_eq!(restored, state);
        assert_eq!(restored.trace_group_id(), Some(group));
    }
    state.set_trace_group_id(None);
    assert_eq!(state.trace_group_id(), None);
    assert!(serde_json::to_value(&state).unwrap().get("trace").is_none());

    // A build that ports the reference's other trace settings writes them beside the group.
    let mut value = serde_json::to_value(RunState::start(RunId::new("run-trace"))).unwrap();
    value["trace"] = json!({
        "group_id": "chat-1",
        "id": "trace_123",
        "workflow_name": "Agent workflow",
    });
    let mut restored: RunState = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(restored.trace_group_id(), Some("chat-1"));
    assert_eq!(
        serde_json::to_value(&restored).unwrap()["trace"],
        value["trace"]
    );
    restored.set_trace_group_id(Some("next-segment".to_owned()));
    assert_eq!(
        serde_json::to_value(&restored).unwrap()["trace"],
        json!({"group_id": "next-segment"})
    );
    value["trace"] = json!({"id": "trace_123"});
    let mut restored: RunState = serde_json::from_value(value).unwrap();
    assert_eq!(restored.trace_group_id(), None);
    assert_eq!(
        serde_json::to_value(&restored).unwrap()["trace"],
        json!({"id": "trace_123"})
    );
    restored.set_trace_group_id(None);
    assert!(
        serde_json::to_value(&restored)
            .unwrap()
            .get("trace")
            .is_none()
    );
}

/// Malformed diagnostic trace settings cannot prevent a valid checkpoint from being restored.
#[test]
fn test_run_state_ignores_invalid_trace_settings() {
    for trace in [
        json!(null),
        json!("garbage"),
        json!(5),
        json!(false),
        json!([]),
        json!([{"group_id": "ignored"}]),
        json!({}),
        json!({"group_id": null}),
        json!({"group_id": 5}),
        json!({"group_id": false}),
        json!({"group_id": []}),
        json!({"group_id": {"value": "ignored"}}),
    ] {
        let mut value = serde_json::to_value(RunState::start(RunId::new("run-trace"))).unwrap();
        value["trace"] = trace.clone();
        let restored: RunState = serde_json::from_value(value).unwrap();
        assert_eq!(
            restored,
            RunState::start(RunId::new("run-trace")),
            "{trace}"
        );
        assert!(
            serde_json::to_value(&restored)
                .unwrap()
                .get("trace")
                .is_none()
        );
    }
}

#[test]
fn test_run_state_drops_invalid_trace_group_and_preserves_unknown_settings() {
    let mut value = serde_json::to_value(RunState::start(RunId::new("run-trace"))).unwrap();
    value["trace"] = json!({
        "group_id": 5,
        "id": "trace_123",
        "future_setting": {"enabled": true},
    });
    let restored: RunState = serde_json::from_value(value).unwrap();
    assert_eq!(restored.trace_group_id(), None);
    assert_eq!(
        serde_json::to_value(restored).unwrap()["trace"],
        json!({"id": "trace_123", "future_setting": {"enabled": true}})
    );
}

fn approval_item(id: &str, call: &str) -> RunItem {
    approval_item_for_agent(id, call, "coder")
}

fn approval_item_for_agent(id: &str, call: &str, agent: &str) -> RunItem {
    RunItem::new(
        ItemId::new(id),
        RunItemKind::ToolApproval(
            ToolApproval::new(
                CallId::new(call),
                "exec_command",
                json!({"cmd": "git status"}),
            )
            .with_tool_origin(&ToolOrigin::new("exec_command").unwrap()),
        ),
    )
    .with_provenance(ItemProvenance::new(AgentId::new(agent)))
}

fn commentary_item(id: &str, text: &str) -> RunItem {
    RunItem::new(
        ItemId::new(id),
        RunItemKind::Message(Message::assistant(text, OutputPhase::Commentary)),
    )
}

/// Every wire version this build can read has to be described next to the number, or a future
/// reader has no way to tell whether an older runtime may resume a checkpoint that carries it.
#[test]
fn test_run_state_schema_version_summaries_describe_every_readable_version() {
    assert!(
        RUN_STATE_SCHEMA_VERSION_SUMMARIES
            .iter()
            .any(|(version, _)| *version == RUN_STATE_SCHEMA_VERSION),
        "the current schema version must carry a summary of what it changed"
    );
    for pair in RUN_STATE_SCHEMA_VERSION_SUMMARIES.windows(2) {
        assert!(
            pair[0].0 < pair[1].0,
            "summaries must be listed once each, in ascending version order"
        );
    }
    for (version, summary) in RUN_STATE_SCHEMA_VERSION_SUMMARIES {
        assert!(
            !summary.trim().is_empty(),
            "schema version {version} must say what it changed"
        );
        assert!(*version <= RUN_STATE_SCHEMA_VERSION);
    }
}

#[test]
fn test_run_state_history_checkpoint_roundtrip() {
    let agent = AgentId::new("coder");
    let original_input = vec![ModelInputItem::Message(Message::user(
        "inspect the workspace",
    ))];
    let approval = approval_item("approval-1", "call-1");
    let response = ModelResponse::new(vec![commentary_item("message-1", "I will inspect it")]);

    let mut state = RunState::start(RunId::new("run-history"));
    state
        .begin_segment(agent.clone(), original_input.clone())
        .expect("first segment must initialize the history");
    state.record_generated_items(vec![approval.clone()]);
    state.record_model_response(response.clone());
    state
        .set_pending_interruptions(std::slice::from_ref(&approval))
        .expect("stored approval must be a generated interruption");

    let serialized = serde_json::to_string(&state).expect("history checkpoint must serialize");
    let restored: RunState = serde_json::from_str(&serialized).expect("history must deserialize");

    assert_eq!(restored.starting_agent(), Some(&agent));
    assert_eq!(restored.current_agent(), Some(&agent));
    assert_eq!(restored.original_input(), original_input);
    assert_eq!(restored.generated_items(), std::slice::from_ref(&approval));
    assert_eq!(restored.model_responses(), std::slice::from_ref(&response));
    assert_eq!(restored.last_model_response(), Some(&response));
    assert_eq!(
        restored.pending_interruptions(),
        [ItemId::new("approval-1")]
    );
    assert_eq!(
        restored.pending_interruption_items().collect::<Vec<_>>(),
        [&approval],
        "a named interruption resolves to the authoritative record, not to a second copy"
    );
}

/// The checkpoint names its pending interruptions instead of copying them, so the wire form must
/// hold one copy of each question rather than two that can drift apart.
#[test]
fn test_run_state_stores_pending_interruptions_by_id_only() {
    let approval = approval_item("approval-1", "call-1");
    let mut state = RunState::start(RunId::new("run-interruption-shape"));
    state
        .begin_segment(AgentId::new("coder"), Vec::new())
        .expect("first segment must initialize the history");
    state.record_generated_items(vec![approval.clone()]);
    state
        .set_pending_interruptions(std::slice::from_ref(&approval))
        .expect("stored approval must be a generated interruption");

    let stored = serde_json::to_value(&state).expect("state must serialize");
    assert_eq!(stored["pending_interruptions"], json!(["approval-1"]));
}

#[test]
fn test_run_state_retains_approval_answers_and_exact_always_rules() {
    let approval = approval_item("approval-1", "call-1");
    let mut state = RunState::start(RunId::new("run-approval-answer"));
    state
        .begin_segment(AgentId::new("coder"), Vec::new())
        .expect("first segment must initialize the history");
    state.record_generated_items([approval.clone()]);
    state
        .set_pending_interruptions(std::slice::from_ref(&approval))
        .expect("approval must be pending");
    state
        .approve(&approval, true)
        .expect("answer must be retained");

    assert_eq!(state.pending_interruption_resolutions().len(), 1);
    assert_eq!(state.permission_rules().len(), 1);
    let rule = &state.permission_rules()[0];
    assert_eq!(rule.decision(), PermissionDecision::Allow);
    assert_eq!(rule.tool_name(), Some("exec_command"));
    // Pinned to the executable the host was shown. A name-only rule would extend one click to
    // every namespace that happens to advertise `exec_command`.
    assert_eq!(
        rule.lookup_key(),
        Some(ToolOrigin::new("exec_command").unwrap().lookup_key())
    );
    let elsewhere =
        ToolOrigin::namespaced(ToolNamespace::new("mcp_shell").unwrap(), "exec_command")
            .expect("a same-named tool in another namespace must be constructible");
    assert!(
        !rule.matches_origin(&elsewhere),
        "an approval answer must not cover a tool the host never saw"
    );

    let restored: RunState = serde_json::from_value(serde_json::to_value(&state).unwrap()).unwrap();
    assert_eq!(
        restored.pending_interruption_resolutions(),
        state.pending_interruption_resolutions()
    );
    assert_eq!(restored.permission_rules(), state.permission_rules());
}

#[test]
fn test_sticky_approval_is_scoped_to_its_agent_and_survives_checkpoint() {
    let first = approval_item_for_agent("approval-first", "call-first", "first");
    let second = approval_item_for_agent("approval-second", "call-second", "second");
    let mut state = RunState::start(RunId::new("run-agent-scoped-approval"));
    state
        .begin_segment(AgentId::new("first"), Vec::new())
        .expect("first segment must initialize the history");
    state.record_generated_items([first.clone(), second.clone()]);
    state
        .set_pending_interruptions(&[first.clone(), second.clone()])
        .expect("both approvals must be pending");

    state
        .approve(&first, true)
        .expect("the first agent's sticky answer must be retained");
    assert_eq!(state.permission_rules().len(), 1);
    assert_eq!(
        state.permission_rules()[0].agent_id(),
        Some(&AgentId::new("first"))
    );
    assert!(
        state
            .pending_interruption_resolutions()
            .iter()
            .all(|answer| answer.item_id() != second.id()),
        "a same-named approval owned by another agent must remain unanswered"
    );

    let restored: RunState =
        serde_json::from_value(serde_json::to_value(&state).expect("state must serialize"))
            .expect("state must deserialize");
    assert_eq!(restored.permission_rules(), state.permission_rules());
    let rule = &restored.permission_rules()[0];
    let origin = ToolOrigin::new("exec_command").expect("origin must build");
    assert!(rule.matches_origin_for_agent(&origin, &AgentId::new("first")));
    assert!(!rule.matches_origin_for_agent(&origin, &AgentId::new("second")));
}

#[test]
fn test_legacy_sticky_rules_without_agent_identity_are_dropped_on_restore() {
    let approval = approval_item("approval-legacy-rule", "call-legacy-rule");
    let mut state = state_awaiting("run-legacy-rule", std::slice::from_ref(&approval));
    state
        .approve(&approval, true)
        .expect("the sticky answer must be retained");

    // Schema 14 is the last one written before rules carried an owner.
    for legacy_schema in [12, 14] {
        let mut wire = serde_json::to_value(&state).expect("state must serialize");
        wire["schema_version"] = json!(legacy_schema);
        for rule in wire["permission_rules"].as_array_mut().unwrap() {
            rule.as_object_mut().unwrap().remove("agent_id");
        }

        let restored: RunState = serde_json::from_value(wire).expect("legacy state must load");
        assert!(
            restored.permission_rules().is_empty(),
            "a schema {legacy_schema} sticky decision without an owner must require approval again"
        );
    }
}

/// Ported from the reference's `test_latest_approval_decision_wins_for_call_id` and
/// `test_matching_exact_call_decisions_preserve_sticky_defaults`: a later exact answer decides its
/// own call, and leaves the sticky default for the others; a later sticky answer replaces it.
#[test]
fn test_run_state_exact_re_answer_keeps_the_sticky_default() {
    let approval = approval_item("approval-1", "call-1");
    let mut state = RunState::start(RunId::new("run-approval-re-answer"));
    state
        .begin_segment(AgentId::new("coder"), Vec::new())
        .expect("first segment must initialize the history");
    state.record_generated_items([approval.clone()]);
    state
        .set_pending_interruptions(std::slice::from_ref(&approval))
        .expect("approval must be pending");

    state
        .approve(&approval, true)
        .expect("the first answer must be retained");
    assert_eq!(state.permission_rules().len(), 1);

    // "Always allow", then this call rejected exactly: the call is rejected and every other call
    // of the tool stays on the sticky approval, as the reference keeps both.
    state
        .reject(&approval, false)
        .expect("the corrected answer must replace the first one");
    assert_eq!(state.pending_interruption_resolutions().len(), 1);
    assert!(matches!(
        state.pending_interruption_resolutions()[0].resolution(),
        ra_core::state::InterruptionResolution::Reject { .. }
    ));
    assert_eq!(state.permission_rules().len(), 1);
    assert_eq!(
        state.permission_rules()[0].decision(),
        PermissionDecision::Allow
    );

    // A sticky answer the other way round replaces the default rather than accumulating.
    state
        .reject(&approval, true)
        .expect("an always-deny answer must be retained");
    state
        .approve(&approval, true)
        .expect("an always-allow answer must replace it");
    assert_eq!(state.permission_rules().len(), 1);
    assert_eq!(
        state.permission_rules()[0].decision(),
        PermissionDecision::Allow
    );
}

/// The refusal side of the same contract: an answer is retained, and `always` denies exactly.
#[test]
fn test_run_state_retains_rejection_answers_and_exact_always_deny_rules() {
    let approval = approval_item("approval-1", "call-1");
    let mut state = RunState::start(RunId::new("run-approval-reject"));
    state
        .begin_segment(AgentId::new("coder"), Vec::new())
        .expect("first segment must initialize the history");
    state.record_generated_items([approval.clone()]);
    state
        .set_pending_interruptions(std::slice::from_ref(&approval))
        .expect("approval must be pending");
    state
        .reject(&approval, true)
        .expect("rejection must be retained");

    assert_eq!(state.pending_interruption_resolutions().len(), 1);
    assert_eq!(state.permission_rules().len(), 1);
    let rule = &state.permission_rules()[0];
    assert_eq!(rule.decision(), PermissionDecision::Deny);
    assert_eq!(
        rule.lookup_key(),
        Some(ToolOrigin::new("exec_command").unwrap().lookup_key())
    );

    // A rejected interruption still blocks the resume until the runtime has recorded its refusal.
    assert!(
        state
            .clone()
            .begin_segment(AgentId::new("coder"), Vec::new())
            .is_ok(),
        "an answered interruption must no longer block a resumed segment"
    );

    let restored: RunState = serde_json::from_value(serde_json::to_value(&state).unwrap()).unwrap();
    assert_eq!(
        restored.pending_interruption_resolutions(),
        state.pending_interruption_resolutions()
    );
    assert_eq!(restored.permission_rules(), state.permission_rules());
}

/// An approval that predates stored routing identities cannot be executed on resume, so the
/// failure belongs at the click rather than at the point of no return.
#[test]
fn test_run_state_refuses_to_approve_an_approval_without_a_routing_identity() {
    let legacy = RunItem::new(
        ItemId::new("approval-legacy"),
        RunItemKind::ToolApproval(ToolApproval::new(
            CallId::new("call-1"),
            "exec_command",
            json!({"cmd": "git status"}),
        )),
    )
    .with_provenance(ItemProvenance::new(AgentId::new("coder")));
    let mut state = RunState::start(RunId::new("run-approval-legacy"));
    state
        .begin_segment(AgentId::new("coder"), Vec::new())
        .expect("first segment must initialize the history");
    state.record_generated_items([legacy.clone()]);
    state
        .set_pending_interruptions(std::slice::from_ref(&legacy))
        .expect("approval must be pending");

    let error = state
        .approve(&legacy, false)
        .expect_err("an unroutable approval must be refused before it is retained");
    assert!(error.to_string().contains("routing identity"), "{error}");
    assert!(state.pending_interruption_resolutions().is_empty());

    // Rejecting one is still safe: it needs no tool to execute, and its `always` rule falls back
    // to the name form, which denies more rather than less.
    state
        .reject(&legacy, true)
        .expect("an unroutable approval may still be refused");
    assert_eq!(state.permission_rules().len(), 1);
    assert_eq!(state.permission_rules()[0].lookup_key(), None);
    assert_eq!(
        state.permission_rules()[0].decision(),
        PermissionDecision::Deny
    );
}

#[test]
fn test_run_state_rejects_pending_interruptions_that_name_nothing_answerable() {
    let approval = approval_item("approval-1", "call-1");
    let commentary = commentary_item("message-1", "still working");
    let mut state = RunState::start(RunId::new("run-invalid-interruption"));
    state
        .begin_segment(AgentId::new("coder"), Vec::new())
        .expect("first segment must initialize the history");
    state.record_generated_items(vec![approval.clone(), commentary.clone()]);

    let absent = approval_item("approval-missing", "call-9");
    let error = state
        .set_pending_interruptions(&[absent])
        .expect_err("an interruption the run never generated is unanswerable");
    assert!(error.to_string().contains("absent from generated items"));

    let error = state
        .set_pending_interruptions(&[commentary])
        .expect_err("an ordinary message is not a question the host can answer");
    assert!(error.to_string().contains("is not an interruption"));

    // The same two rules on the way in from a checkpoint. Enforcing them only on the setter would
    // let a hand-edited or older-build record walk around both.
    state
        .set_pending_interruptions(std::slice::from_ref(&approval))
        .expect("stored approval must be valid before the wire form is edited");
    let stored = serde_json::to_value(&state).expect("state must serialize");

    let mut names_nothing = stored.clone();
    names_nothing["pending_interruptions"] = json!(["approval-missing"]);
    assert!(
        serde_json::from_value::<RunState>(names_nothing).is_err(),
        "a checkpoint must not resume waiting on a record it does not hold"
    );

    let mut names_a_message = stored;
    names_a_message["pending_interruptions"] = json!(["message-1"]);
    assert!(
        serde_json::from_value::<RunState>(names_a_message).is_err(),
        "a checkpoint must not resume a plain message as a pending approval"
    );
}

/// History with no agent naming it is the shape that resumes as a *first* segment: the caller's
/// input would replace `original_input` while the records generated against the old input stayed,
/// and the spliced transcript that comes out is untraceable afterwards.
#[test]
fn test_run_state_rejects_history_that_names_no_current_agent() {
    let mut state = RunState::start(RunId::new("run-orphan-history"));
    state
        .begin_segment(
            AgentId::new("coder"),
            vec![ModelInputItem::Message(Message::user("start"))],
        )
        .expect("first segment must initialize the history");
    state.record_generated_items(vec![commentary_item("message-1", "working")]);
    state.record_model_response(ModelResponse::new(Vec::new()));

    let stored = serde_json::to_value(&state).expect("state must serialize");
    for dropped in ["current_agent", "starting_agent"] {
        let mut orphaned = stored.clone();
        orphaned
            .as_object_mut()
            .expect("a run state serializes as an object")
            .remove(dropped);
        assert!(
            serde_json::from_value::<RunState>(orphaned).is_err(),
            "a checkpoint with history must not resume without `{dropped}`"
        );
    }
}

/// A resumed segment may keep a caller-managed continuation base without replacing the opening
/// input stored in its checkpoint.
#[test]
fn test_run_state_allows_a_resumed_segment_that_carries_its_own_input() {
    let agent = AgentId::new("coder");
    let original_input = vec![ModelInputItem::Message(Message::user("start"))];
    let mut state = RunState::start(RunId::new("run-resume-input"));
    state
        .begin_segment(agent.clone(), original_input.clone())
        .expect("first segment must initialize the history");

    let caller_managed_input = vec![ModelInputItem::Message(Message::user("now do X"))];
    state
        .begin_segment(agent.clone(), caller_managed_input)
        .expect("a resumed segment may carry its caller-managed continuation base");
    assert_eq!(
        state.original_input(),
        original_input,
        "a resumed segment must leave the run's opening input alone"
    );

    let error = state
        .begin_segment(agent.clone(), Vec::new())
        .expect_err("a caller-managed continuation makes automatic projection unsafe");
    assert!(error.to_string().contains("cannot project input"));

    let serialized = serde_json::to_value(&state).expect("state must serialize");
    assert_eq!(serialized["input_history_is_complete"], false);
    let mut restored: RunState =
        serde_json::from_value(serialized).expect("state must retain its resume mode");
    let error = restored
        .begin_segment(agent.clone(), Vec::new())
        .expect_err("a restored caller-managed continuation cannot project input either");
    assert!(error.to_string().contains("cannot project input"));

    let error = state
        .begin_segment(AgentId::new("planner"), Vec::new())
        .expect_err("a restored checkpoint must bind back to the agent that owns it");
    assert!(error.to_string().contains("expects current agent `coder`"));
}

/// A transfer of control replaces the base the next request is built from, and survives a
/// checkpoint — otherwise the first turn after a resume re-sends exactly the history the transfer
/// withheld from the agent that took over.
#[test]
fn test_run_state_carries_the_model_input_projection_a_transfer_installed() {
    let agent = AgentId::new("coder");
    let opening = vec![ModelInputItem::Message(Message::user("start"))];
    let mut state = RunState::start(RunId::new("run-handoff-projection"));
    state
        .begin_segment(agent.clone(), opening.clone())
        .expect("first segment must initialize the history");
    state.record_generated_items(vec![
        commentary_item("msg-1", "thinking"),
        commentary_item("msg-2", "transferring"),
    ]);

    // Without a projection the base is the run's own opening input and its whole history.
    let (base, carried) = state.model_input_base().unwrap();
    assert_eq!(base, opening);
    assert_eq!(carried.len(), 2);

    let projection = HandoffProjection::new(
        vec![ModelInputItem::Message(Message::user("the brief"))],
        ItemId::new("msg-2"),
    );
    state
        .install_handoff_projection(projection.clone())
        .expect("the boundary record is part of this run's history");
    state.record_generated_items(vec![commentary_item("msg-3", "after the transfer")]);

    let restored: RunState =
        serde_json::from_value(serde_json::to_value(&state).expect("state must serialize"))
            .expect("the projection must survive a checkpoint");
    assert_eq!(restored.handoff_projection(), Some(&projection));
    let (base, carried) = restored.model_input_base().unwrap();
    assert_eq!(base.len(), 1);
    assert_eq!(ids(carried), ["msg-3"]);
    // The session keeps everything either way; only the model-facing view was narrowed.
    assert_eq!(restored.generated_items().len(), 3);

    // A caller that supplies its own input is answering the same question, so its answer replaces
    // the framework's rather than being layered on top of it.
    let mut resumed = restored;
    resumed
        .begin_segment(agent, vec![ModelInputItem::Message(Message::user("now X"))])
        .expect("a caller-managed continuation may take over the projection");
    assert!(resumed.handoff_projection().is_none());
    let (base, carried) = resumed.model_input_base().unwrap();
    assert_eq!(base, opening);
    assert_eq!(carried.len(), 3);
}

#[test]
fn pending_handoffs_validate_stored_calls_and_migrate_without_widening_old_projections() {
    use ra_core::{item::HandoffCall, state::PendingHandoff};
    let call = HandoffCall::new(
        CallId::new("handoff"),
        AgentId::new("target"),
        json!({"reason": "review"}),
    )
    .with_tool_name("transfer_to_target");
    let item = RunItem::new(ItemId::new("handoff-item"), RunItemKind::HandoffCall(call));
    let mut state = RunState::start(RunId::new("migration"));
    state
        .begin_segment(
            AgentId::new("source"),
            vec![ModelInputItem::Message(Message::user("full input"))],
        )
        .unwrap();
    state.record_generated_items([item.clone()]);
    let projection = HandoffProjection::new(Vec::new(), item.id().clone());
    state
        .install_handoff_projection(projection.clone())
        .unwrap();
    state
        .set_pending_handoff(Some(
            PendingHandoff::new(
                vec![item],
                Vec::new(),
                Vec::new(),
                ItemId::new("handoff-item"),
            )
            .with_declarations(vec![1])
            .with_unpublished_items(vec![ItemId::new("handoff-item")])
            .with_server_managed_conversation(true),
        ))
        .unwrap();
    let stored = serde_json::to_value(&state).unwrap();
    let restored: RunState = serde_json::from_value(stored.clone()).unwrap();
    assert!(
        restored
            .pending_handoff()
            .unwrap()
            .server_manages_conversation()
    );
    assert_eq!(restored.pending_handoff().unwrap().declarations(), [1]);
    assert_eq!(
        restored.pending_handoff().unwrap().unpublished_items(),
        [ItemId::new("handoff-item")]
    );
    for unpublished in [json!(["missing"]), json!(["handoff-item", "handoff-item"])] {
        let mut corrupted = stored.clone();
        corrupted["pending_handoff"]["unpublished_items"] = unpublished;
        assert!(serde_json::from_value::<RunState>(corrupted).is_err());
    }
    let mut earlier = stored.clone();
    earlier["pending_handoff"]
        .as_object_mut()
        .unwrap()
        .remove("unpublished_items");
    let earlier: RunState = serde_json::from_value(earlier).unwrap();
    assert!(
        earlier
            .pending_handoff()
            .unwrap()
            .unpublished_items()
            .is_empty()
    );
    // Resume rebinds by declaration position, so a call without one cannot be restored.
    for declarations in [json!([]), json!([1, 2])] {
        let mut corrupted = stored.clone();
        corrupted["pending_handoff"]["declarations"] = declarations;
        assert!(serde_json::from_value::<RunState>(corrupted).is_err());
    }
    for field in ["call_id", "arguments", "target_agent"] {
        let mut corrupted = stored.clone();
        corrupted["pending_handoff"]["calls"][0]["kind"]["data"][field] = json!("altered");
        assert!(
            serde_json::from_value::<RunState>(corrupted).is_err(),
            "{field}"
        );
    }
    let mut legacy = stored;
    legacy["schema_version"] = json!(10);
    legacy.as_object_mut().unwrap().remove("pending_handoff");
    let legacy: RunState = serde_json::from_value(legacy).unwrap();
    assert_eq!(legacy.schema_version(), RUN_STATE_SCHEMA_VERSION);
    assert!(legacy.pending_handoff().is_none());
    assert_eq!(legacy.handoff_projection(), Some(&projection));
}

/// A projection whose boundary the history no longer holds refuses rather than picking a boundary.
#[test]
fn test_run_state_refuses_a_projection_that_names_no_stored_record() {
    let mut state = RunState::start(RunId::new("run-handoff-boundary"));
    state
        .begin_segment(AgentId::new("coder"), Vec::new())
        .expect("first segment must initialize the history");
    state.record_generated_items(vec![commentary_item("msg-1", "thinking")]);

    let error = state
        .install_handoff_projection(HandoffProjection::new(Vec::new(), ItemId::new("msg-404")))
        .expect_err("a boundary this run never generated cannot be accepted");
    assert!(error.to_string().contains("msg-404"));
}

/// A filter can remove every record of a run's first turn; the projection then precedes all
/// stored history instead of naming a boundary record.
#[test]
fn a_projection_from_the_start_precedes_every_stored_record() {
    let mut state = RunState::start(RunId::new("run-from-start"));
    state
        .begin_segment(
            AgentId::new("coder"),
            vec![ModelInputItem::Message(Message::user("full input"))],
        )
        .unwrap();
    let projection = HandoffProjection::from_start(
        vec![ModelInputItem::Message(Message::user("kept input"))],
        ItemId::new("removed-transfer"),
    );
    state
        .install_handoff_projection(projection.clone())
        .unwrap();
    state.record_generated_items(vec![commentary_item("msg-1", "after the transfer")]);

    let restored: RunState = serde_json::from_value(serde_json::to_value(&state).unwrap()).unwrap();
    assert!(restored.handoff_projection().unwrap().is_from_start());
    let (input, carried) = restored.model_input_base().unwrap();
    assert_eq!(input, projection.input());
    assert_eq!(ids(carried), ["msg-1"]);

    // Once anything is stored, a projection has to name its boundary.
    assert!(
        state
            .install_handoff_projection(HandoffProjection::from_start(
                Vec::new(),
                ItemId::new("removed-transfer"),
            ))
            .is_err()
    );
}

/// A handoff filter may replace only records the session has not received, and only in a way
/// that keeps every record the run still refers to.
#[test]
fn unpersisted_records_can_be_replaced_without_dropping_referenced_ones() {
    let mut state = RunState::start(RunId::new("run-replace"));
    state
        .begin_segment(AgentId::new("coder"), Vec::new())
        .unwrap();
    state.record_generated_items(vec![
        commentary_item("stored", "already in the session"),
        commentary_item("unsent", "not yet in the session"),
    ]);
    state.mark_session_persisted(1).unwrap();

    assert!(
        state.replace_unpersisted_records(0, Vec::new()).is_err(),
        "a record the session holds cannot be removed"
    );
    assert!(
        state
            .replace_unpersisted_records(
                1,
                vec![commentary_item("twice", "a"), commentary_item("twice", "b"),],
            )
            .is_err(),
        "the replacement cannot store one record twice"
    );
    state
        .replace_unpersisted_records(1, vec![commentary_item("rewritten", "kept by the filter")])
        .unwrap();
    assert_eq!(ids(state.generated_items()), ["stored", "rewritten"]);

    let approval = approval_item("approval-1", "call-1");
    state.push_pending_interruption(approval).unwrap();
    assert!(
        state.replace_unpersisted_records(1, Vec::new()).is_err(),
        "a pending question must stay in history"
    );
}

fn ids(items: &[RunItem]) -> Vec<&str> {
    items.iter().map(|item| item.id().as_str()).collect()
}

/// Entering a segment preserves unanswered questions for the runner to return.
#[test]
fn test_run_state_preserves_unanswered_interruptions_across_segments() {
    let agent = AgentId::new("coder");
    let approval = approval_item("approval-1", "call-1");
    let mut state = RunState::start(RunId::new("run-blocked"));
    state
        .begin_segment(agent.clone(), Vec::new())
        .expect("first segment must initialize the history");
    state.record_generated_items(vec![approval.clone()]);
    state
        .set_pending_interruptions(std::slice::from_ref(&approval))
        .expect("stored approval must be a generated interruption");

    state
        .begin_segment(agent.clone(), Vec::new())
        .expect("a segment can settle partial answers before returning remaining approvals");
    assert_eq!(
        state
            .pending_interruption_items()
            .cloned()
            .collect::<Vec<_>>(),
        vec![approval]
    );

    state
        .set_pending_interruptions(&[])
        .expect("answering every question clears the block");
    state
        .begin_segment(agent, Vec::new())
        .expect("an answered run resumes");
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
fn an_allocator_advanced_past_another_shares_nothing_already_issued_and_never_moves_back() {
    let earlier = RunState::start(RunId::new("run-joined"))
        .with_next_host_event_seq(3)
        .restore_event_seq_allocator(None);
    let restored = RunState::start(RunId::new("run-joined"))
        .with_next_host_event_seq(1)
        .restore_event_seq_allocator(Some(9));

    earlier.advance_past(&restored);
    assert_eq!(earlier.current_next(), 10);
    // A clone draws from the same counter, so the two hand out one sequence between them.
    let shared = earlier.clone();
    assert_eq!(shared.allocate().unwrap(), 10);
    assert_eq!(earlier.allocate().unwrap(), 11);

    let behind = RunState::start(RunId::new("run-joined")).restore_event_seq_allocator(None);
    earlier.advance_past(&behind);
    assert_eq!(earlier.current_next(), 12);
}

#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "event sequence allocators of different runs")]
fn an_allocator_is_not_advanced_past_another_runs() {
    let mine = RunState::start(RunId::new("run-mine")).restore_event_seq_allocator(None);
    let theirs = RunState::start(RunId::new("run-theirs"))
        .with_next_host_event_seq(50)
        .restore_event_seq_allocator(None);
    mine.advance_past(&theirs);
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

    let state =
        RunState::start(run_id.clone()).with_pending_control_requests(vec![request.clone()]);

    let context = RunContext::new(run_id.clone(), &agent_spec)
        .with_pending_control_requests(state.pending_control_requests().to_vec());

    assert_eq!(context.pending_control_requests().len(), 1);
    assert_eq!(context.pending_control_requests()[0], request);
    assert_eq!(context.pending_control_requests()[0].request_id(), "req-1");
    assert_eq!(
        context.pending_control_requests()[0].call_id(),
        Some(&CallId::new("call-1"))
    );
    assert_eq!(
        context.pending_control_requests()[0].description(),
        Some("approval required")
    );
}

/// The ledger grows only through the settlement entry point, and it keeps itemizing across a
/// checkpoint: a resumed run adds to what earlier segments spent instead of starting over.
#[test]
fn test_usage_ledger_accumulates_and_survives_a_checkpoint() {
    let mut state = RunState::start(RunId::new("run-ledger"));
    state.record_usage(&Usage::from_request(
        RequestUsage::new(1_000, 100).with_cached_input_tokens(0),
    ));
    state.record_usage(&Usage::from_request(
        RequestUsage::new(1_200, 80).with_cached_input_tokens(1_000),
    ));

    assert_eq!(state.usage_totals().requests(), 2);
    assert_eq!(state.tokens_used(), 2_380);

    let serialized = serde_json::to_string(&state).expect("state must serialize");
    let mut restored: RunState = serde_json::from_str(&serialized).expect("state must deserialize");
    assert_eq!(restored.usage_totals(), state.usage_totals());

    restored.record_usage(&Usage::from_request(
        RequestUsage::new(1_300, 90).with_cached_input_tokens(1_200),
    ));

    assert_eq!(restored.usage_totals().requests(), 3);
    assert_eq!(restored.tokens_used(), 3_770);
    let cached: Vec<u64> = restored
        .usage_totals()
        .request_usage_entries()
        .iter()
        .map(RequestUsage::cached_input_tokens)
        .collect();
    assert_eq!(cached, vec![0, 1_000, 1_200]);
}

/// A stage reads what the run has spent through its context, which is a copy: writing through it
/// is not possible, and the state stays the only thing that accumulates.
#[test]
fn test_usage_ledger_projects_into_a_read_only_context() {
    let run_id = RunId::new("run-ledger-ctx");
    let agent_spec = AgentSpec::builder()
        .id(AgentId::new("tester"))
        .name("Tester")
        .build()
        .expect("agent spec must build");
    let mut state = RunState::start(run_id.clone());
    state.record_usage(&Usage::from_request(
        RequestUsage::new(500, 50).with_reasoning_tokens(20),
    ));

    let context = RunContext::new(run_id, &agent_spec)
        .with_usage_totals(state.usage_totals().clone())
        .with_budget(state.budget().clone());

    assert_eq!(context.usage_totals(), state.usage_totals());
    assert_eq!(context.usage_totals().requests(), 1);
    assert_eq!(context.usage_totals().reasoning_tokens(), 20);
}

/// A checkpoint written while the budget still counted tokens itself must not hand the run that
/// resumes from it a fresh allowance. The spend moves into the ledger on the way in, and it is not
/// re-counted when that state is written out and read back.
#[test]
fn test_run_state_migrates_pre_ledger_token_spend_into_the_usage_ledger() {
    let legacy = r#"{
        "schema_version": 1,
        "run_id": "run-legacy",
        "next_host_event_seq": 4,
        "budget": { "schema_version": 1, "turns_used": 3, "tokens_used": 12000 }
    }"#;

    let restored: RunState = serde_json::from_str(legacy).expect("legacy state must deserialize");

    assert_eq!(restored.budget().turns_used(), 3);
    assert_eq!(restored.tokens_used(), 12_000);
    assert_eq!(restored.usage_totals().carried_total_tokens(), 12_000);
    assert_eq!(restored.usage_totals().requests(), 0);
    assert_eq!(
        restored.remaining_tokens(&BudgetLimit::new().with_max_tokens(20_000)),
        Some(8_000)
    );

    // Written back, the spend is stated by the ledger and the old field is gone — so a second pass
    // through this migration cannot add it again.
    let rewritten = serde_json::to_value(&restored).expect("state must serialize");
    assert!(rewritten["budget"].get("tokens_used").is_none());
    assert_eq!(rewritten["usage_totals"]["carried_total_tokens"], 12_000);

    let round_tripped: RunState =
        serde_json::from_value(rewritten).expect("state must deserialize again");
    assert_eq!(round_tripped.tokens_used(), 12_000);
    assert_eq!(round_tripped, restored);

    // Calls made after the resume add to the carried spend rather than replacing it.
    let mut continued = round_tripped;
    continued.record_usage(&Usage::from_request(RequestUsage::new(500, 100)));
    assert_eq!(continued.tokens_used(), 12_600);
    assert_eq!(continued.usage_totals().requests(), 1);
    assert_eq!(continued.usage_totals().input_tokens(), 500);
}

// -------------------------------------------------------------------------------------------------
// Agent-tool runs paused on an approval
// -------------------------------------------------------------------------------------------------

fn paused_child(parent: &str, approval: &RunItem) -> RunState {
    let mut child = RunState::start(RunId::new("run-child"));
    child
        .assign_parent_run_id(RunId::new(parent))
        .expect("a fresh child accepts its parent");
    child
        .begin_segment(AgentId::new("nested"), Vec::new())
        .expect("first segment must initialize the history");
    child.record_generated_items([approval.clone()]);
    child
        .set_pending_interruptions(std::slice::from_ref(approval))
        .expect("stored approval must be a generated interruption");
    child
}

fn paused_ref(parent: &str, approval: &RunItem) -> NestedRunRef {
    NestedRunRef::interrupted(
        parent,
        &ToolOrigin::new("nested").unwrap(),
        CallId::new("outer-1"),
        json!({"input": "x"}),
        paused_child(parent, approval),
    )
}

fn parent_state() -> RunState {
    let mut parent = RunState::start(RunId::new("run-parent"));
    parent
        .begin_segment(AgentId::new("orchestrator"), Vec::new())
        .expect("first segment must initialize the history");
    parent
}

#[test]
fn test_nested_approvals_are_asked_answered_and_checkpointed_through_the_parent() {
    let approval = approval_item("n-approval", "inner-1");
    let mut parent = parent_state();
    parent
        .set_nested_runs(vec![paused_ref("run-parent", &approval)])
        .expect("a paused child of this run is recorded");

    assert!(parent.pending_interruptions().is_empty());
    assert_eq!(
        parent.pending_interruption_items().collect::<Vec<_>>(),
        [&approval],
        "the nested run's question is asked through the parent"
    );
    assert!(parent.has_unanswered_interruptions());
    parent
        .clone()
        .begin_segment(AgentId::new("orchestrator"), Vec::new())
        .expect("a nested question does not block the resume; the runner asks it again");

    parent
        .approve(&approval, true)
        .expect("the answer is routed to the nested run");
    assert!(parent.pending_interruption_resolutions().is_empty());
    assert!(!parent.has_unanswered_interruptions());
    assert!(
        parent.permission_rules().is_empty(),
        "an always answer is a rule of the nested run only"
    );
    let child = parent.nested_runs()[0].state().expect("checkpoint is kept");
    assert_eq!(child.pending_interruption_resolutions().len(), 1);
    assert_eq!(child.permission_rules().len(), 1);

    let restored: RunState =
        serde_json::from_str(&serde_json::to_string(&parent).expect("state must serialize"))
            .expect("a paused child round-trips with its parent");
    assert_eq!(restored, parent);
    assert_eq!(
        restored.nested_runs()[0].lookup_key(),
        Some(ToolOrigin::new("nested").unwrap().lookup_key())
    );
    assert_eq!(
        restored.nested_runs()[0].arguments(),
        &json!({"input": "x"})
    );
    restored
        .clone()
        .begin_segment(AgentId::new("orchestrator"), Vec::new())
        .expect("an answered nested approval lets the parent resume");
}

#[test]
fn test_a_paused_child_of_another_run_is_refused() {
    let approval = approval_item("n-approval", "inner-1");
    let mut parent = parent_state();
    let error = parent
        .set_nested_runs(vec![paused_ref("someone-else", &approval)])
        .expect_err("a child of another run cannot be resumed from this one");
    assert!(error.to_string().contains("another parent"), "{error}");

    parent
        .set_nested_runs(vec![paused_ref("run-parent", &approval)])
        .expect("a paused child of this run is recorded");
    let mut stored = serde_json::to_value(&parent).expect("state must serialize");
    stored["nested_runs"][0]["state"]["parent_run_id"] = json!("someone-else");
    let error = serde_json::from_value::<RunState>(stored)
        .expect_err("a checkpoint whose child names another parent is refused");
    assert!(error.to_string().contains("another parent"), "{error}");

    let error = parent
        .clone()
        .set_nested_runs(vec![
            paused_ref("run-parent", &approval),
            paused_ref("run-parent", &approval),
        ])
        .expect_err("one call cannot have two paused runs");
    assert!(error.to_string().contains("two paused"), "{error}");
}

#[test]
fn test_a_shared_approval_identity_is_routed_by_record_or_refused() {
    let own = approval_item("approval-1", "call-1");
    let nested = RunItem::new(
        ItemId::new("approval-1"),
        RunItemKind::ToolApproval(
            ToolApproval::new(CallId::new("call-1"), "exec_command", json!({"cmd": "ls"}))
                .with_tool_origin(&ToolOrigin::new("exec_command").unwrap()),
        ),
    );
    let mut parent = parent_state();
    parent.record_generated_items([own.clone()]);
    parent
        .set_pending_interruptions(std::slice::from_ref(&own))
        .expect("own approval is pending");
    parent
        .set_nested_runs(vec![paused_ref("run-parent", &nested)])
        .expect("a paused child of this run is recorded");

    parent
        .reject(&nested, false)
        .expect("the record the host holds tells the two apart");
    assert!(parent.pending_interruption_resolutions().is_empty());
    assert_eq!(
        parent.nested_runs()[0]
            .state()
            .unwrap()
            .pending_interruption_resolutions()
            .len(),
        1
    );
    parent
        .approve(&own, false)
        .expect("and the parent's own record");
    assert_eq!(parent.pending_interruption_resolutions().len(), 1);

    let mut ambiguous = parent_state();
    ambiguous.record_generated_items([own.clone()]);
    ambiguous
        .set_pending_interruptions(std::slice::from_ref(&own))
        .expect("own approval is pending");
    ambiguous
        .set_nested_runs(vec![paused_ref("run-parent", &own)])
        .expect("a paused child of this run is recorded");
    let error = ambiguous
        .approve(&own, false)
        .expect_err("two identical pending records cannot be told apart");
    assert!(error.to_string().contains("unique call IDs"), "{error}");
}

/// What a run owes its session is one index into its records, carried by the checkpoint. A run
/// never bound to a session carries none and writes nothing for it, and one bound only when it
/// resumes owes nothing it generated before.
#[test]
fn test_run_state_session_persistence_count_binds_advances_and_roundtrips() {
    let mut state = RunState::start(RunId::new("run-session-count"));
    state
        .begin_segment(AgentId::new("coder"), Vec::new())
        .expect("first segment must initialize the history");
    state.record_generated_items([commentary_item("message-1", "looking")]);
    assert_eq!(state.session_persisted_item_count(), None);
    let unbound = serde_json::to_value(&state).expect("state must serialize");
    assert!(unbound.get("session_persisted_item_count").is_none());

    state.bind_session_persistence();
    assert_eq!(
        state.session_persisted_item_count(),
        Some(1),
        "a run bound late owes its session nothing it generated before"
    );
    state.bind_session_persistence();
    assert_eq!(state.session_persisted_item_count(), Some(1));

    state.record_generated_items([commentary_item("message-2", "found it")]);
    state
        .mark_session_persisted(2)
        .expect("the count may move to the end of the history");
    assert!(
        state.mark_session_persisted(1).is_err(),
        "a count moving backwards would append records twice"
    );
    assert!(
        state.mark_session_persisted(3).is_err(),
        "a count past the end would skip records the session never received"
    );

    let restored: RunState =
        serde_json::from_value(serde_json::to_value(&state).expect("state must serialize"))
            .expect("state must deserialize");
    assert_eq!(restored.session_persisted_item_count(), Some(2));
}

#[test]
fn test_run_state_rejects_session_count_past_its_history() {
    let stored = json!({
        "run_id": "run-session-overrun",
        "next_host_event_seq": 0,
        "starting_agent": "coder",
        "current_agent": "coder",
        "generated_items": [serde_json::to_value(commentary_item("message-1", "hi")).unwrap()],
        "session_persisted_item_count": 2
    });
    let error = serde_json::from_value::<RunState>(stored)
        .expect_err("a checkpoint claiming more session records than it holds must be refused");
    assert!(error.to_string().contains("session was given 2 records"));
}

#[test]
fn pending_session_writes_and_terminal_failures_survive_a_checkpoint() {
    use ra_core::{item::InputItemDigest, session::SessionId, state::PendingSessionWrite};
    let mut state = RunState::start(RunId::new("pending-write"));
    state
        .begin_segment(AgentId::new("coder"), Vec::new())
        .unwrap();
    state.bind_session_persistence();
    let item = commentary_item("m1", "done");
    state.record_generated_items([item.clone()]);
    let mut pending = PendingSessionWrite::new(SessionId::new("session"), vec![item.clone()], 1);
    pending.set_before(vec![InputItemDigest::compute_session_item(&item).unwrap()]);
    state.begin_session_write(pending).unwrap();
    state.mark_terminal_unrecoverable();
    let restored: RunState = serde_json::from_value(serde_json::to_value(&state).unwrap()).unwrap();
    assert_eq!(restored, state);
    assert!(restored.terminal_unrecoverable());
    assert!(restored.pending_session_write().is_some());
    let mut malformed = serde_json::to_value(&state).unwrap();
    malformed["pending_session_write"]["persisted_count"] = json!(2);
    assert!(serde_json::from_value::<RunState>(malformed).is_err());
    let mut malformed = serde_json::to_value(&state).unwrap();
    malformed["pending_session_write"]["before"] = json!(["invalid digest"]);
    assert!(serde_json::from_value::<RunState>(malformed).is_err());
    let mut malformed = serde_json::to_value(&state).unwrap();
    malformed["pending_session_write"]["items"] = json!([]);
    assert!(serde_json::from_value::<RunState>(malformed).is_err());
}

#[test]
fn session_compaction_exchange_and_append_acknowledgement_survive_checkpoint_without_generation() {
    use ra_core::{
        item::InputItemDigest,
        session::{SessionCompactionContext, SessionId},
        state::PendingSessionWrite,
    };
    let mut state = RunState::start(RunId::new("compaction-checkpoint"));
    state
        .begin_segment(AgentId::new("worker"), Vec::new())
        .unwrap();
    state.bind_session_persistence();
    let item = commentary_item("message", "done");
    state.record_generated_items(vec![item.clone()]);
    let digest = InputItemDigest::compute(&item.to_model_input().unwrap()).unwrap();
    state.set_session_compaction(SessionCompactionContext::new(
        Some(3),
        vec![digest.clone()],
        Some("resp-final".into()),
        Some(false),
    ));
    let mut pending = PendingSessionWrite::new(SessionId::new("session"), vec![item], 1);
    pending.set_before(vec![digest]);
    pending.acknowledge_append();
    state.begin_session_write(pending).unwrap();
    let wire = serde_json::to_value(&state).unwrap();
    assert!(wire["session_compaction"].get("generation").is_none());
    let restored: RunState = serde_json::from_value(wire.clone()).unwrap();
    // Exchange evidence survives; wrapper ownership is process-local and must be re-established.
    let restored_context = restored.session_compaction().unwrap();
    assert_eq!(restored_context.generation(), None);
    let mut expected = state.session_compaction().unwrap().clone();
    expected.set_generation(None);
    assert_eq!(restored_context, &expected);
    assert!(
        restored
            .pending_session_write()
            .unwrap()
            .append_acknowledged()
    );
    let mut invalid = wire.clone();
    invalid["session_compaction"]["model_exchange"] = serde_json::json!(["not-a-digest"]);
    assert!(serde_json::from_value::<RunState>(invalid).is_err());
    let mut invalid = wire;
    invalid["pending_session_write"]["before"] = serde_json::Value::Null;
    assert!(serde_json::from_value::<RunState>(invalid).is_err());
}

fn state_awaiting(run: &str, approvals: &[RunItem]) -> RunState {
    let mut state = RunState::start(RunId::new(run));
    state
        .begin_segment(AgentId::new("coder"), Vec::new())
        .expect("first segment must initialize the history");
    state.record_generated_items(approvals.iter().cloned());
    state
        .set_pending_interruptions(approvals)
        .expect("approvals must be pending");
    state
}

fn answer_message<'a>(state: &'a RunState, item: &RunItem) -> Option<&'a str> {
    state
        .pending_interruption_resolutions()
        .iter()
        .find(|answer| answer.item_id() == item.id())
        .and_then(|answer| answer.rejection_message())
}

/// Ported from the reference's `test_reject_stores_rejection_message` and
/// `test_run_context_stores_per_call_rejection_messages`: the text belongs to the call it answered.
#[test]
fn test_reject_with_message_stores_the_text_for_that_call_only() {
    let first = approval_item("approval-1", "call-1");
    let second = approval_item("approval-2", "call-2");
    let mut state = state_awaiting("run-reject-message", &[first.clone(), second.clone()]);

    state
        .reject_with_message(&first, false, "Denied by reviewer")
        .expect("a rejection with a message must be retained");
    state
        .reject(&second, false)
        .expect("a plain rejection must be retained");

    assert_eq!(answer_message(&state, &first), Some("Denied by reviewer"));
    assert_eq!(answer_message(&state, &second), None);
    assert!(
        state.permission_rules().is_empty(),
        "a one-off rejection leaves no rule behind"
    );
}

/// Ported from the reference's `test_always_reject_reuses_rejection_message_for_future_calls` and
/// `test_run_context_stores_sticky_rejection_messages_for_always_reject`: an empty string is a
/// message, and `always` keeps it for the calls its rule refuses later.
#[test]
fn test_always_reject_keeps_an_empty_message_for_later_calls() {
    let approval = approval_item("approval-1", "call-1");
    let mut state = state_awaiting("run-reject-sticky", std::slice::from_ref(&approval));

    state
        .reject_with_message(&approval, true, "")
        .expect("an always rejection with a message must be retained");

    assert_eq!(answer_message(&state, &approval), Some(""));
    assert_eq!(state.permission_rules().len(), 1);
    let rule = &state.permission_rules()[0];
    assert_eq!(rule.decision(), PermissionDecision::Deny);
    assert_eq!(rule.rejection_message(), Some(""));
}

/// Ported from the reference's `test_run_context_clears_rejection_message_after_approval`, with
/// the sticky side: approving the same record afterwards withdraws the text and its rule.
#[test]
fn test_a_later_approval_clears_the_rejection_message() {
    let approval = approval_item("approval-1", "call-1");
    let mut state = state_awaiting("run-reject-then-approve", std::slice::from_ref(&approval));

    state
        .reject_with_message(&approval, false, "Denied by policy")
        .expect("the rejection must be retained");
    state
        .approve(&approval, false)
        .expect("the approval must replace it");
    assert_eq!(answer_message(&state, &approval), None);

    state
        .reject_with_message(&approval, true, "Denied by policy")
        .expect("an always rejection must be retained");
    state
        .approve(&approval, true)
        .expect("an always approval must replace it");
    assert_eq!(answer_message(&state, &approval), None);
    assert_eq!(state.permission_rules().len(), 1);
    assert_eq!(
        state.permission_rules()[0].decision(),
        PermissionDecision::Allow
    );
    assert_eq!(state.permission_rules()[0].rejection_message(), None);

    // A plain rejection after one with text replaces the text too.
    state
        .reject_with_message(&approval, false, "Denied by policy")
        .expect("the rejection must be retained");
    state
        .reject(&approval, false)
        .expect("a plain rejection must replace it");
    assert_eq!(answer_message(&state, &approval), None);
}

/// Ported from the reference's `test_serializes_and_restores_rejection_messages`.
#[test]
fn test_rejection_messages_survive_a_checkpoint() {
    let approval = approval_item("approval-1", "call-1");
    let mut state = state_awaiting("run-reject-roundtrip", std::slice::from_ref(&approval));
    state
        .reject_with_message(&approval, true, "Denied by reviewer")
        .expect("the rejection must be retained");

    let restored: RunState = serde_json::from_value(serde_json::to_value(&state).unwrap()).unwrap();

    assert_eq!(
        answer_message(&restored, &approval),
        Some("Denied by reviewer")
    );
    assert_eq!(
        restored.permission_rules()[0].rejection_message(),
        Some("Denied by reviewer")
    );
}

/// Ported from the reference's
/// `test_from_json_accepts_previous_schema_version_without_rejection_messages`.
#[test]
fn test_a_previous_checkpoint_without_rejection_messages_still_loads() {
    let approval = approval_item("approval-1", "call-1");
    let mut state = state_awaiting("run-reject-legacy", std::slice::from_ref(&approval));
    state
        .reject_with_message(&approval, true, "Denied by reviewer")
        .expect("the rejection must be retained");

    let mut json = serde_json::to_value(&state).unwrap();
    json["schema_version"] = json!(8);
    for answer in json["pending_interruption_resolutions"]
        .as_array_mut()
        .unwrap()
    {
        answer.as_object_mut().unwrap().remove("rejection_message");
    }
    for rule in json["permission_rules"].as_array_mut().unwrap() {
        rule.as_object_mut().unwrap().remove("rejection_message");
    }

    let restored: RunState = serde_json::from_value(json).unwrap();

    assert_eq!(restored.schema_version(), RUN_STATE_SCHEMA_VERSION);
    assert_eq!(restored.pending_interruption_resolutions().len(), 1);
    assert_eq!(answer_message(&restored, &approval), None);
    assert_eq!(
        restored.permission_rules()[0].decision(),
        PermissionDecision::Deny
    );
    assert_eq!(restored.permission_rules()[0].rejection_message(), None);
}

/// The message travels with the answer to the paused agent-tool run that raised the approval, as
/// the reference forwards `rejection_message` to the nested state.
#[test]
fn test_a_nested_rejection_message_is_kept_by_the_nested_run() {
    let approval = approval_item("n-approval", "inner-1");
    let mut parent = parent_state();
    parent
        .set_nested_runs(vec![paused_ref("run-parent", &approval)])
        .expect("a paused child of this run is recorded");

    parent
        .reject_with_message(&approval, false, "exact denial")
        .expect("the nested approval is answered through the parent");

    assert!(parent.pending_interruption_resolutions().is_empty());
    let nested = parent.nested_runs()[0].state().unwrap();
    assert_eq!(answer_message(nested, &approval), Some("exact denial"));
}

fn resolution_of(
    state: &RunState,
    item: &RunItem,
) -> Option<ra_core::state::InterruptionResolution> {
    state
        .pending_interruption_resolutions()
        .iter()
        .find(|answer| answer.item_id() == item.id())
        .map(ra_core::state::PendingInterruptionResolution::resolution)
}

fn is_rejected(state: &RunState, item: &RunItem) -> bool {
    matches!(
        resolution_of(state, item),
        Some(ra_core::state::InterruptionResolution::Reject { .. })
    )
}

fn is_approved(state: &RunState, item: &RunItem) -> bool {
    matches!(
        resolution_of(state, item),
        Some(ra_core::state::InterruptionResolution::Approve { .. })
    )
}

fn sticky(state: &RunState) -> (PermissionDecision, Option<&str>) {
    assert_eq!(
        state.permission_rules().len(),
        1,
        "one sticky default per action"
    );
    let rule = &state.permission_rules()[0];
    (rule.decision(), rule.rejection_message())
}

/// Ported from the reference's `test_exact_call_decisions_override_sticky_defaults`.
#[test]
fn test_exact_call_decisions_override_sticky_defaults() {
    let sticky_call = approval_item("approval-sticky", "approve-sticky");
    let exception = approval_item("approval-exception", "approve-exception");
    let mut approved = state_awaiting(
        "run-sticky-approve",
        &[sticky_call.clone(), exception.clone()],
    );
    approved.approve(&sticky_call, true).unwrap();
    approved
        .reject_with_message(&exception, false, "denied by user")
        .unwrap();
    assert!(is_rejected(&approved, &exception));
    assert_eq!(
        approved.rejection_message_for(exception.id()),
        Some("denied by user")
    );
    assert_eq!(sticky(&approved), (PermissionDecision::Allow, None));

    let sticky_call = approval_item("approval-sticky", "reject-sticky");
    let exception = approval_item("approval-exception", "reject-exception");
    let mut rejected = state_awaiting(
        "run-sticky-reject",
        &[sticky_call.clone(), exception.clone()],
    );
    rejected
        .reject_with_message(&sticky_call, true, "denied by default")
        .unwrap();
    rejected.approve(&exception, false).unwrap();
    assert!(is_approved(&rejected, &exception));
    assert_eq!(rejected.rejection_message_for(exception.id()), None);
    assert_eq!(
        sticky(&rejected),
        (PermissionDecision::Deny, Some("denied by default"))
    );
}

/// Ported from the reference's `test_exact_call_reversals_keep_other_calls_on_sticky_default`.
#[test]
fn test_exact_call_reversals_keep_other_calls_on_sticky_default() {
    let sticky_call = approval_item("approval-sticky", "approve-sticky");
    let exception = approval_item("approval-exception", "approve-exception");
    let mut approved = state_awaiting(
        "run-reversal-approve",
        &[sticky_call.clone(), exception.clone()],
    );
    approved.approve(&sticky_call, true).unwrap();
    approved
        .reject_with_message(&exception, false, "denied")
        .unwrap();
    approved.approve(&exception, false).unwrap();
    assert!(is_approved(&approved, &exception));
    assert_eq!(approved.rejection_message_for(exception.id()), None);
    assert_eq!(sticky(&approved), (PermissionDecision::Allow, None));

    let sticky_call = approval_item("approval-sticky", "reject-sticky");
    let exception = approval_item("approval-exception", "reject-exception");
    let mut rejected = state_awaiting(
        "run-reversal-reject",
        &[sticky_call.clone(), exception.clone()],
    );
    rejected
        .reject_with_message(&sticky_call, true, "denied by default")
        .unwrap();
    rejected.approve(&exception, false).unwrap();
    rejected
        .reject_with_message(&exception, false, "denied exactly")
        .unwrap();
    assert!(is_rejected(&rejected, &exception));
    assert_eq!(
        rejected.rejection_message_for(exception.id()),
        Some("denied exactly")
    );
    assert_eq!(
        sticky(&rejected),
        (PermissionDecision::Deny, Some("denied by default"))
    );
}

/// A call rejected without text of its own falls back to the sticky rejection's text, whichever
/// answer came first, and a sticky answer settles the calls of its action still waiting.
#[test]
fn test_a_sticky_rejection_supplies_text_to_calls_rejected_without_their_own() {
    let first = approval_item("approval-a", "call-a");
    let second = approval_item("approval-b", "call-b");
    let unanswered = approval_item("approval-c", "call-c");
    let mut state = state_awaiting(
        "run-sticky-fallback",
        &[first.clone(), second.clone(), unanswered.clone()],
    );

    state.reject(&first, false).unwrap();
    state
        .reject_with_message(&second, true, "denied by default")
        .unwrap();

    assert!(is_rejected(&state, &first));
    assert_eq!(
        state.rejection_message_for(first.id()),
        Some("denied by default")
    );
    assert_eq!(
        state.rejection_message_for(second.id()),
        Some("denied by default")
    );
    assert!(
        is_rejected(&state, &unanswered),
        "the sticky rejection settles the call still waiting"
    );
    assert_eq!(
        state.rejection_message_for(unanswered.id()),
        Some("denied by default")
    );
    assert_eq!(state.unanswered_interruption_items().count(), 0);
}

/// An exact rejection keeps another call's sticky rejection and its text, and its own text.
#[test]
fn test_an_exact_rejection_keeps_the_sticky_rejection_of_another_call() {
    let first = approval_item("approval-a", "call-a");
    let second = approval_item("approval-b", "call-b");
    let mut state = state_awaiting("run-sticky-kept", &[first.clone(), second.clone()]);

    state
        .reject_with_message(&first, true, "denied by default")
        .unwrap();
    state
        .reject_with_message(&second, false, "denied exactly")
        .unwrap();

    assert_eq!(
        state.rejection_message_for(first.id()),
        Some("denied by default")
    );
    assert_eq!(
        state.rejection_message_for(second.id()),
        Some("denied exactly")
    );
    assert_eq!(
        sticky(&state),
        (PermissionDecision::Deny, Some("denied by default"))
    );
}

/// The reference's sticky value supersedes earlier exact entries of the same tool: a sticky
/// approval turns an exact rejection into an approval, and a sticky rejection turns an exact
/// approval into a rejection that keeps no text of its own.
#[test]
fn test_a_sticky_answer_supersedes_earlier_exact_answers() {
    let earlier = approval_item("approval-a", "call-a");
    let sticky_call = approval_item("approval-b", "call-b");
    let mut state = state_awaiting(
        "run-sticky-supersedes",
        &[earlier.clone(), sticky_call.clone()],
    );
    state
        .reject_with_message(&earlier, false, "denied exactly")
        .unwrap();
    state.approve(&sticky_call, true).unwrap();
    assert!(is_approved(&state, &earlier));
    assert_eq!(state.rejection_message_for(earlier.id()), None);

    let mut state = state_awaiting(
        "run-sticky-supersedes-2",
        &[earlier.clone(), sticky_call.clone()],
    );
    state.approve(&earlier, false).unwrap();
    state
        .reject_with_message(&sticky_call, true, "denied by default")
        .unwrap();
    assert!(is_rejected(&state, &earlier));
    assert_eq!(
        state.rejection_message_for(earlier.id()),
        Some("denied by default")
    );

    // An exact rejection text survives a later sticky rejection, as the reference keeps its
    // per-call messages when the sticky value changes to a rejection.
    let mut state = state_awaiting(
        "run-sticky-supersedes-3",
        &[earlier.clone(), sticky_call.clone()],
    );
    state
        .reject_with_message(&earlier, false, "denied exactly")
        .unwrap();
    state
        .reject_with_message(&sticky_call, true, "denied by default")
        .unwrap();
    assert_eq!(
        state.rejection_message_for(earlier.id()),
        Some("denied exactly")
    );
}

/// Ported from the reference's `test_exact_call_override_round_trips_with_sticky_default`.
#[test]
fn test_exact_call_override_round_trips_with_sticky_default() {
    for sticky_approved in [true, false] {
        let sticky_call = approval_item("approval-sticky", "sticky");
        let exception = approval_item("approval-exception", "exception");
        let mut state = state_awaiting(
            "run-mixed-roundtrip",
            &[sticky_call.clone(), exception.clone()],
        );
        if sticky_approved {
            state.approve(&sticky_call, true).unwrap();
            state
                .reject_with_message(&exception, false, "denied exactly")
                .unwrap();
        } else {
            state
                .reject_with_message(&sticky_call, true, "denied by default")
                .unwrap();
            state.approve(&exception, false).unwrap();
        }

        let restored: RunState =
            serde_json::from_value(serde_json::to_value(&state).unwrap()).unwrap();

        assert_eq!(is_approved(&restored, &exception), !sticky_approved);
        assert_eq!(
            restored.rejection_message_for(exception.id()),
            sticky_approved.then_some("denied exactly")
        );
        let expected = if sticky_approved {
            (PermissionDecision::Allow, None)
        } else {
            (PermissionDecision::Deny, Some("denied by default"))
        };
        assert_eq!(sticky(&restored), expected);
    }
}

#[test]
fn pending_tool_output_settlement_survives_a_checkpoint_and_defaults_off_for_legacy_records() {
    let mut state = RunState::start(RunId::new("settlement"));
    assert!(!state.tool_output_settlement_pending());
    state.set_tool_output_settlement_pending(true);
    let wire = serde_json::to_value(&state).unwrap();
    assert_eq!(wire["tool_output_settlement_pending"], true);
    let mut restored: RunState = serde_json::from_value(wire.clone()).unwrap();
    assert!(restored.tool_output_settlement_pending());
    restored.set_tool_output_settlement_pending(false);
    assert!(
        serde_json::to_value(restored)
            .unwrap()
            .get("tool_output_settlement_pending")
            .is_none()
    );
    let mut legacy = wire;
    legacy["schema_version"] = json!(11);
    legacy
        .as_object_mut()
        .unwrap()
        .remove("tool_output_settlement_pending");
    let restored: RunState = serde_json::from_value(legacy).unwrap();
    assert!(!restored.tool_output_settlement_pending());
    assert_eq!(restored.schema_version(), RUN_STATE_SCHEMA_VERSION);
}

#[test]
fn sticky_answers_require_agent_provenance_without_mutating_state() {
    for reject in [false, true] {
        let approval = approval_item("unowned", "unowned-call");
        let approval = RunItem::new(approval.id().clone(), approval.kind().clone());
        let mut state = state_awaiting("unowned-run", std::slice::from_ref(&approval));
        let before = state.clone();
        let result = if reject {
            state.reject_with_message(&approval, true, "denied")
        } else {
            state.approve(&approval, true)
        };
        assert!(result.unwrap_err().to_string().contains("provenance"));
        assert_eq!(state, before);
        if reject {
            state.reject(&approval, false).unwrap();
        } else {
            state.approve(&approval, false).unwrap();
        }
        assert!(
            !state.pending_interruption_resolutions()[0]
                .resolution()
                .always()
        );
    }
}

#[test]
fn current_schema_preserves_ownerless_rules_instead_of_applying_legacy_migration() {
    let mut wire = serde_json::to_value(RunState::start(RunId::new("current-rules"))).unwrap();
    wire["permission_rules"] = json!([{"decision":"deny", "tool_name":"operate"}]);
    let state: RunState = serde_json::from_value(wire).unwrap();
    assert_eq!(state.permission_rules().len(), 1);
    assert_eq!(
        RUN_STATE_SCHEMA_VERSION,
        ra_core::compat::SchemaVersion::new(18)
    );
    assert_eq!(state.schema_version(), RUN_STATE_SCHEMA_VERSION);
}

#[test]
fn saved_turn_limits_distinguish_legacy_and_explicitly_unlimited_checkpoints() {
    for limit in [None, Some(0), Some(10)] {
        let mut state = RunState::start(RunId::new("saved-limit"));
        assert_eq!(state.max_turns(), None);
        state.set_max_turns(limit);
        let restored: RunState =
            serde_json::from_str(&serde_json::to_string(&state).unwrap()).unwrap();
        assert_eq!(restored.max_turns(), Some(limit));
    }

    let mut legacy = serde_json::to_value(RunState::start(RunId::new("legacy-limit"))).unwrap();
    legacy["schema_version"] = json!(13);
    assert!(legacy.get("max_turns").is_none());
    let restored: RunState = serde_json::from_value(legacy).unwrap();
    assert_eq!(restored.max_turns(), None);
    assert_eq!(restored.schema_version(), RUN_STATE_SCHEMA_VERSION);
}

#[test]
fn generated_prompt_cache_key_roundtrips_and_old_states_omit_it() {
    let mut state = RunState::start(RunId::new("cache-run"));
    let legacy = serde_json::to_value(&state).unwrap();
    assert!(legacy.get("generated_prompt_cache_key").is_none());
    let restored: RunState = serde_json::from_value(legacy).unwrap();
    assert_eq!(restored.generated_prompt_cache_key(), None);
    state.set_generated_prompt_cache_key("agents-sdk:run:1234567890abcdef1234567890abcdef");
    let serialized = serde_json::to_value(&state).unwrap();
    assert_eq!(
        serialized["generated_prompt_cache_key"],
        state.generated_prompt_cache_key().unwrap()
    );
    let restored: RunState = serde_json::from_value(serialized).unwrap();
    assert_eq!(restored, state);
}

#[test]
fn malformed_optional_generated_cache_hints_follow_the_sdk_restore_behavior() {
    let state = RunState::start(RunId::new("cache-run"));
    for value in [json!(null), json!(42), json!({"future":"value"}), json!([])] {
        let mut json = serde_json::to_value(&state).unwrap();
        json["generated_prompt_cache_key"] = value;
        let restored: RunState = serde_json::from_value(json).unwrap();
        assert_eq!(restored.generated_prompt_cache_key(), None);
    }
    let mut json = serde_json::to_value(&state).unwrap();
    json["generated_prompt_cache_key"] = json!("prompt-cache-key-1");
    let restored: RunState = serde_json::from_value(json).unwrap();
    assert_eq!(
        restored.generated_prompt_cache_key(),
        Some("prompt-cache-key-1")
    );
}

#[test]
fn pending_context_input_requires_a_started_run_identity() {
    let mut state = RunState::start(RunId::new("unstarted-context"));
    state.queue_input_contributions([ModelInputItem::Message(Message::user("accepted input"))]);
    let wire = serde_json::to_value(&state).unwrap();
    let error = serde_json::from_value::<RunState>(wire).unwrap_err();
    assert!(error.to_string().contains("names no current agent"));
}
