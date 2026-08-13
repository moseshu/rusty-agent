//! Contracts for the structured failure record and the no-progress counter it feeds.

use ra_core::{
    item::{AgentId, CallId},
    state::{
        AgentToolFailures, EvidenceFingerprint, TOOL_FAILURE_RECENT_LIMIT, ToolFailureEntry,
        ToolFailureTracker, ToolOutcome, ToolUse,
    },
    tool::{ToolLookupKey, ToolNamespace},
};
use serde_json::{Value, json};

fn bare(name: &str) -> ToolUse {
    ToolUse::Tool(ToolLookupKey::bare(name).unwrap())
}

fn namespaced(namespace: &str, name: &str) -> ToolUse {
    ToolUse::Tool(ToolLookupKey::namespaced(ToolNamespace::new(namespace).unwrap(), name).unwrap())
}

fn failure(identity: ToolUse, call_id: &str, arguments: Value, evidence: Value) -> ToolOutcome {
    ToolOutcome::failed(
        identity,
        CallId::new(call_id),
        &arguments,
        &evidence,
        "tool.execution_failed",
    )
}

fn success(identity: ToolUse, call_id: &str, arguments: Value, evidence: Value) -> ToolOutcome {
    ToolOutcome::succeeded(identity, CallId::new(call_id), &arguments, &evidence)
}

fn refusal(identity: ToolUse, call_id: &str, arguments: Value, evidence: Value) -> ToolOutcome {
    ToolOutcome::refused(identity, CallId::new(call_id), &arguments, &evidence)
}

fn entry<'a>(
    tracker: &'a ToolFailureTracker,
    agent: &AgentId,
    identity: &ToolUse,
) -> &'a ToolFailureEntry {
    tracker
        .agent(agent)
        .and_then(|failures| failures.entry(identity))
        .unwrap_or_else(|| panic!("agent `{agent}` has no record for {identity:?}"))
}

#[test]
fn identical_failures_accumulate_and_new_evidence_resets() {
    // The acceptance case in both directions: a tool failing the same way three times is stuck,
    // and the same failure code carrying a different answer is a run that learned something.
    let agent = AgentId::new("coder");
    let tests = bare("run_tests");
    let mut tracker = ToolFailureTracker::new();

    for call in ["c-1", "c-2", "c-3"] {
        tracker.record_turn(
            &agent,
            [failure(
                tests.clone(),
                call,
                json!({}),
                json!({ "failed": ["a", "b"] }),
            )],
        );
    }
    assert_eq!(tracker.no_progress_streak(&agent, &tests), 3);

    tracker.record_turn(
        &agent,
        [failure(
            tests.clone(),
            "c-4",
            json!({}),
            json!({ "failed": ["a"] }),
        )],
    );
    assert_eq!(
        tracker.no_progress_streak(&agent, &tests),
        1,
        "one test fixed is evidence, even though the call failed again"
    );
    assert_eq!(entry(&tracker, &agent, &tests).run_failures(), 4);
}

#[test]
fn changing_the_arguments_does_not_clear_a_streak_the_answer_did_not() {
    // The difference from the repeat breaker, stated as a test: a model that edits its request
    // every time and receives the identical refusal has changed nothing that matters.
    let agent = AgentId::new("coder");
    let read = bare("read_file");
    let mut tracker = ToolFailureTracker::new();

    for (call, path) in [("c-1", "a.rs"), ("c-2", "b.rs"), ("c-3", "c.rs")] {
        tracker.record_turn(
            &agent,
            [failure(
                read.clone(),
                call,
                json!({ "path": path }),
                json!({ "error": { "code": "tool.execution_failed", "tool": "read_file" } }),
            )],
        );
    }

    assert_eq!(tracker.no_progress_streak(&agent, &read), 3);
    let inputs: Vec<&str> = entry(&tracker, &agent, &read)
        .recent()
        .iter()
        .map(|record| record.input().as_str())
        .collect();
    assert_eq!(
        inputs
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        3,
        "the record still shows three different requests; only the counter ignores that"
    );
    assert!(
        entry(&tracker, &agent, &read)
            .recent()
            .iter()
            .skip(1)
            .all(|record| !record.new_evidence())
    );
}

