//! R3-3 contracts for the single product of one settled turn.

use std::sync::Arc;

use ra_core::{
    agent::{AgentId, AgentSpec},
    finish::FinishReason,
    item::{
        CallId, ItemId, ItemProvenance, McpApprovalRequest, Message, MessageRole, ModelInputItem,
        ModelResponse, OutputPhase, RunItem, RunItemKind, ToolApproval,
    },
    step::{NextStep, ProcessedResponse, SingleStepResult},
};
use serde_json::json;

fn item(id: &str, kind: RunItemKind) -> RunItem {
    RunItem::new(ItemId::new(id), kind)
}

fn message(id: &str, text: &str) -> RunItem {
    item(
        id,
        RunItemKind::Message(Message::assistant(text, OutputPhase::Final)),
    )
}

fn tool_approval(id: &str) -> RunItem {
    item(
        id,
        RunItemKind::ToolApproval(ToolApproval::new(
            CallId::new(id),
            "write_file",
            json!({ "path": "a.txt" }),
        )),
    )
}

fn quiet_response() -> ProcessedResponse {
    ProcessedResponse::builder()
        .item(message("msg-1", "完事了"))
        .build()
        .unwrap()
}

fn mcp_approval(id: &str) -> RunItem {
    item(
        id,
        RunItemKind::McpApprovalRequest(McpApprovalRequest::new(
            "req-1",
            "docs",
            "search",
            json!({ "query": "x" }),
        )),
    )
}

fn pending_response() -> ProcessedResponse {
    ProcessedResponse::builder()
        .mcp_approval(mcp_approval("approval-1"))
        .unwrap()
        .build()
        .unwrap()
}

/// The model response `pending_response()` was built from. The two have to be the same batch of
/// records, or `build()` fails on the consistency check first.
fn pending_model_response() -> ModelResponse {
    ModelResponse::new(vec![mcp_approval("approval-1")])
}

fn settled() -> ra_core::step::SingleStepResultBuilder {
    SingleStepResult::builder()
        .model_response(ModelResponse::new(vec![message("msg-1", "完事了")]))
        .processed_response(quiet_response())
        .next_step(NextStep::FinalOutput {
            reason: FinishReason::Final,
        })
        .session_step_items(vec![message("msg-1", "完事了")])
}

#[test]
fn test_single_step_result_01() {
    for (label, builder) in [
        (
            "model_response",
            SingleStepResult::builder()
                .processed_response(quiet_response())
                .next_step(NextStep::RunAgain)
                .session_step_items(Vec::new()),
        ),
        (
            "processed_response",
            SingleStepResult::builder()
                .model_response(ModelResponse::new(Vec::new()))
                .next_step(NextStep::RunAgain)
                .session_step_items(Vec::new()),
        ),
        (
            "next_step",
            SingleStepResult::builder()
                .model_response(ModelResponse::new(Vec::new()))
                .processed_response(quiet_response())
                .session_step_items(Vec::new()),
        ),
        (
            "session_step_items",
            SingleStepResult::builder()
                .model_response(ModelResponse::new(Vec::new()))
                .processed_response(quiet_response())
                .next_step(NextStep::RunAgain),
        ),
    ] {
        let error = builder.build().unwrap_err();
        assert!(
            error.to_string().contains(label),
            "缺 {label} 时的报错应当点名它，实际是：{error}"
        );
    }
}

#[test]
fn test_single_step_result_02() {
    let error = SingleStepResult::builder()
        .model_response(ModelResponse::new(Vec::new()))
        .processed_response(quiet_response())
        .next_step(NextStep::RunAgain)
        .build()
        .unwrap_err();
    // The moment this defaults to `new_step_items`, a filtered record has never reached the session.
    assert!(error.to_string().contains("no default"));
}

#[test]
fn test_single_step_result_03() {
    let error = settled()
        .new_step_items(vec![
            message("msg-1", "完事了"),
            message("msg-2", "还有一句"),
        ])
        .session_step_items(vec![message("msg-1", "完事了")])
        .build()
        .unwrap_err();
    assert!(error.to_string().contains("msg-2"));

    // The other direction is allowed: the session keeps records this turn does not show the model.
    settled()
        .new_step_items(vec![message("msg-1", "完事了")])
        .session_step_items(vec![
            message("msg-1", "完事了"),
            tool_approval("approval-1"),
        ])
        .build()
        .unwrap();

    // The same ID is not the same record; otherwise what the model was given and what the session
    // holds diverge.
    let error = settled()
        .new_step_items(vec![message("msg-1", "被过滤器改写过")])
        .build()
        .unwrap_err();
    assert!(error.to_string().contains("payload differs"));
}

