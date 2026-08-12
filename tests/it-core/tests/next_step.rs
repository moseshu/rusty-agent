//! R3-1 contracts for the four-state loop control flow.

use std::sync::Arc;

use ra_core::{
    agent::{AgentId, AgentSpec},
    finish::FinishReason,
    item::{
        CallId, ItemId, McpApprovalRequest, Message, OutputPhase, RunItem, RunItemKind,
        ToolApproval, ToolCall,
    },
    step::NextStep,
};
use serde_json::json;

fn agent(id: &str) -> Arc<AgentSpec> {
    AgentSpec::builder()
        .id(AgentId::new(id))
        .name("Worker")
        .build()
        .unwrap()
}

fn item(id: &str, kind: RunItemKind) -> RunItem {
    RunItem::new(ItemId::new(id), kind)
}

fn tool_approval(call_id: &str) -> RunItem {
    item(
        call_id,
        RunItemKind::ToolApproval(ToolApproval::new(
            CallId::new(call_id),
            "write_file",
            json!({ "path": "a.txt" }),
        )),
    )
}

fn mcp_approval(request_id: &str) -> RunItem {
    item(
        request_id,
        RunItemKind::McpApprovalRequest(McpApprovalRequest::new(
            request_id,
            "docs",
            "search",
            json!({ "query": "x" }),
        )),
    )
}

/// The whole point of the type, written as code: four arms, **no `_ =>` fallback**.
///
/// It is a compile-time assertion. Adding a fifth state has to break this function, and every
/// other site that matches, until each one says what it does about the new state.
fn summarize_step(step: &NextStep) -> String {
    match step {
        NextStep::RunAgain => "run_again".to_owned(),
        NextStep::Handoff { new_agent } => format!("handoff:{}", new_agent.id()),
        NextStep::FinalOutput { reason } => format!("final:{reason}"),
        NextStep::Interruption { items } => format!("interruption:{}", items.len()),
    }
}

#[test]
fn test_next_step_01() {
    let steps = [
        NextStep::RunAgain,
        NextStep::Handoff {
            new_agent: agent("reviewer"),
        },
        NextStep::FinalOutput {
            reason: FinishReason::Final,
        },
        NextStep::interruption(vec![tool_approval("call-1")]).unwrap(),
    ];

    let labels = steps.iter().map(summarize_step).collect::<Vec<_>>();
    assert_eq!(
        labels,
        [
            "run_again",
            "handoff:reviewer",
            "final:final",
            "interruption:1"
        ]
    );
}

#[test]
fn test_next_step_02() {
    let reviewer = agent("reviewer");
    let step = NextStep::Handoff {
        new_agent: Arc::clone(&reviewer),
    };

    let NextStep::Handoff { new_agent } = &step else {
        panic!("应当是 handoff");
    };
    // R3-12 的绑定要靠稳定身份反查，这里必须还是同一个实例而不是等价副本。
    assert!(Arc::ptr_eq(new_agent, &reviewer));
    assert_eq!(new_agent.id().as_str(), "reviewer");
}

#[test]
fn test_next_step_03() {
    // 一个变体背四种含义正是 R3-1b 要拆掉的东西：宿主对这两种的反应完全不同。
    let concluded = NextStep::FinalOutput {
        reason: FinishReason::Final,
    };
    let out_of_budget = NextStep::FinalOutput {
        reason: FinishReason::BudgetExhausted,
    };

    let NextStep::FinalOutput { reason: concluded } = concluded else {
        panic!("应当是 final output");
    };
    let NextStep::FinalOutput {
        reason: out_of_budget,
    } = out_of_budget
    else {
        panic!("应当是 final output");
    };

    assert!(concluded.is_complete());
    assert!(!out_of_budget.is_complete());
    assert!(out_of_budget.is_resumable());
}

#[test]
fn test_next_step_04() {
    let step = NextStep::interruption(vec![tool_approval("call-1"), mcp_approval("req-1")]).unwrap();
    let NextStep::Interruption { items } = step else {
        panic!("应当是 interruption");
    };
    assert_eq!(items.len(), 2);
    assert!(items.iter().all(|item| item.kind().is_interruption()));
}

#[test]
fn test_next_step_05() {
    // 混进一条非审批项，run 会永远等一个没人被问到的决定，而症状（挂住）离病因很远。
    let error = NextStep::interruption(vec![
        tool_approval("call-1"),
        item(
            "msg-1",
            RunItemKind::Message(Message::assistant("我先说一句", OutputPhase::Commentary)),
        ),
    ])
    .unwrap_err();
    assert!(error.to_string().contains("message"));

    let empty = NextStep::interruption(Vec::new()).unwrap_err();
    assert!(empty.to_string().contains("at least one item"));
}

#[test]
fn test_next_step_06() {
    assert!(
        RunItemKind::ToolApproval(ToolApproval::new(
            CallId::new("call-1"),
            "write_file",
            json!({})
        ))
        .is_interruption()
    );
    assert!(
        RunItemKind::McpApprovalRequest(McpApprovalRequest::new("req-1", "docs", "search", json!({})))
            .is_interruption()
    );

    // 已经答完的那条不再是待决项，否则 resume 会把同一个审批再问一遍。
    assert!(!RunItemKind::Message(Message::user("hi")).is_interruption());
    assert!(
        !RunItemKind::ToolCall(ToolCall::new(
            CallId::new("call-1"),
            "write_file",
            json!({})
        ))
        .is_interruption()
    );
}
