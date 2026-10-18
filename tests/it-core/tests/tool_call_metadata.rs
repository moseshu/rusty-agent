//! Run-item display metadata persists independently of replayable tool payloads.

use std::sync::Arc;

use async_trait::async_trait;
use ra_core::{
    error::Result,
    item::{CallId, ItemId, McpCall, ModelInputItem, RunItem, RunItemKind, ToolCall},
    state::{RunId, RunState},
    step::ProcessedResponse,
    tool::{Tool, ToolContext, ToolOrigin, ToolOutput, ToolSchema},
};
use serde_json::json;

#[test]
fn local_and_hosted_display_metadata_survive_run_state_roundtrips() {
    let items = [
        RunItem::new(
            ItemId::new("local"),
            RunItemKind::ToolCall(ToolCall::new(CallId::new("call"), "search", json!({}))),
        ),
        RunItem::new(
            ItemId::new("hosted"),
            RunItemKind::McpCall(McpCall::new("hosted", "docs", "search", "{}")),
        ),
    ];
    let mut state = RunState::start(RunId::new("metadata"));
    state
        .begin_segment(ra_core::agent::AgentId::new("agent"), vec![])
        .unwrap();
    state.record_generated_items(items.into_iter().map(|item| {
        item.with_tool_call_metadata(Some("Description".into()), Some("Display title".into()))
    }));
    let restored: RunState = serde_json::from_value(serde_json::to_value(state).unwrap()).unwrap();
    for item in restored.generated_items() {
        assert_eq!(item.description(), Some("Description"));
        assert_eq!(item.title(), Some("Display title"));
        let stored = serde_json::to_value(item).unwrap();
        assert_eq!(stored["title"], "Display title");
        assert_eq!(stored["description"], "Description");
        let projected = serde_json::to_value(item.to_model_input().unwrap()).unwrap();
        assert!(!projected.to_string().contains("Display title"));
        assert!(!projected.to_string().contains("Description"));
    }
}

struct DisplayTool {
    origin: ToolOrigin,
    schema: ToolSchema,
}

#[async_trait]
impl Tool for DisplayTool {
    fn origin(&self) -> &ToolOrigin {
        &self.origin
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    fn title(&self) -> Option<&str> {
        Some("Display title")
    }

    async fn call(&self, _: ToolContext<'_>) -> Result<ToolOutput> {
        unreachable!("classification does not execute tools")
    }
}

#[test]
fn custom_call_items_do_not_inherit_the_tool_declarations_display_metadata() {
    let tool = Arc::new(DisplayTool {
        origin: ToolOrigin::new("raw_editor").unwrap(),
        schema: ToolSchema::custom("raw_editor", None)
            .unwrap()
            .with_description("Edit raw text"),
    });
    let call = ToolCall::custom(CallId::new("edit"), "raw_editor", "-old\n+new\n");
    let processed = ProcessedResponse::builder()
        .function(
            RunItem::new(ItemId::new("edit"), RunItemKind::ToolCall(call.clone())),
            tool.clone(),
        )
        .unwrap()
        .build()
        .unwrap();
    let item = &processed.new_items()[0];
    assert_eq!((item.description(), item.title()), (None, None));
    assert_eq!(item.to_model_input(), Some(ModelInputItem::ToolCall(call)));
    assert_eq!(tool.model_definition().description(), Some("Edit raw text"));
}

#[test]
fn legacy_records_default_to_absent_metadata_and_explicit_empty_values_survive() {
    let item = RunItem::new(
        ItemId::new("legacy"),
        RunItemKind::ToolCall(ToolCall::new(CallId::new("call"), "search", json!({}))),
    );
    let value = serde_json::to_value(&item).unwrap();
    assert!(value.get("description").is_none());
    assert!(value.get("title").is_none());
    let restored: RunItem = serde_json::from_value(value).unwrap();
    assert_eq!(restored.description(), None);
    assert_eq!(restored.title(), None);
    let item = item.with_tool_call_metadata(Some(String::new()), Some(String::new()));
    let restored: RunItem = serde_json::from_value(serde_json::to_value(item).unwrap()).unwrap();
    assert_eq!(restored.description(), Some(""));
    assert_eq!(restored.title(), Some(""));
}

#[test]
fn hosted_invocations_are_passive_records_with_no_local_call_output_pairing() {
    let call: McpCall = serde_json::from_value(json!({
        "schema_version":1, "id":"hosted", "server":"docs", "name":"search",
        "arguments":"provider-owned string", "output":"result", "error":"warning",
        "status":"completed", "approval_request_id":"approval"
    }))
    .unwrap();
    assert_eq!(call.arguments(), "provider-owned string");
    assert_eq!(call.output(), Some("result"));
    assert_eq!(call.error(), Some("warning"));
    assert_eq!(call.unknown().get("status"), Some(&json!("completed")));
    let normalized = ra_core::item::InputItemNormalizer::new()
        .normalize_model_items(&[
            ModelInputItem::McpCall(call.clone()),
            ModelInputItem::McpCall(call.clone()),
        ])
        .unwrap()
        .into_items();
    assert_eq!(normalized, [ModelInputItem::McpCall(call.clone())]);
    let kind = RunItemKind::McpCall(call.clone());
    assert!(!kind.requires_action_binding());
    assert!(!kind.is_interruption());
    let item = RunItem::new(ItemId::new("hosted"), kind);
    assert_eq!(item.call_id(), None);
    assert_eq!(item.to_model_input(), Some(ModelInputItem::McpCall(call)));
}