#[test]
fn test_single_step_result_04() {
    let error = settled()
        .pre_step_items(vec![message("msg-1", "完事了")])
        .new_step_items(vec![message("msg-1", "完事了")])
        .build()
        .unwrap_err();
    assert!(error.to_string().contains("twice"));
}

#[test]
fn test_single_step_result_05() {
    let session = vec![mcp_approval("approval-1"), message("msg-1", "完事了")];

    for next_step in [
        NextStep::RunAgain,
        NextStep::Handoff {
            new_agent: AgentSpec::builder()
                .id(AgentId::new("reviewer"))
                .name("Reviewer")
                .build()
                .unwrap(),
        },
    ] {
        let error = SingleStepResult::builder()
            .model_response(pending_model_response())
            .processed_response(pending_response())
            .session_step_items(session.clone())
            .next_step(next_step)
            .build()
            .unwrap_err();
        assert!(error.to_string().contains("pending approvals"));
    }

    // Ending is allowed: the run is over, so nobody still owes a decision.
    SingleStepResult::builder()
        .model_response(pending_model_response())
        .processed_response(pending_response())
        .session_step_items(session)
        .next_step(NextStep::FinalOutput {
            reason: FinishReason::Cancelled,
        })
        .build()
        .unwrap();
}

#[test]
fn test_single_step_result_06() {
    let error = settled()
        .next_step(NextStep::interruption(vec![tool_approval("approval-1")]).unwrap())
        .session_step_items(vec![message("msg-1", "完事了")])
        .build()
        .unwrap_err();
    assert!(error.to_string().contains("approval-1"));

    // An approval raised during execution does **not** come from the model response — the kind
    // `needs_approval` triggers — so it is required in the session and not in `processed_response`.
    settled()
        .next_step(NextStep::interruption(vec![tool_approval("approval-1")]).unwrap())
        .session_step_items(vec![
            message("msg-1", "完事了").with_output_phase(OutputPhase::Commentary),
            tool_approval("approval-1"),
        ])
        .build()
        .unwrap();
}

#[test]
fn test_single_step_result_07() {
    let session = vec![mcp_approval("approval-1"), tool_approval("approval-2")];

    // Stopping while asking about only some of them leaves the rest waiting for a turn that never
    // comes.
    let error = SingleStepResult::builder()
        .model_response(pending_model_response())
        .processed_response(pending_response())
        .session_step_items(session.clone())
        .next_step(NextStep::interruption(vec![tool_approval("approval-2")]).unwrap())
        .build()
        .unwrap_err();
    assert!(error.to_string().contains("approval-1"));

    SingleStepResult::builder()
        .model_response(pending_model_response())
        .processed_response(pending_response())
        .session_step_items(session)
        .next_step(
            NextStep::interruption(vec![
                mcp_approval("approval-1"),
                tool_approval("approval-2"),
            ])
            .unwrap(),
        )
        .build()
        .unwrap();
}

#[test]
fn test_single_step_result_08() {
    // Nothing else catches this mismatch: usage is filed against one call, the bound actions came
    // from another, and a resume replays a third story.
    let error = settled()
        .model_response(ModelResponse::new(vec![message("msg-9", "另一次调用")]))
        .build()
        .unwrap_err();
    assert!(error.to_string().contains("different response"));

    let reordered = ProcessedResponse::builder()
        .item(message("msg-2", "第二句"))
        .item(message("msg-1", "第一句"))
        .build()
        .unwrap();
    let out_of_order = settled()
        .model_response(ModelResponse::new(vec![
            message("msg-1", "第一句"),
            message("msg-2", "第二句"),
        ]))
        .processed_response(reordered)
        .build()
        .unwrap_err();
    assert!(out_of_order.to_string().contains("same order"));

    // Matching IDs with different content is exactly the case an ID-only comparison waves through
    // while the mismatch stands.
    let rewritten = ProcessedResponse::builder()
        .item(message("msg-1", "被改写过的内容"))
        .build()
        .unwrap();
    let same_ids = settled().processed_response(rewritten).build().unwrap_err();
    assert!(same_ids.to_string().contains("different content"));
}

#[test]
fn test_single_step_result_09() {
    // `new_step_items` may be filtered, and filtered to empty the rule "what the model was given is
    // a subset of the session" holds trivially — which is what makes this the dangerous case: the
    // model did produce records and the session stored none of them.
    let error = settled()
        .new_step_items(Vec::new())
        .session_step_items(Vec::new())
        .build()
        .unwrap_err();
    assert!(error.to_string().contains("msg-1"));

    // A session record may carry more than the wire one did — provenance, session data, and the raw
    // payload exist for that — so the check is on ID and payload rather than on whole-record equality.
    settled()
        .new_step_items(Vec::new())
        .session_step_items(vec![
            message("msg-1", "完事了")
                .with_provenance(ItemProvenance::new(AgentId::new("worker")))
                .with_session_data("ui_collapsed", json!(true)),
        ])
        .build()
        .unwrap();

    // A different payload under the same ID is not a persisted copy of what the model produced.
    let error = settled()
        .new_step_items(Vec::new())
        .session_step_items(vec![message("msg-1", "被换成另一句话")])
        .build()
        .unwrap_err();
    assert!(error.to_string().contains("payload differs"));
}

