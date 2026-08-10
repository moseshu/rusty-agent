//! R6-6 的 `RunState` 骨架：一次 run 自己的可续接状态（与 R3-13 的任务态分属两处）。

use ra_core::{
    compat::SchemaVersion,
    item::{AgentId, CallId},
    state::{RUN_STATE_SCHEMA_VERSION, RunState, ToolUse, ToolUseAttempt},
    tool::ToolLookupKey,
};
use serde_json::json;

fn 记一次调用(state: &mut RunState, agent: &AgentId, identity: &ToolUse, call: &str) {
    let attempt = ToolUseAttempt::new(identity.clone(), CallId::new(call), &json!({"path":"a"}));
    state.tool_use_mut().record_turn(agent, [attempt]);
}

#[test]
fn 默认状态是当前版本且没有工具轨迹() {
    let state = RunState::new();

    assert_eq!(state.schema_version(), RUN_STATE_SCHEMA_VERSION);
    assert_eq!(state.tool_use().agents().count(), 0);
    assert!(state.unknown().is_empty());
}

#[test]
fn 工具轨迹作为完整状态的一部分续接() {
    let agent = AgentId::new("coder");
    let identity = ToolUse::Tool(ToolLookupKey::bare("write_file").unwrap());
    let mut state = RunState::new();
    记一次调用(&mut state, &agent, &identity, "call-1");

    let restored: RunState =
        serde_json::from_str(&serde_json::to_string(&state).expect("run state must serialize"))
            .expect("run state must deserialize");

    assert_eq!(restored.tool_use().repeat_streak(&agent, &identity), 1);
}

#[test]
fn 换掉工具轨迹不动同一份状态里的其它字段() {
    // 守的是「按字段续接」那个失败模式：R3-8 与 R6-6 往这里加的东西，不能因为调用方只
    // 认识 tool_use 就被顺手抹掉。今天唯一的其它字段是更高版本写下的未知字段。
    let stored = r#"{ "schema_version": 7, "future_policy": { "enabled": true } }"#;
    let state: RunState = serde_json::from_str(stored).expect("newer state must remain readable");

    let agent = AgentId::new("coder");
    let identity = ToolUse::Tool(ToolLookupKey::bare("write_file").unwrap());
    let mut carried = RunState::new();
    记一次调用(&mut carried, &agent, &identity, "call-1");
    let state = state.with_tool_use(carried.tool_use().clone());

    assert_eq!(state.tool_use().repeat_streak(&agent, &identity), 1);
    assert_eq!(state.schema_version(), SchemaVersion::new(7));
    let written = serde_json::to_value(&state).expect("run state must serialize");
    assert_eq!(written["future_policy"]["enabled"], true);
}

#[test]
fn 缺少后来加入的字段仍能读出且未知字段会回写() {
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
