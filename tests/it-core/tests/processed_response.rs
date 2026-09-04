//! R3-2 contracts for classifying one model response into bound actions.

use std::sync::Arc;

use async_trait::async_trait;
use ra_core::{
    error::Result,
    item::{
        AgentId, CallId, ItemId, McpApprovalRequest, Message, OutputPhase, RunItem, RunItemKind,
        ToolApproval, ToolCall, ToolCallOutput,
    },
    step::{
        ProcessedResponse, ToolNotFound, ToolRunApproval, ToolRunFunction, ToolRunHandoff, ToolUse,
    },
    tool::{Tool, ToolContext, ToolNamespace, ToolOrigin, ToolOutput, ToolSchema},
};
use serde_json::json;

struct StubTool {
    origin: ToolOrigin,
    schema: ToolSchema,
}

#[async_trait]
impl Tool for StubTool {
    fn origin(&self) -> &ToolOrigin {
        &self.origin
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    async fn call(&self, _context: ToolContext<'_>) -> Result<ToolOutput> {
        Ok(ToolOutput::text("unused"))
    }
}

fn schema(name: &str) -> ToolSchema {
    ToolSchema::new(
        name,
        json!({
            "type": "object",
            "properties": {},
            "required": [],
            "additionalProperties": false
        }),
    )
    .unwrap()
}

fn bare_tool(name: &str) -> Arc<dyn Tool> {
    Arc::new(StubTool {
        origin: ToolOrigin::new(name).unwrap(),
        schema: schema(name),
    })
}

fn namespaced_tool(namespace: &str, name: &str) -> Arc<dyn Tool> {
    Arc::new(StubTool {
        origin: ToolOrigin::namespaced(ToolNamespace::new(namespace).unwrap(), name).unwrap(),
        schema: schema(name),
    })
}

fn item(id: &str, kind: RunItemKind) -> RunItem {
    RunItem::new(ItemId::new(id), kind)
}

fn tool_call(id: &str, call_id: &str, name: &str) -> RunItem {
    item(
        id,
        RunItemKind::ToolCall(ToolCall::new(
            CallId::new(call_id),
            name,
            json!({ "path": "a.txt" }),
        )),
    )
}

fn mcp_approval(id: &str, request_id: &str) -> RunItem {
    item(
        id,
        RunItemKind::McpApprovalRequest(McpApprovalRequest::new(
            request_id,
            "docs",
            "search",
            json!({ "query": "x" }),
        )),
    )
}

#[test]
fn test_processed_response_01() {
    let processed = ProcessedResponse::builder()
        .item(item(
            "msg-1",
            RunItemKind::Message(Message::assistant("先说一句", OutputPhase::Commentary)),
        ))
        .function(
            tool_call("call-item-1", "call-1", "write_file"),
            bare_tool("write_file"),
        )
        .unwrap()
        .handoff(
            tool_call("call-item-2", "call-2", "transfer_to_reviewer"),
            AgentId::new("reviewer"),
        )
        .unwrap()
        .mcp_approval(mcp_approval("approval-1", "req-1"))
        .unwrap()
        .tool_not_found(tool_call("call-item-3", "call-3", "vanished"))
        .unwrap()
        .build()
        .unwrap();

    // `new_items` is the complete record in the response's own order; the actions are a typed view
    // of it rather than a second copy.
    let ids = processed
        .new_items()
        .iter()
        .map(|item| item.id().as_str().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(
        ids,
        [
            "msg-1",
            "call-item-1",
            "call-item-2",
            "approval-1",
            "call-item-3"
        ]
    );

    assert_eq!(processed.functions().len(), 1);
    assert_eq!(processed.functions()[0].item_id().as_str(), "call-item-1");
    assert_eq!(processed.functions()[0].call_id().as_str(), "call-1");
    assert_eq!(
        processed.functions()[0].tool().origin().name(),
        "write_file"
    );

    assert_eq!(processed.handoffs().len(), 1);
    assert_eq!(processed.handoffs()[0].target_agent().as_str(), "reviewer");
    assert_eq!(processed.mcp_approval_requests().len(), 1);
    assert_eq!(processed.tools_not_found().len(), 1);
    assert_eq!(processed.tools_not_found()[0].name(), "vanished");
}

#[test]
fn test_processed_response_02() {
    let processed = ProcessedResponse::builder()
        .handoff(
            tool_call("call-item-1", "call-1", "transfer_to_reviewer"),
            AgentId::new("reviewer"),
        )
        .unwrap()
        .build()
        .unwrap();

    let handoff = &processed.handoffs()[0];
    // A handoff is an ordinary function call on the wire, and the next turn cannot replay it without
    // that name.
    assert_eq!(handoff.call().tool_name(), Some("transfer_to_reviewer"));
    assert_eq!(handoff.call_id().as_str(), "call-1");
    // The record itself is not rewritten: what the session stores is still the item the provider sent.
    assert!(matches!(
        processed.new_items()[0].kind(),
        RunItemKind::ToolCall(_)
    ));
}

#[test]
fn test_processed_response_03() {
    let typed = item(
        "call-item-1",
        RunItemKind::HandoffCall(ra_core::item::HandoffCall::new(
            CallId::new("call-1"),
            AgentId::new("reviewer"),
            json!({}),
        )),
    );

    let ok = ProcessedResponse::builder()
        .handoff(typed.clone(), AgentId::new("reviewer"))
        .unwrap()
        .build()
        .unwrap();
    assert_eq!(ok.handoffs()[0].target_agent().as_str(), "reviewer");

    let error = ProcessedResponse::builder()
        .handoff(typed, AgentId::new("planner"))
        .unwrap_err();
    assert!(error.to_string().contains("reviewer"));
    assert!(error.to_string().contains("planner"));
}

#[test]
fn test_processed_response_04() {
    // A name that does not match means settlement runs an implementation the model never asked for,
    // and then reports success all the same.
    let error = ProcessedResponse::builder()
        .function(
            tool_call("call-item-1", "call-1", "write_file"),
            bare_tool("read_file"),
        )
        .unwrap_err();
    assert!(error.to_string().contains("write_file"));
    assert!(error.to_string().contains("read_file"));

    let wrong_kind = ProcessedResponse::builder()
        .function(
            item(
                "msg-1",
                RunItemKind::Message(Message::assistant("hi", OutputPhase::Final)),
            ),
            bare_tool("write_file"),
        )
        .unwrap_err();
    assert!(wrong_kind.to_string().contains("message"));
}

#[test]
fn test_processed_response_05() {
    // Two actions each return one output, so the provider either rejects the whole request or keeps
    // the wrong one.
    let error = ProcessedResponse::builder()
        .function(
            tool_call("call-item-1", "same-call", "write_file"),
            bare_tool("write_file"),
        )
        .unwrap()
        .tool_not_found(tool_call("call-item-2", "same-call", "vanished"))
        .unwrap()
        .build()
        .unwrap_err();
    assert!(error.to_string().contains("same-call"));

    let duplicate_item = ProcessedResponse::builder()
        .item(item("msg-1", RunItemKind::Message(Message::user("hi"))))
        .item(item(
            "msg-1",
            RunItemKind::Message(Message::user("hi again")),
        ))
        .build()
        .unwrap_err();
    assert!(duplicate_item.to_string().contains("msg-1"));
}

#[test]
fn test_processed_response_06() {
    // A call admitted through `item()` makes `has_tools_or_approvals_to_run()` answer "nothing to
    // do" while the response still holds a call nobody will answer — and the symptom only shows up
    // on the next request.
    for (label, unclassified) in [
        (
            "tool_call",
            tool_call("call-item-1", "call-1", "write_file"),
        ),
        (
            "handoff_call",
            item(
                "call-item-1",
                RunItemKind::HandoffCall(ra_core::item::HandoffCall::new(
                    CallId::new("call-1"),
                    AgentId::new("reviewer"),
                    json!({}),
                )),
            ),
        ),
        ("mcp_approval_request", mcp_approval("approval-1", "req-1")),
    ] {
        let error = ProcessedResponse::builder()
            .item(unclassified)
            .build()
            .unwrap_err();
        assert!(
            error.to_string().contains(label),
            "{label} 应当被要求分类，实际报错：{error}"
        );
    }

    // An answered output and a control-plane record owe nobody anything, so `item()` is right for
    // them.
    ProcessedResponse::builder()
        .item(item(
            "out-1",
            RunItemKind::ToolCallOutput(ToolCallOutput::new(CallId::new("call-1"), json!("ok"))),
        ))
        .item(item(
            "approval-2",
            RunItemKind::ToolApproval(ToolApproval::new(
                CallId::new("call-9"),
                "write_file",
                json!({}),
            )),
        ))
        .build()
        .unwrap();
}

#[test]
fn test_processed_response_07() {
    let only_missing = ProcessedResponse::builder()
        .tool_not_found(tool_call("call-item-1", "call-1", "vanished"))
        .unwrap()
        .build()
        .unwrap();

    // The reference implementation leaves not-found out of this predicate. Then `false` reads as
    // "nothing to do", and the next request goes out carrying a tool call with no result.
    assert!(only_missing.has_tools_or_approvals_to_run());
    assert!(!only_missing.has_interruptions());

    let nothing = ProcessedResponse::builder()
        .item(item(
            "msg-1",
            RunItemKind::Message(Message::assistant("完事了", OutputPhase::Final)),
        ))
        .build()
        .unwrap();
    assert!(!nothing.has_tools_or_approvals_to_run());
    assert!(!nothing.has_interruptions());
}

#[test]
fn test_processed_response_08() {
    let processed = ProcessedResponse::builder()
        .mcp_approval(mcp_approval("approval-1", "req-1"))
        .unwrap()
        .item(item(
            "approval-2",
            RunItemKind::ToolApproval(ToolApproval::new(
                CallId::new("call-9"),
                "write_file",
                json!({}),
            )),
        ))
        .item(item(
            "out-1",
            RunItemKind::ToolCallOutput(ToolCallOutput::new(CallId::new("call-8"), json!("ok"))),
        ))
        .build()
        .unwrap();

    assert!(processed.has_tools_or_approvals_to_run());
    assert!(processed.has_interruptions());

    // Interruptions are a projection rather than a field: `is_interruption` is the only predicate, so
    // there is no second place that could answer differently.
    let pending = processed
        .interruptions()
        .map(|item| item.id().as_str().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(pending, ["approval-1", "approval-2"]);
}

#[test]
fn test_processed_response_09() {
    let processed = ProcessedResponse::builder()
        .function(
            tool_call("call-item-1", "call-1", "search"),
            namespaced_tool("mcp.github", "search"),
        )
        .unwrap()
        .function(
            tool_call("call-item-2", "call-2", "search"),
            namespaced_tool("mcp.gitlab", "search"),
        )
        .unwrap()
        .function(
            tool_call("call-item-3", "call-3", "search"),
            namespaced_tool("mcp.github", "search"),
        )
        .unwrap()
        .handoff(
            tool_call("call-item-4", "call-4", "transfer_to_reviewer"),
            AgentId::new("reviewer"),
        )
        .unwrap()
        .mcp_approval(mcp_approval("approval-1", "req-1"))
        .unwrap()
        .tool_not_found(tool_call("call-item-5", "call-5", "vanished"))
        .unwrap()
        .build()
        .unwrap();

    let used = processed.tools_used();
    // The two `search` entries come from different namespaces and are two identities; the third call
    // is the one that shares an identity with the first.
    assert_eq!(used.len(), 5);
    assert_eq!(
        used.iter()
            .filter(|use_| matches!(use_, ToolUse::Tool(_)))
            .count(),
        2
    );
    assert!(used.contains(&ToolUse::Handoff(AgentId::new("reviewer"))));
    assert!(used.contains(&ToolUse::Mcp {
        server: "docs".to_owned(),
        tool_name: "search".to_owned(),
    }));
    assert!(used.contains(&ToolUse::Unresolved("vanished".to_owned())));

    // Order follows the response, and de-duplication keeps the position of the first occurrence.
    let ToolUse::Tool(first) = &used[0] else {
        panic!("第一条应当是本地工具");
    };
    assert_eq!(
        first.namespace().map(ra_core::tool::ToolNamespace::as_str),
        Some("mcp.github")
    );

    // The identity an action reports has to be the one the projection files it under. The breaker
    // looks up a streak by `identity()` while the tracker files by the projection, so the moment the
    // two count separately the lookup asks about an identity nothing ever filed — reading zero
    // forever, with the loop still going round and no assertion failing.
    let mut from_actions = Vec::new();
    from_actions.extend(processed.functions().iter().map(ToolRunFunction::identity));
    from_actions.extend(processed.handoffs().iter().map(ToolRunHandoff::identity));
    from_actions.extend(
        processed
            .mcp_approval_requests()
            .iter()
            .map(ToolRunApproval::identity),
    );
    from_actions.extend(
        processed
            .tools_not_found()
            .iter()
            .map(ToolNotFound::identity),
    );
    from_actions.sort();
    from_actions.dedup();

    let mut from_projection = used;
    from_projection.sort();
    assert_eq!(from_actions, from_projection);
}
