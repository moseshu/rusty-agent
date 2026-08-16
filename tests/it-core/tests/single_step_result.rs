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

/// 与 `pending_response()` 同源的那条模型响应。两者必须是同一批记录，否则 `build()`
/// 会先在一致性那一条上报错。
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
    // 默认成 new_step_items 的那一刻，过滤掉的记录就再也没进过会话。
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

    // 反过来是允许的：会话保留了模型这轮看不到的完整记录。
    settled()
        .new_step_items(vec![message("msg-1", "完事了")])
        .session_step_items(vec![
            message("msg-1", "完事了"),
            tool_approval("approval-1"),
        ])
        .build()
        .unwrap();

    // 相同 ID 不等于同一条记录；否则给模型的内容和会话里的历史会分叉。
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

    // 结束是允许的：run 已经完了，没有谁还欠一个决定。
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

    // 执行阶段生成的审批项**不**来自模型响应（`needs_approval` 触发的那种），
    // 所以只要求会话里有它，不要求它出现在 `processed_response` 里。
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

    // 停下来却只问其中一部分，剩下那条要等一个永远不会来的轮次。
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
    // 别的都拦不住这种错配：用量记的是一次调用，绑定的动作来自另一次，
    // resume 重放的又是第三个故事。
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

    // ID 对得上、内容对不上，正是只比 ID 会放行而错配照旧的那一格。
    let rewritten = ProcessedResponse::builder()
        .item(message("msg-1", "被改写过的内容"))
        .build()
        .unwrap();
    let same_ids = settled().processed_response(rewritten).build().unwrap_err();
    assert!(same_ids.to_string().contains("different content"));
}

#[test]
fn test_single_step_result_09() {
    // `new_step_items` 允许被过滤，过滤成空时「送模型的项是会话的子集」那条
    // 恒成立——真正危险的正是这一格：模型确实产出了记录，会话一条没存。
    let error = settled()
        .new_step_items(Vec::new())
        .session_step_items(Vec::new())
        .build()
        .unwrap_err();
    assert!(error.to_string().contains("msg-1"));

    // 会话记录可以比线上那份更厚（provenance / session_data / raw payload 就是干这个的），
    // 所以校验 ID 与 payload，而不是按整条 record 相等。
    settled()
        .new_step_items(Vec::new())
        .session_step_items(vec![
            message("msg-1", "完事了")
                .with_provenance(ItemProvenance::new(AgentId::new("worker")))
                .with_session_data("ui_collapsed", json!(true)),
        ])
        .build()
        .unwrap();

    // 但同 ID 的另一条 payload 不是对模型输出的持久化。
    let error = settled()
        .new_step_items(Vec::new())
        .session_step_items(vec![message("msg-1", "被换成另一句话")])
        .build()
        .unwrap_err();
    assert!(error.to_string().contains("payload differs"));
}

#[test]
fn test_single_step_result_10() {
    // R3-10：通道是「这一轮怎么收的场」的结论，provider 说了不算——它完全可以一边要工具
    // 一边把消息标成 final。所以结算改这一个字段并把改过的那份存下去是允许的。
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

    // 松的只有这一格：正文变了照样是把模型说过的话换掉。
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
    // 终态轮唯一的 assistant 消息是交付；把它存成 commentary 会让 `final_message()` 说
    // `None`，即使 `NextStep` 已经说这轮结束了。
    let error = settled()
        .new_step_items(Vec::new())
        .session_step_items(vec![item(
            "msg-1",
            RunItemKind::Message(Message::assistant("完事了", OutputPhase::Commentary)),
        )])
        .build()
        .unwrap_err();
    assert!(error.to_string().contains("requires `final`"));

    // 反过来，尚要继续的轮绝不能把模型的预判提前当成交付。
    let error = SingleStepResult::builder()
        .model_response(ModelResponse::new(vec![message("msg-1", "完事了")]))
        .processed_response(quiet_response())
        .next_step(NextStep::RunAgain)
        .new_step_items(vec![message("msg-1", "完事了")])
        .session_step_items(vec![message("msg-1", "完事了")])
        .build()
        .unwrap_err();
    assert!(error.to_string().contains("requires `commentary`"));

    // 一条都没定过通道的 assistant 消息同样过不去：R3-10 的两个通道是**必选一个**，
    // 「没标」不是第三种状态，UI 与 `final_message()` 都没有它的位置。
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
    // 规则只有一处推导：`resolve_output_phases` 与 `build()` 里的闸门读同一个模块。这条
    // 测试是那句话的可执行形式——生产者的产物必须原样通过闸门，两边一旦分头演化就在这里红。
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

    // 同一个 ItemId 在 carried 与 session 里也不能各自有一份不同的 phase。
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
    // `NextStep::Interruption` 刻意可以直接构造（结算在框架内部），所以那个构造器
    // 是约定不是闸门。闸门放在这里：混进一条非审批项，run 会永远等一个没人被问到的决定。
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

    // 模型产物被过滤掉也不能让这一轮覆盖以前那条同 ID 的历史。
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
    // 中断恢复要靠它拿回本轮绑定好的动作，而不是重新猜模型的意思。
    assert!(!result.processed_response().has_interruptions());
    assert!(matches!(
        result.next_step(),
        NextStep::FinalOutput {
            reason: FinishReason::Final
        }
    ));
    let _ = Arc::new(result);
}
