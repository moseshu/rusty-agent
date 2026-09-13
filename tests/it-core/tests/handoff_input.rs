//! Contracts for what a transfer of control hands the agent taking over.

use ra_core::{
    agent::{HandoffInputData, HistoryProjection},
    item::{
        AgentId, CallId, HandoffCall, HandoffOutput, ItemId, McpApprovalRequest, Message,
        ModelInputItem, OutputPhase, RunItem, RunItemKind, ToolApproval, ToolCall, ToolCallOutput,
    },
};
use serde_json::json;

fn item(id: &str, kind: RunItemKind) -> RunItem {
    RunItem::new(ItemId::new(id), kind)
}

fn message(id: &str, text: &str) -> RunItem {
    item(
        id,
        RunItemKind::Message(Message::assistant(text, OutputPhase::Commentary)),
    )
}

fn tool_call(id: &str, call_id: &str) -> RunItem {
    item(
        id,
        RunItemKind::ToolCall(ToolCall::new(
            CallId::new(call_id),
            "read_file",
            json!({ "path": "a.txt" }),
        )),
    )
}

fn tool_output(id: &str, call_id: &str) -> RunItem {
    item(
        id,
        RunItemKind::ToolCallOutput(ToolCallOutput::new(CallId::new(call_id), json!("content"))),
    )
}

fn handoff_call(id: &str, call_id: &str) -> RunItem {
    item(
        id,
        RunItemKind::HandoffCall(
            HandoffCall::new(
                CallId::new(call_id),
                AgentId::new("reviewer"),
                json!({ "reason": "needs review" }),
            )
            .with_tool_name("transfer_to_reviewer"),
        ),
    )
}

fn handoff_output(id: &str, call_id: &str) -> RunItem {
    item(
        id,
        RunItemKind::HandoffOutput(HandoffOutput::new(
            CallId::new(call_id),
            AgentId::new("planner"),
            AgentId::new("reviewer"),
        )),
    )
}

/// One transfer, preceded by a tool exchange and some narration.
fn complete() -> HandoffInputData {
    HandoffInputData::new(
        vec![ModelInputItem::Message(Message::user("请处理这件事"))],
        vec![message("pre-1", "先看一下"), message("pre-2", "看完了")],
        vec![
            message("new-1", "转给审阅者"),
            tool_call("new-2", "call-tool"),
            tool_output("new-3", "call-tool"),
            handoff_call("new-4", "call-handoff"),
            handoff_output("new-5", "call-handoff"),
        ],
    )
}

fn ids(items: &[RunItem]) -> Vec<&str> {
    items.iter().map(|item| item.id().as_str()).collect()
}

#[test]
fn full_projection_hands_over_everything_and_none_hands_over_only_the_transfer() {
    let complete = complete();
    let transfer = CallId::new("call-handoff");

    let full = complete
        .project(&HistoryProjection::Full, &transfer)
        .unwrap();
    assert_eq!(full.input_history().len(), 1);
    assert_eq!(ids(full.pre_handoff_items()), ["pre-1", "pre-2"]);
    assert_eq!(full.new_items().len(), 5);

    // The default. The receiving agent is answering a brief, not reading the caller's transcript —
    // and the brief is exactly the call that moved control and the record answering it.
    let none = complete
        .project(&HistoryProjection::None, &transfer)
        .unwrap();
    assert!(none.input_history().is_empty());
    assert!(none.pre_handoff_items().is_empty());
    assert_eq!(ids(none.new_items()), ["new-4", "new-5"]);
}

#[test]
fn last_items_counts_backwards_and_never_spends_the_window_on_the_transfer() {
    let complete = complete();
    let transfer = CallId::new("call-handoff");

    // Two items of predecessor history: the last two before the transfer records, which are the
    // tool call and its output. The transfer itself is retained on top of the window rather than
    // out of it, so a window this small still arrives with the brief attached.
    let window = complete
        .project(&HistoryProjection::LastItems(2), &transfer)
        .unwrap();
    assert!(window.input_history().is_empty());
    assert!(window.pre_handoff_items().is_empty());
    assert_eq!(
        ids(window.new_items()),
        ["new-2", "new-3", "new-4", "new-5"]
    );

    // Once the window is wider than this turn's three ordinary records, the allowance carries on
    // into the earlier turns and then into the caller's own input, in that order.
    let wide = complete
        .project(&HistoryProjection::LastItems(4), &transfer)
        .unwrap();
    assert_eq!(wide.new_items().len(), 5);
    assert_eq!(ids(wide.pre_handoff_items()), ["pre-2"]);
    assert!(wide.input_history().is_empty());

    let widest = complete
        .project(&HistoryProjection::LastItems(6), &transfer)
        .unwrap();
    assert_eq!(ids(widest.pre_handoff_items()), ["pre-1", "pre-2"]);
    assert_eq!(widest.input_history().len(), 1);
}

#[test]
fn a_summary_projection_is_refused_rather_than_quietly_downgraded() {
    let error = complete()
        .project(&HistoryProjection::Summary, &CallId::new("call-handoff"))
        .unwrap_err();
    // Downgrading to `Full` would expand authority and downgrading to `None` would withhold the
    // context that was promised; both are worse than saying the projection cannot be applied.
    assert!(error.to_string().contains("summarizer"));
    assert!(HistoryProjection::Summary.requires_summarizer());
    assert!(!HistoryProjection::Full.requires_summarizer());
}

#[test]
fn flattening_drops_control_plane_records_and_repairs_broken_pairs() {
    let data = HandoffInputData::new(
        vec![ModelInputItem::Message(Message::user("请处理这件事"))],
        vec![
            // An answer whose question a narrowing left behind, and a question whose answer it did.
            tool_output("pre-1", "call-orphan-output"),
            tool_call("pre-2", "call-orphan-call"),
        ],
        vec![
            item(
                "new-1",
                RunItemKind::ToolApproval(ToolApproval::new(
                    CallId::new("call-approval"),
                    "write_file",
                    json!({}),
                )),
            ),
            item(
                "new-2",
                RunItemKind::McpApprovalRequest(McpApprovalRequest::new(
                    "req-1",
                    "docs",
                    "search",
                    json!({}),
                )),
            ),
            handoff_call("new-3", "call-handoff"),
            handoff_output("new-4", "call-handoff"),
        ],
    );

    let input = data.into_model_input();
    let labels = input.iter().map(ModelInputItem::label).collect::<Vec<_>>();
    // The approval record has no model-input form at all; the two orphans and the unanswered hosted
    // request are dropped because a provider rejects a history that carries either half alone.
    assert_eq!(labels, ["message", "handoff_call", "handoff_output"]);
}
