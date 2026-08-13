//! R6-6 的 `RunState` 骨架：一次 run 自己的可续接状态（与 R3-13 的任务态分属两处）。

use ra_core::{
    budget::BudgetSnapshot,
    compat::SchemaVersion,
    item::{AgentId, CallId},
    state::{RUN_STATE_SCHEMA_VERSION, RunState, ToolUse, ToolUseAttempt},
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
    let state = RunState::new();

    assert_eq!(state.schema_version(), RUN_STATE_SCHEMA_VERSION);
    assert_eq!(state.tool_use().agents().count(), 0);
    assert!(state.unknown().is_empty());
}

#[test]
fn test_run_state_02() {
    let agent = AgentId::new("coder");
    let identity = ToolUse::Tool(ToolLookupKey::bare("write_file").unwrap());
    let mut budget = BudgetSnapshot::new();
    budget.record_turn();
    budget.record_usage(&Usage::new(3, 4));
    let mut state = RunState::new().with_budget(budget);
    record_call(&mut state, &agent, &identity, "call-1");

    let restored: RunState =
        serde_json::from_str(&serde_json::to_string(&state).expect("run state must serialize"))
            .expect("run state must deserialize");

    assert_eq!(restored.tool_use().repeat_streak(&agent, &identity), 1);
    assert_eq!(restored.budget().turns_used(), 1);
    assert_eq!(restored.budget().tokens_used(), 7);
}

#[test]
fn test_run_state_03() {
    // 守的是「按字段续接」那个失败模式：R3-8 与 R6-6 往这里加的东西，不能因为调用方只
    // 认识 tool_use 就被顺手抹掉。今天唯一的其它字段是更高版本写下的未知字段。
    let stored = r#"{ "schema_version": 7, "future_policy": { "enabled": true } }"#;
    let state: RunState = serde_json::from_str(stored).expect("newer state must remain readable");

    let agent = AgentId::new("coder");
    let identity = ToolUse::Tool(ToolLookupKey::bare("write_file").unwrap());
    let mut carried = RunState::new();
    record_call(&mut carried, &agent, &identity, "call-1");
    let state = state.with_tool_use(carried.tool_use().clone());

    assert_eq!(state.tool_use().repeat_streak(&agent, &identity), 1);
    assert_eq!(state.schema_version(), SchemaVersion::new(7));
    let written = serde_json::to_value(&state).expect("run state must serialize");
    assert_eq!(written["future_policy"]["enabled"], true);
}

#[test]
fn test_run_state_04() {
    let stored = r#"{
        "schema_version": 7,
        "future_policy": { "enabled": true }
    }"#;
    let state: RunState = serde_json::from_str(stored).expect("newer state must remain readable");
    let written = serde_json::to_value(&state).expect("run state must serialize");

    assert_eq!(state.schema_version(), SchemaVersion::new(7));
    assert_eq!(state.tool_use().agents().count(), 0);
    assert_eq!(written["future_policy"]["enabled"], true);
}
