//! R3-6b contracts for per-agent tool-use tracking.

use ra_core::{
    item::{AgentId, CallId},
    state::{
        AgentToolUse, ArgumentFingerprint, TOOL_USE_RECENT_LIMIT, ToolUse, ToolUseAttempt,
        ToolUseEntry, ToolUseTracker,
    },
    tool::{ToolLookupKey, ToolNamespace},
};
use serde_json::json;

fn bare(name: &str) -> ToolUse {
    ToolUse::Tool(ToolLookupKey::bare(name).unwrap())
}

fn namespaced(namespace: &str, name: &str) -> ToolUse {
    ToolUse::Tool(ToolLookupKey::namespaced(ToolNamespace::new(namespace).unwrap(), name).unwrap())
}

fn attempt(identity: ToolUse, call_id: &str, arguments: serde_json::Value) -> ToolUseAttempt {
    ToolUseAttempt::new(identity, CallId::new(call_id), &arguments)
}

fn entry<'a>(tracker: &'a ToolUseTracker, agent: &AgentId, identity: &ToolUse) -> &'a ToolUseEntry {
    tracker
        .agent(agent)
        .and_then(|agent_use| agent_use.entry(identity))
        .unwrap_or_else(|| panic!("agent `{agent}` 没有 {identity:?} 的记录"))
}

#[test]
fn test_tool_use_tracker_01() {
    let planner = AgentId::new("planner");
    let executor = AgentId::new("executor");
    let read = bare("read_file");
    let mut tracker = ToolUseTracker::new();

    tracker.record_turn(
        &planner,
        [attempt(read.clone(), "p-1", json!({ "path": "a" }))],
    );
    tracker.record_turn(
        &executor,
        [
            attempt(read.clone(), "e-1", json!({ "path": "a" })),
            attempt(read.clone(), "e-2", json!({ "path": "a" })),
        ],
    );

    // The key is a stable `AgentId`. Filing by an agent's display name would fold two agents that
    // share one into a single account, and `AgentSpec` **deliberately** lets names collide.
    assert_eq!(entry(&tracker, &planner, &read).run_calls(), 1);
    assert_eq!(entry(&tracker, &executor, &read).run_calls(), 2);
    assert_eq!(tracker.repeat_streak(&planner, &read), 1);
    assert_eq!(tracker.repeat_streak(&executor, &read), 2);

    // An agent nothing filed is not an agent with zero calls, and the two have to stay apart.
    assert!(tracker.agent(&AgentId::new("reviewer")).is_none());
    assert_eq!(
        tracker.repeat_streak(&AgentId::new("reviewer"), &read),
        0,
        "从没跑过的 agent 读连续段应当是 0，不是 panic 也不是别人的值"
    );
}

#[test]
fn test_tool_use_tracker_02() {
    let agent = AgentId::new("main");
    let github = namespaced("mcp.github", "search");
    let gitlab = namespaced("mcp.gitlab", "search");
    let local = bare("search");
    let mut tracker = ToolUseTracker::new();

    tracker.record_turn(
        &agent,
        [
            attempt(github.clone(), "c-1", json!({ "q": "x" })),
            attempt(gitlab.clone(), "c-2", json!({ "q": "x" })),
            attempt(local.clone(), "c-3", json!({ "q": "x" })),
            attempt(github.clone(), "c-4", json!({ "q": "x" })),
        ],
    );

    // The three `search` entries are three identities. Counting by the model-facing name folds them
    // into one, so the breaker fires on two unrelated tools while missing a real repeat.
    let agent_use = tracker.agent(&agent).unwrap();
    assert_eq!(agent_use.entries().len(), 3);
    assert_eq!(entry(&tracker, &agent, &github).run_calls(), 2);
    assert_eq!(entry(&tracker, &agent, &gitlab).run_calls(), 1);
    assert_eq!(entry(&tracker, &agent, &local).run_calls(), 1);

    // Records are ordered by first use, which is where the determinism comes from rather than from
    // lexical order.
    let order = agent_use
        .entries()
        .iter()
        .map(|entry| entry.identity().clone())
        .collect::<Vec<_>>();
    assert_eq!(order, [github, gitlab, local]);
}