#[test]
fn test_single_step_result_10() {
    // The channel is a conclusion about how this turn ended, and the provider does not decide it — it
    // is free to ask for a tool and mark the message `final` in the same breath. So settlement may
    // change that one field and store the changed record.
    SingleStepResult::builder()
        .model_response(ModelResponse::new(vec![message("msg-1", "完事了")]))
        .processed_response(quiet_response())
        .next_step(NextStep::RunAgain)
        .new_step_items(vec![item(
            "msg-1",
            RunItemKind::Message(Message::assistant("完事了", OutputPhase::Commentary)),
        )])
        .session_step_items(vec![item(
            "msg-1",
            RunItemKind::Message(Message::assistant("完事了", OutputPhase::Commentary)),
        )])
        .build()
        .unwrap();

    // That one field is the only slack: changed body text is still replacing what the model said.
    let error = SingleStepResult::builder()
        .model_response(ModelResponse::new(vec![message("msg-1", "完事了")]))
        .processed_response(quiet_response())
        .next_step(NextStep::RunAgain)
        .new_step_items(vec![item(
            "msg-1",
            RunItemKind::Message(Message::assistant(
                "被换成另一句话",
                OutputPhase::Commentary,
            )),
        )])
        .session_step_items(vec![item(
            "msg-1",
            RunItemKind::Message(Message::assistant(
                "被换成另一句话",
                OutputPhase::Commentary,
            )),
        )])
        .build()
        .unwrap_err();
    assert!(error.to_string().contains("payload differs"));
}

#[test]
fn test_single_step_result_11() {
    // On a terminal turn the one assistant message is the delivery; storing it as commentary makes
    // `final_message()` answer `None` even though `NextStep` already said the turn ended.
    let error = settled()
        .new_step_items(Vec::new())
        .session_step_items(vec![item(
            "msg-1",
            RunItemKind::Message(Message::assistant("完事了", OutputPhase::Commentary)),
        )])
        .build()
        .unwrap_err();
    assert!(error.to_string().contains("requires `final`"));

    // The other way round, a turn that is still going may never promote the model's guess to a
    // delivery.
    let error = SingleStepResult::builder()
        .model_response(ModelResponse::new(vec![message("msg-1", "完事了")]))
        .processed_response(quiet_response())
        .next_step(NextStep::RunAgain)
        .new_step_items(vec![message("msg-1", "完事了")])
        .session_step_items(vec![message("msg-1", "完事了")])
        .build()
        .unwrap_err();
    assert!(error.to_string().contains("requires `commentary`"));

    // An assistant message with no channel at all fails too: one of the two channels is **required**,
    // "unmarked" is not a third state, and neither the UI nor `final_message()` has a place for it.
    let error = settled()
        .new_step_items(Vec::new())
        .session_step_items(vec![item(
            "msg-1",
            RunItemKind::Message(Message::text(MessageRole::Assistant, "完事了")),
        )])
        .build()
        .unwrap_err();
    assert!(error.to_string().contains("channel `none`"));
}

#[test]
fn test_single_step_result_12() {
    // The rule is derived in one place: `resolve_output_phases` and the gate inside `build()` read the
    // same module. This test is the executable form of that sentence — what the producer emits has to
    // pass the gate unchanged, and the day the two evolve apart it goes red here.
    let narration = message("msg-1", "先说明思路");
    let delivery = message("msg-2", "最后交付");
    let output = message("call-1.output", "工具结果");
    let processed = ProcessedResponse::builder()
        .item(narration.clone())
        .item(delivery.clone())
        .build()
        .unwrap();

    for next_step in [
        NextStep::FinalOutput {
            reason: FinishReason::Final,
        },
        NextStep::RunAgain,
        NextStep::Handoff {
            new_agent: AgentSpec::builder()
                .id(AgentId::new("reviewer"))
                .name("Reviewer")
                .build()
                .unwrap(),
        },
    ] {
        let resolved = ra_core::step::resolve_output_phases(
            vec![narration.clone(), delivery.clone(), output.clone()],
            &next_step,
        );
        SingleStepResult::builder()
            .model_response(ModelResponse::new(vec![
                narration.clone(),
                delivery.clone(),
            ]))
            .processed_response(processed.clone())
            .next_step(next_step)
            .new_step_items(resolved.clone())
            .session_step_items(resolved)
            .build()
            .unwrap();
    }
}