#[test]
fn a_success_clears_the_streak() {
    let agent = AgentId::new("coder");
    let exec = bare("exec_command");
    let mut tracker = ToolFailureTracker::new();

    for call in ["c-1", "c-2"] {
        tracker.record_turn(
            &agent,
            [failure(
                exec.clone(),
                call,
                json!({}),
                json!({ "error": 1 }),
            )],
        );
    }
    assert_eq!(tracker.no_progress_streak(&agent, &exec), 2);

    tracker.record_turn(
        &agent,
        [success(
            exec.clone(),
            "c-3",
            json!({}),
            json!({ "ok": true }),
        )],
    );
    assert_eq!(tracker.no_progress_streak(&agent, &exec), 0);
    assert!(entry(&tracker, &agent, &exec).last_evidence().is_none());

    // And the streak restarts from the failure after it rather than resuming where it left off.
    tracker.record_turn(
        &agent,
        [failure(
            exec.clone(),
            "c-4",
            json!({}),
            json!({ "error": 1 }),
        )],
    );
    assert_eq!(tracker.no_progress_streak(&agent, &exec), 1);
}

#[test]
fn replaying_an_old_reset_cannot_clear_a_newer_failure() {
    let agent = AgentId::new("coder");
    let exec = bare("exec_command");
    let mut tracker = ToolFailureTracker::new();

    tracker.record_turn(
        &agent,
        [success(
            exec.clone(),
            "success-1",
            json!({}),
            json!({ "ok": true }),
        )],
    );
    tracker.record_turn(
        &agent,
        [failure(
            exec.clone(),
            "failure-1",
            json!({}),
            json!({ "error": "same" }),
        )],
    );
    let snapshot = serde_json::to_string(&tracker).unwrap();
    let mut restored: ToolFailureTracker = serde_json::from_str(&snapshot).unwrap();
    restored.record_turn(
        &agent,
        [success(
            exec.clone(),
            "success-1",
            json!({}),
            json!({ "ok": true }),
        )],
    );
    assert_eq!(restored.no_progress_streak(&agent, &exec), 1);

    restored.record_turn(
        &agent,
        [refusal(
            exec.clone(),
            "refusal-1",
            json!({}),
            json!({ "error": { "code": "tool.no_progress" } }),
        )],
    );
    restored.record_turn(
        &agent,
        [failure(
            exec.clone(),
            "failure-2",
            json!({}),
            json!({ "error": "same" }),
        )],
    );
    restored.record_turn(
        &agent,
        [refusal(
            exec.clone(),
            "refusal-1",
            json!({}),
            json!({ "error": { "code": "tool.no_progress" } }),
        )],
    );

    assert_eq!(restored.no_progress_streak(&agent, &exec), 1);
}

#[test]
fn identities_and_agents_are_counted_apart() {
    let coder = AgentId::new("coder");
    let reviewer = AgentId::new("reviewer");
    let github = namespaced("mcp.github", "search");
    let gitlab = namespaced("mcp.gitlab", "search");
    let evidence = json!({ "error": "same words" });
    let mut tracker = ToolFailureTracker::new();

    tracker.record_turn(
        &coder,
        [
            failure(github.clone(), "c-1", json!({}), evidence.clone()),
            failure(gitlab.clone(), "c-2", json!({}), evidence.clone()),
        ],
    );
    tracker.record_turn(
        &coder,
        [failure(github.clone(), "c-3", json!({}), evidence.clone())],
    );
    tracker.record_turn(
        &reviewer,
        [failure(github.clone(), "c-4", json!({}), evidence)],
    );

    // Two servers exposing one name are two tools, and one tool is not two agents' problem.
    assert_eq!(tracker.no_progress_streak(&coder, &github), 2);
    assert_eq!(tracker.no_progress_streak(&coder, &gitlab), 1);
    assert_eq!(tracker.no_progress_streak(&reviewer, &github), 1);
    assert_eq!(tracker.no_progress_streak(&coder, &bare("search")), 0);
}

#[test]
fn settling_the_same_turn_twice_changes_nothing() {
    // Resuming an interruption settles one response again. Counting it twice would fire the
    // breaker at half its configured threshold, on a record that looks perfectly well formed.
    let agent = AgentId::new("coder");
    let exec = bare("exec_command");
    let evidence = json!({ "error": "same" });
    let turn = || {
        [
            failure(exec.clone(), "c-1", json!({}), evidence.clone()),
            failure(exec.clone(), "c-2", json!({}), evidence.clone()),
        ]
    };
    let mut tracker = ToolFailureTracker::new();

    tracker.record_turn(&agent, turn());
    let after_first = tracker.clone();
    tracker.record_turn(&agent, turn());

    assert_eq!(tracker, after_first);
    assert_eq!(tracker.no_progress_streak(&agent, &exec), 2);
}