#[test]
fn test_tool_use_tracker_03() {
    let same_a = ArgumentFingerprint::compute(&json!({ "path": "a.txt", "limit": 10 }));
    let same_b = ArgumentFingerprint::compute(&json!({ "limit": 10, "path": "a.txt" }));
    let nested_a = ArgumentFingerprint::compute(&json!({ "o": { "x": 1, "y": 2 } }));
    let nested_b = ArgumentFingerprint::compute(&json!({ "o": { "y": 2, "x": 1 } }));
    let different = ArgumentFingerprint::compute(&json!({ "path": "b.txt", "limit": 10 }));

    // A provider promises nothing about key order. Fingerprinting the raw bytes would have the
    // detector call it "a different call" exactly while the model goes round in circles.
    assert_eq!(same_a, same_b);
    assert_eq!(nested_a, nested_b);
    assert_ne!(same_a, different);

    // Arrays are ordered, so a reordering is a different call.
    assert_ne!(
        ArgumentFingerprint::compute(&json!({ "xs": [1, 2] })),
        ArgumentFingerprint::compute(&json!({ "xs": [2, 1] }))
    );
}

#[test]
fn test_tool_use_tracker_04() {
    let agent = AgentId::new("main");
    let grep = bare("grep");
    let read = bare("read_file");
    let mut tracker = ToolUseTracker::new();

    // Two tools called alternately, neither with changed arguments. A streak is counted over **this
    // identity's own** call sequence: if another tool in between reset it, an agent stuck in a
    // read/grep loop would report two streaks of length one — the loop invisible.
    for turn in 0..3 {
        tracker.record_turn(
            &agent,
            [
                attempt(read.clone(), &format!("r-{turn}"), json!({ "path": "a" })),
                attempt(grep.clone(), &format!("g-{turn}"), json!({ "q": "todo" })),
            ],
        );
    }
    assert_eq!(tracker.repeat_streak(&agent, &read), 3);
    assert_eq!(tracker.repeat_streak(&agent, &grep), 3);

    // Changed arguments are a changed hypothesis, so the streak restarts here while the running total
    // carries on.
    tracker.record_turn(
        &agent,
        [attempt(read.clone(), "r-9", json!({ "path": "b" }))],
    );
    assert_eq!(tracker.repeat_streak(&agent, &read), 1);
    assert_eq!(entry(&tracker, &agent, &read).run_calls(), 4);
    // A tool this turn did not name keeps its streak: it neither repeated nor changed a hypothesis.
    assert_eq!(tracker.repeat_streak(&agent, &grep), 3);
}

#[test]
fn test_tool_use_tracker_05() {
    let agent = AgentId::new("main");
    let read = bare("read_file");
    let write = bare("write_file");
    let mut tracker = ToolUseTracker::new();

    tracker.record_turn(
        &agent,
        [
            attempt(read.clone(), "r-1", json!({ "path": "a" })),
            attempt(read.clone(), "r-2", json!({ "path": "b" })),
            attempt(write.clone(), "w-1", json!({ "path": "c" })),
        ],
    );
    assert_eq!(entry(&tracker, &agent, &read).turn_calls(), 2);
    assert_eq!(tracker.agent(&agent).unwrap().turn_calls(), 3);
    assert!(tracker.used_any_this_turn(&agent));

    tracker.record_turn(
        &agent,
        [attempt(read.clone(), "r-3", json!({ "path": "a" }))],
    );
    assert_eq!(entry(&tracker, &agent, &read).turn_calls(), 1);
    assert_eq!(entry(&tracker, &agent, &read).run_calls(), 3);
    // A tool this turn did not name has to read zero rather than hold last turn's value:
    // `reset_tool_choice` and the tool-use behavior both ask about *this* turn.
    assert_eq!(entry(&tracker, &agent, &write).turn_calls(), 0);
    assert_eq!(entry(&tracker, &agent, &write).run_calls(), 1);

    // A turn that asked for nothing: the agent stays on file, since it did run a turn, but this
    // turn is empty.
    tracker.record_turn(&agent, []);
    assert!(!tracker.used_any_this_turn(&agent));
    assert_eq!(tracker.agent(&agent).unwrap().turn_calls(), 0);
    assert_eq!(tracker.agent(&agent).unwrap().run_calls(), 4);
}

#[test]
fn test_tool_use_tracker_06() {
    let agent = AgentId::new("main");
    let vanished = ToolUse::Unresolved("vanished".to_owned());
    let mut tracker = ToolUseTracker::new();

    tracker.record_turn(
        &agent,
        [
            attempt(vanished.clone(), "c-1", json!({})),
            attempt(ToolUse::Handoff(AgentId::new("reviewer")), "c-2", json!({})),
            attempt(
                ToolUse::Mcp {
                    server: "docs".to_owned(),
                    tool_name: "search".to_owned(),
                },
                "req-1",
                json!({ "q": "x" }),
            ),
        ],
    );

    // `reset_tool_choice` asks whether the model asked for anything this turn. An unresolvable name,
    // a transfer of control, and a hosted approval all count as asking — looking only at what ran
    // successfully would leave a forced `tool_choice` pinned on.
    assert!(tracker.used_any_this_turn(&agent));
    assert_eq!(tracker.agent(&agent).unwrap().entries().len(), 3);
    assert_eq!(entry(&tracker, &agent, &vanished).run_calls(), 1);

    // Reaching for the same non-existent name over and over is a loop too.
    tracker.record_turn(&agent, [attempt(vanished.clone(), "c-3", json!({}))]);
    assert_eq!(tracker.repeat_streak(&agent, &vanished), 2);
}