#[test]
fn test_single_step_result_13() {
    let first = message("msg-1", "先说明思路");
    let last = message("msg-2", "最后交付");
    let processed = ProcessedResponse::builder()
        .item(first.clone())
        .item(last.clone())
        .build()
        .unwrap();
    let resolved = vec![
        first.clone().with_output_phase(OutputPhase::Commentary),
        last.clone(),
    ];

    SingleStepResult::builder()
        .model_response(ModelResponse::new(vec![first.clone(), last.clone()]))
        .processed_response(processed.clone())
        .next_step(NextStep::FinalOutput {
            reason: FinishReason::Final,
        })
        .new_step_items(resolved.clone())
        .session_step_items(resolved)
        .build()
        .unwrap();

    // One `ItemId` may not carry a different phase in the carried list and in the session either.
    let error = SingleStepResult::builder()
        .model_response(ModelResponse::new(vec![first.clone(), last.clone()]))
        .processed_response(processed)
        .next_step(NextStep::FinalOutput {
            reason: FinishReason::Final,
        })
        .new_step_items(vec![first.clone(), last.clone()])
        .session_step_items(vec![first.with_output_phase(OutputPhase::Commentary), last])
        .build()
        .unwrap_err();
    assert!(error.to_string().contains("payload differs"));
}

#[test]
fn test_single_step_result_14() {
    // `NextStep::Interruption` is deliberately constructible directly, since settlement lives inside
    // the framework, so that constructor is a convention rather than a gate. The gate is here: with a
    // non-approval item mixed in, the run waits forever on a decision nobody was asked for.
    let error = settled()
        .next_step(NextStep::Interruption {
            items: vec![message("msg-1", "完事了")],
        })
        .build()
        .unwrap_err();
    assert!(error.to_string().contains("message"));
    assert!(error.to_string().contains("msg-1"));

    let error = settled()
        .next_step(NextStep::Interruption { items: Vec::new() })
        .build()
        .unwrap_err();
    assert!(error.to_string().contains("at least one item"));
}

#[test]
fn test_single_step_result_15() {
    let error = settled()
        .pre_step_items(vec![message("old-1", "上一轮"), message("old-1", "又一份")])
        .build()
        .unwrap_err();
    assert!(error.to_string().contains("pre_step_items"));

    let error = settled()
        .new_step_items(vec![message("msg-1", "完事了"), message("msg-1", "又一份")])
        .build()
        .unwrap_err();
    assert!(error.to_string().contains("new_step_items"));

    let error = settled()
        .session_step_items(vec![message("msg-1", "完事了"), message("msg-1", "又一份")])
        .build()
        .unwrap_err();
    assert!(error.to_string().contains("session_step_items"));

    // A filtered-out model item still may not let this turn overwrite an earlier record of the same
    // ID.
    let error = settled()
        .pre_step_items(vec![message("msg-1", "上一轮")])
        .new_step_items(Vec::new())
        .build()
        .unwrap_err();
    assert!(error.to_string().contains("two turns"));
}

#[test]
fn test_single_step_result_16() {
    let error = settled()
        .nested_history_owned_items(vec![ItemId::new("ghost")])
        .build()
        .unwrap_err();
    assert!(error.to_string().contains("ghost"));

    let result = settled()
        .session_step_items(vec![
            message("msg-1", "完事了").with_output_phase(OutputPhase::Commentary),
            message("nested-1", "子 run"),
        ])
        .nested_history_owned_items(vec![ItemId::new("nested-1")])
        .build()
        .unwrap();
    assert_eq!(
        result.nested_history_owned_items(),
        [ItemId::new("nested-1")]
    );
}

#[test]
fn test_single_step_result_17() {
    let result = settled()
        .original_input(vec![ModelInputItem::Message(Message::user("开始"))])
        .pre_step_items(vec![message("msg-0", "上一轮")])
        .new_step_items(vec![message("msg-1", "完事了")])
        .build()
        .unwrap();

    let ids = result
        .generated_items()
        .map(|item| item.id().as_str().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(ids, ["msg-0", "msg-1"]);
    assert_eq!(result.original_input().len(), 1);
    assert_eq!(result.model_response().output().len(), 1);
    // Resuming from an interruption recovers the actions this turn bound rather than guessing at the
    // model's intent again.
    assert!(!result.processed_response().has_interruptions());
    assert!(matches!(
        result.next_step(),
        NextStep::FinalOutput {
            reason: FinishReason::Final
        }
    ));
    let _ = Arc::new(result);
}