#[test]
fn a_replayed_turn_longer_than_the_window_is_still_idempotent() {
    // One response may fail more times on one identity than the window retains. Checking a single
    // `call_id` at a time then cascades: each re-recorded failure evicts the next one about to be
    // checked, and the whole turn counts twice.
    let agent = AgentId::new("coder");
    let exec = bare("exec_command");
    let evidence = json!({ "error": "same" });
    let turn = || {
        (0..TOOL_FAILURE_RECENT_LIMIT + 4)
            .map(|index| {
                failure(
                    exec.clone(),
                    &format!("c-{index}"),
                    json!({}),
                    evidence.clone(),
                )
            })
            .collect::<Vec<_>>()
    };
    let mut tracker = ToolFailureTracker::new();

    tracker.record_turn(&agent, turn());
    let after_first = tracker.clone();
    tracker.record_turn(&agent, turn());

    assert_eq!(tracker, after_first);
    assert_eq!(
        entry(&tracker, &agent, &exec).run_failures() as usize,
        TOOL_FAILURE_RECENT_LIMIT + 4
    );
    assert_eq!(
        entry(&tracker, &agent, &exec).recent().len(),
        TOOL_FAILURE_RECENT_LIMIT
    );
}

#[test]
fn the_record_keeps_digests_and_not_the_payload() {
    // This value is written into every checkpoint, and a failing call's arguments and output are
    // exactly where a path, a query, or a credential a user pasted would show up.
    let agent = AgentId::new("coder");
    let read = bare("read_file");
    let mut tracker = ToolFailureTracker::new();

    tracker.record_turn(
        &agent,
        [failure(
            read,
            "c-1",
            json!({ "path": "/home/secret/.aws/credentials" }),
            json!({ "error": "AKIAIOSFODNN7EXAMPLE is not readable" }),
        )],
    );

    let snapshot = serde_json::to_string(&tracker).unwrap();
    assert!(!snapshot.contains("credentials"));
    assert!(!snapshot.contains("AKIAIOSFODNN7EXAMPLE"));
    assert!(snapshot.contains("tool.execution_failed"));

    let restored: ToolFailureTracker = serde_json::from_str(&snapshot).unwrap();
    assert_eq!(restored, tracker);
}

#[test]
fn evidence_is_compared_by_value_and_not_by_key_order() {
    // A provider does not promise key order. Digesting raw bytes would report "new evidence" for a
    // re-serialized copy of the same answer, which is the one direction that must not happen: it
    // makes a stuck run look like a progressing one.
    let ordered = EvidenceFingerprint::compute(&json!({ "a": 1, "b": [2, 3] }));
    let reordered = EvidenceFingerprint::compute(&json!({ "b": [2, 3], "a": 1 }));
    let reversed_array = EvidenceFingerprint::compute(&json!({ "a": 1, "b": [3, 2] }));

    assert_eq!(ordered, reordered);
    assert_ne!(ordered, reversed_array);
}

#[test]
fn a_higher_version_record_survives_a_round_trip() {
    let wire = json!({
        "schema_version": 2,
        "agents": {
            "coder": {
                "schema_version": 2,
                "entries": [{
                    "schema_version": 2,
                    "identity": { "type": "tool", "data": { "schema_version": 1, "kind": "bare", "name": "run_tests" } },
                    "run_failures": 2,
                    "no_progress_streak": 2,
                    "future_progress_signal": { "workspace_dirty": true }
                }],
                "future_agent_field": 7
            }
        },
        "future_tracker_field": "kept"
    });

    let restored: ToolFailureTracker = serde_json::from_value(wire).unwrap();
    let agent = AgentId::new("coder");
    assert_eq!(restored.no_progress_streak(&agent, &bare("run_tests")), 2);
    assert_eq!(
        restored.unknown().get("future_tracker_field"),
        Some(&json!("kept"))
    );
    let round_tripped = serde_json::to_value(&restored).unwrap();
    assert_eq!(round_tripped["future_tracker_field"], json!("kept"));
    assert_eq!(
        round_tripped["agents"]["coder"]["entries"][0]["future_progress_signal"],
        json!({ "workspace_dirty": true })
    );
}

#[test]
fn one_identity_cannot_appear_in_two_entries() {
    // Split counts mean every consumer reads whichever entry it finds first, and the breaker needs
    // twice the failures to fire — on a record that otherwise looks correct.
    let wire = json!({
        "schema_version": 1,
        "entries": [
            { "identity": { "type": "tool", "data": { "kind": "bare", "name": "run_tests" } }, "no_progress_streak": 1 },
            { "identity": { "type": "tool", "data": { "kind": "bare", "name": "run_tests" } }, "no_progress_streak": 2 }
        ]
    });

    assert!(serde_json::from_value::<AgentToolFailures>(wire).is_err());
}