#[test]
fn test_tool_use_tracker_07() {
    let agent = AgentId::new("main");
    let read = bare("read_file");
    let total = TOOL_USE_RECENT_LIMIT + 3;
    let mut tracker = ToolUseTracker::new();

    for index in 0..total {
        tracker.record_turn(
            &agent,
            [attempt(
                read.clone(),
                &format!("c-{index}"),
                json!({ "path": "a" }),
            )],
        );
    }

    let read_entry = entry(&tracker, &agent, &read);
    // An unbounded trail grows with the run and is rewritten whole at every checkpoint.
    assert_eq!(read_entry.recent().len(), TOOL_USE_RECENT_LIMIT);
    // The calls the window pushed out still count: the total and the streak are folded values, not a
    // projection of the window.
    assert_eq!(read_entry.run_calls(), u32::try_from(total).unwrap());
    assert_eq!(read_entry.repeat_streak(), u32::try_from(total).unwrap());
    // What stays is the most recent calls; the oldest go first.
    assert_eq!(read_entry.recent()[0].call_id().as_str(), "c-3");
    assert_eq!(
        read_entry.recent().last().unwrap().call_id().as_str(),
        &format!("c-{}", total - 1)
    );
}

#[test]
fn test_tool_use_tracker_08() {
    let agent = AgentId::new("main");
    let read = bare("read_file");
    let mut tracker = ToolUseTracker::new();

    let turn = || {
        [
            attempt(read.clone(), "c-1", json!({ "path": "a" })),
            attempt(read.clone(), "c-2", json!({ "path": "a" })),
        ]
    };
    tracker.record_turn(&agent, turn());
    // Resuming an interruption settles the same response a second time. Counting it twice makes the
    // model look more repetitive than it was, and the breaker acts on exactly that number.
    tracker.record_turn(&agent, turn());

    let read_entry = entry(&tracker, &agent, &read);
    assert_eq!(read_entry.run_calls(), 2);
    assert_eq!(read_entry.repeat_streak(), 2);
    assert_eq!(read_entry.recent().len(), 2);
    // The per-turn count is still what this turn really asked for, and a replay reads the same value
    // as the first pass.
    assert_eq!(read_entry.turn_calls(), 2);
}

#[test]
fn test_tool_use_tracker_09() {
    let agent = AgentId::new("main");
    let read = bare("read_file");
    let mut tracker = ToolUseTracker::new();
    let total = TOOL_USE_RECENT_LIMIT + 1;
    let turn = || {
        (0..total)
            .map(|index| attempt(read.clone(), &format!("c-{index}"), json!({ "path": "a" })))
            .collect::<Vec<_>>()
    };

    tracker.record_turn(&agent, turn());
    tracker.record_turn(&agent, turn());

    let read_entry = entry(&tracker, &agent, &read);
    assert_eq!(read_entry.run_calls(), u32::try_from(total).unwrap());
    assert_eq!(
        read_entry.repeat_streak(),
        u32::try_from(total).unwrap(),
        "重放不能因窗口淘汰而抬高熔断计数"
    );
    assert_eq!(read_entry.turn_calls(), u32::try_from(total).unwrap());
}

#[test]
fn test_tool_use_tracker_10() {
    let agent = AgentId::new("main");
    let read = bare("read_file");
    let mut tracker = ToolUseTracker::new();
    tracker.record_turn(
        &agent,
        [attempt(
            read.clone(),
            "c-1",
            json!({ "token": "hunter2-secret-value" }),
        )],
    );

    let encoded = serde_json::to_string(&tracker).unwrap();
    // Tool arguments hold whatever the model put in them, and this structure goes into every
    // checkpoint. A digest answers the only question a consumer actually asks — "is this the same
    // call as last time?" — while keeping the payload out of the snapshot entirely.
    assert!(
        !encoded.contains("hunter2-secret-value"),
        "raw arguments must not appear in a snapshot: {encoded}"
    );
    assert!(encoded.contains(
        ArgumentFingerprint::compute(&json!({ "token": "hunter2-secret-value" })).as_str()
    ));

    let mut restored: ToolUseTracker = serde_json::from_str(&encoded).unwrap();
    assert_eq!(restored, tracker);

    // Counting has to continue after a restore. Coming back with an empty tracker would zero every
    // streak on each pause and resume, which turns "stop and continue" into a way around the breaker.
    restored.record_turn(
        &agent,
        [attempt(
            read.clone(),
            "c-2",
            json!({ "token": "hunter2-secret-value" }),
        )],
    );
    assert_eq!(restored.repeat_streak(&agent, &read), 2);
    assert_eq!(entry(&restored, &agent, &read).run_calls(), 2);
}

