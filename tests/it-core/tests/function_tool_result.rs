//! Function-tool execution-result construction contracts.

use ra_core::{
    agent::FunctionToolResult,
    item::{CallId, ItemId, RunItem, RunItemKind, ToolApproval, ToolCallOutput},
    tool::ToolOrigin,
};
use serde_json::json;

fn output(call_id: &str, payload: serde_json::Value) -> ToolCallOutput {
    ToolCallOutput::new(CallId::new(call_id), payload)
}

fn approval_item(call_id: &str) -> RunItem {
    RunItem::new(
        ItemId::new(format!("{call_id}.approval")),
        RunItemKind::ToolApproval(ToolApproval::new(
            CallId::new(call_id),
            "exec_command",
            json!({ "command": "ls" }),
        )),
    )
}

#[test]
fn completed_result_requires_a_matching_output_item_but_not_a_duplicate_payload() {
    let result = FunctionToolResult::completed(
        ToolOrigin::new("read_file").unwrap(),
        output("call-1", json!({ "retained": "full result" })),
        RunItem::new(
            ItemId::new("call-1.output"),
            RunItemKind::ToolCallOutput(output("call-1", json!({ "trimmed": true }))),
        ),
    )
    .unwrap();

    assert_eq!(result.call_id().unwrap().as_str(), "call-1");
    assert!(result.output().is_some());
}

#[test]
fn an_approval_result_answers_no_output_yet_but_still_names_its_call() {
    let result = FunctionToolResult::awaiting_approval(
        ToolOrigin::new("exec_command").unwrap(),
        approval_item("call-1"),
    )
    .unwrap();

    // The call ID has to survive the one state where there is no output to read it from: a host
    // pairing an interruption back to the call it suspended has nothing else to key on.
    assert_eq!(result.call_id().unwrap().as_str(), "call-1");
    assert!(result.output().is_none());
    assert_eq!(result.interruptions().len(), 1);
    assert!(result.run_item().is_some());
}

#[test]
fn an_approval_result_rejects_a_record_that_is_not_an_approval() {
    let error = FunctionToolResult::awaiting_approval(
        ToolOrigin::new("exec_command").unwrap(),
        RunItem::new(
            ItemId::new("call-1.output"),
            RunItemKind::ToolCallOutput(output("call-1", json!("result"))),
        ),
    )
    .unwrap_err();

    assert_eq!(error.code(), "caller");
}

#[test]
fn completed_result_rejects_an_output_item_for_a_different_call() {
    let error = FunctionToolResult::completed(
        ToolOrigin::new("read_file").unwrap(),
        output("call-1", json!("result")),
        RunItem::new(
            ItemId::new("call-2.output"),
            RunItemKind::ToolCallOutput(output("call-2", json!("result"))),
        ),
    )
    .unwrap_err();

    assert_eq!(error.code(), "caller");
}
