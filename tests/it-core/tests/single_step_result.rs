//! R3-3 contracts for the single product of one settled turn.

use std::sync::Arc;

use ra_core::{
    agent::{AgentId, AgentSpec},
    finish::FinishReason,
    item::{
        CallId, ItemId, ItemProvenance, McpApprovalRequest, Message, ModelInputItem, ModelResponse,
        OutputPhase, RunItem, RunItemKind, ToolApproval,
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
fn 四份必需事实缺一个就不算结算完() {
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
fn 会话项没有默认值因为默认值只会静默丢历史() {
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
fn 送给模型的项必须是存进会话那份的子集() {
    let error = settled()
        .new_step_items(vec![message("msg-1", "完事了"), message("msg-2", "还有一句")])
        .session_step_items(vec![message("msg-1", "完事了")])
        .build()
        .unwrap_err();
    assert!(error.to_string().contains("msg-2"));

    // 反过来是允许的：会话保留了模型这轮看不到的完整记录。
    settled()
        .new_step_items(vec![message("msg-1", "完事了")])
        .session_step_items(vec![message("msg-1", "完事了"), message("msg-2", "内部记录")])
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
fn 前序项与本轮项不许重叠() {
    let error = settled()
        .pre_step_items(vec![message("msg-1", "完事了")])
        .new_step_items(vec![message("msg-1", "完事了")])
        .build()
        .unwrap_err();
    assert!(error.to_string().contains("twice"));
}

#[test]
fn 有待审批时不允许settle成继续跑的状态() {
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
fn 中断项必须在会话里否则恢复时永远答不上() {
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
        .session_step_items(vec![message("msg-1", "完事了"), tool_approval("approval-1")])
        .build()
        .unwrap();
}

#[test]
fn 响应里提出的待决项一个都不能漏问() {
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
            NextStep::interruption(vec![mcp_approval("approval-1"), tool_approval("approval-2")])
                .unwrap(),
        )
        .build()
        .unwrap();
}

#[test]
fn 分类结果必须是这条模型响应的分类() {
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
    let same_ids = settled()
        .processed_response(rewritten)
        .build()
        .unwrap_err();
    assert!(same_ids.to_string().contains("different content"));
}

#[test]
fn 模型说过的话必须进会话哪怕这轮不往下带() {
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
fn 绕过校验构造器的中断形状在结算时仍然过不去() {
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
fn 结算中的_item_id_不能重复或跨轮复用() {
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
fn 嵌套归属只记_id_且必须指向真实存在的记录() {
    let error = settled()
        .nested_history_owned_items(vec![ItemId::new("ghost")])
        .build()
        .unwrap_err();
    assert!(error.to_string().contains("ghost"));

    let result = settled()
        .session_step_items(vec![message("msg-1", "完事了"), message("nested-1", "子 run")])
        .nested_history_owned_items(vec![ItemId::new("nested-1")])
        .build()
        .unwrap();
    assert_eq!(result.nested_history_owned_items(), [ItemId::new("nested-1")]);
}

#[test]
fn generated_items_按前序在前本轮在后拼接() {
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