#[test]
fn test_tool_use_tracker_11() {
    let stored = json!({
        "schema_version": 9,
        "agents": {
            "main": {
                "schema_version": 9,
                "entries": [{
                    "schema_version": 9,
                    "identity": { "type": "tool", "data": {
                        "schema_version": 1, "kind": "bare", "name": "read_file"
                    }},
                    "run_calls": 2,
                    "turn_calls": 1,
                    "repeat_streak": 2,
                    "recent": [],
                    "cost_cents": 41
                }],
                "quarantined": true
            }
        },
        "sub_runs": ["nested-1"]
    });

    let tracker: ToolUseTracker = serde_json::from_value(stored).unwrap();
    assert_eq!(tracker.schema_version().get(), 9);
    assert_eq!(
        tracker.unknown().get("sub_runs"),
        Some(&json!(["nested-1"]))
    );

    let agent = AgentId::new("main");
    let agent_use = tracker.agent(&agent).unwrap();
    assert_eq!(agent_use.unknown().get("quarantined"), Some(&json!(true)));
    assert_eq!(
        agent_use.entries()[0].unknown().get("cost_cents"),
        Some(&json!(41))
    );
    assert_eq!(agent_use.entries()[0].run_calls(), 2);

    // Not one field may be lost along the "a newer build writes, an older one reads, the older one
    // writes again" path.
    let rewritten = serde_json::to_value(&tracker).unwrap();
    assert_eq!(rewritten["sub_runs"], json!(["nested-1"]));
    assert_eq!(rewritten["agents"]["main"]["quarantined"], json!(true));
    assert_eq!(
        rewritten["agents"]["main"]["entries"][0]["cost_cents"],
        json!(41)
    );
}

#[test]
fn test_tool_use_tracker_12() {
    let identity = json!({ "type": "tool", "data": {
        "schema_version": 1, "kind": "bare", "name": "read_file"
    }});
    let stored = json!({
        "schema_version": 1,
        "agents": {
            "main": {
                "schema_version": 1,
                "entries": [
                    { "schema_version": 1, "identity": identity, "run_calls": 3, "recent": [] },
                    { "schema_version": 1, "identity": identity, "run_calls": 4, "recent": [] }
                ]
            }
        }
    });

    // The count is split in two and each consumer reads whichever half it hits first: the breaker
    // needs twice the repetition to fire, while the record itself looks perfectly normal.
    let error = serde_json::from_value::<ToolUseTracker>(stored).unwrap_err();
    assert!(error.to_string().contains("read_file"), "{error}");
}

#[test]
fn test_tool_use_tracker_13() {
    for identity in [
        bare("read_file"),
        namespaced("mcp.github", "search"),
        ToolUse::Handoff(AgentId::new("reviewer")),
        ToolUse::Mcp {
            server: "docs".to_owned(),
            tool_name: "search".to_owned(),
        },
        ToolUse::Unresolved("vanished".to_owned()),
    ] {
        let encoded = serde_json::to_string(&identity).unwrap();
        let decoded: ToolUse = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, identity, "{encoded} 没能原样回来");
    }

    // A deferred top-level tool and a bare tool of the same name are not one identity, and that
    // distinction has to survive all the way into the snapshot.
    let deferred = ToolUse::Tool(ToolLookupKey::deferred_top_level("read_file").unwrap());
    assert_ne!(deferred, bare("read_file"));
    let round_tripped: ToolUse =
        serde_json::from_str(&serde_json::to_string(&deferred).unwrap()).unwrap();
    assert_ne!(round_tripped, bare("read_file"));
}

#[test]
fn test_tool_use_tracker_14() {
    let agent = AgentId::new("main");
    let mut tracker = ToolUseTracker::new();
    tracker.record_turn(
        &agent,
        [
            attempt(bare("read_file"), "c-1", json!({})),
            attempt(bare("write_file"), "c-2", json!({})),
            attempt(bare("read_file"), "c-3", json!({})),
        ],
    );

    let agent_use: &AgentToolUse = tracker.agent(&agent).unwrap();
    let summed: u32 = agent_use
        .entries()
        .iter()
        .map(ToolUseEntry::run_calls)
        .sum();
    assert_eq!(agent_use.run_calls(), summed);
    assert_eq!(agent_use.turn_calls(), summed);
    assert_eq!(agent_use.used_any_this_turn(), agent_use.turn_calls() > 0);
}
