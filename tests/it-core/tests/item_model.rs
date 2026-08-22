//! R1-1 contract tests for the item model, authoritative session records, and model-input views.

use ra_core::{
    item::{
        AgentId, CallId, Compaction, HandoffCall, HandoffOutput, ItemId, ItemProvenance,
        McpApprovalRequest, McpApprovalResponse, McpListTools, McpTool, Message, MessageRole,
        ModelInputItem, ModelResponse, OutputPhase, RawProviderItem, Reasoning, RunItem,
        RunItemKind, ToolApproval, ToolCall, ToolCallOutput,
    },
    usage::{RequestUsage, Usage},
};
use serde_json::{Value, json};

fn item(id: &str, kind: RunItemKind) -> RunItem {
    RunItem::new(ItemId::new(id), kind)
}

fn all_items() -> Vec<RunItem> {
    vec![
        item(
            "item-message",
            RunItemKind::Message(Message::assistant("完成", OutputPhase::Final)),
        ),
        item(
            "item-reasoning",
            RunItemKind::Reasoning(
                Reasoning::new()
                    .with_id("reasoning-1")
                    .with_summary(vec!["先检查".into()])
                    .with_content(vec!["读取配置".into()])
                    .with_encrypted_content("signature-data")
                    .with_provider_data(json!({"thinking_blocks": [{"signature": "s1"}]})),
            ),
        ),
        item(
            "item-call",
            RunItemKind::ToolCall(ToolCall::new(
                CallId::new("call-1"),
                "read_file",
                json!({"path": "Cargo.toml"}),
            )),
        ),
        item(
            "item-output",
            RunItemKind::ToolCallOutput(ToolCallOutput::new(
                CallId::new("call-1"),
                json!({"text": "workspace"}),
            )),
        ),
        item(
            "item-handoff-call",
            RunItemKind::HandoffCall(HandoffCall::new(
                CallId::new("handoff-1"),
                AgentId::new("reviewer"),
                json!({"focus": "correctness"}),
            )),
        ),
        item(
            "item-handoff-output",
            RunItemKind::HandoffOutput(
                HandoffOutput::new(
                    CallId::new("handoff-1"),
                    AgentId::new("coder"),
                    AgentId::new("reviewer"),
                )
                .with_note("请检查边界"),
            ),
        ),
        item(
            "item-mcp-list",
            RunItemKind::McpListTools(McpListTools::new(
                "filesystem",
                vec![
                    McpTool::new("read_text_file", json!({"type": "object"}))
                        .with_description("读取文本"),
                ],
            )),
        ),
        item(
            "item-mcp-request",
            RunItemKind::McpApprovalRequest(McpApprovalRequest::new(
                "approval-1",
                "filesystem",
                "write_file",
                json!({"path": "a.txt"}),
            )),
        ),
        item(
            "item-mcp-response",
            RunItemKind::McpApprovalResponse(
                McpApprovalResponse::new("approval-1", false).with_reason("只读任务"),
            ),
        ),
        item(
            "item-compaction",
            RunItemKind::Compaction(Compaction::new(
                "此前已检查 workspace",
                vec![ItemId::new("item-message")],
            )),
        ),
        item(
            "item-tool-approval",
            RunItemKind::ToolApproval(
                ToolApproval::new(
                    CallId::new("call-danger"),
                    "exec_command",
                    json!({"cmd": "deploy"}),
                )
                .with_namespace("shell"),
            ),
        ),
    ]
}

#[test]
fn test_item_model_01() {
    let items = all_items();
    assert_eq!(items.len(), 11);

    for item in items {
        let json = serde_json::to_string(&item).expect("RunItem 应可序列化");
        let back: RunItem = serde_json::from_str(&json).expect("RunItem 应可反序列化");
        assert_eq!(back, item, "往返改变了 {}", item.kind().label());
    }
}

#[test]
fn test_item_model_02() {
    let items = all_items();
    let response = ModelResponse::new(items);
    let input = response.to_input_items();

    assert_eq!(response.output().len(), 11, "session 权威记录不能被过滤");
    assert_eq!(input.len(), 10, "只有 ToolApproval 属于 session 控制面");
    assert!(input.iter().all(|item| item.label() != "tool_approval"));
}

#[test]
fn test_item_model_03() {
    let item = item("item-1", RunItemKind::Message(Message::user("检查项目")))
        .with_provenance(
            ItemProvenance::new(AgentId::new("agent-secret")).with_agent_name("内部执行器"),
        )
        .with_raw_provider_item(RawProviderItem::new(
            "provider-secret",
            json!({"provider_only": true}),
        ))
        .with_session_data("ui-secret", json!({"expanded": false}));

    let stored = serde_json::to_string(&item).expect("权威项应可序列化");
    assert!(stored.contains("agent-secret"));
    assert!(stored.contains("provider-secret"));
    assert!(stored.contains("ui-secret"));

    let projected = item.to_model_input().expect("消息应可投影");
    let sent = serde_json::to_string(&projected).expect("模型输入应可序列化");
    for forbidden in [
        "item-1",
        "agent-secret",
        "内部执行器",
        "provider-secret",
        "provider_only",
        "ui-secret",
    ] {
        assert!(
            !sent.contains(forbidden),
            "模型输入泄漏了 {forbidden}: {sent}"
        );
    }
    assert!(sent.contains("检查项目"));
}

#[test]
fn test_item_model_04() {
    let call = item(
        "a",
        RunItemKind::ToolCall(ToolCall::new(
            CallId::new("same-call"),
            "read_file",
            json!({}),
        )),
    );
    let output = item(
        "完全不同的-item-id",
        RunItemKind::ToolCallOutput(ToolCallOutput::new(CallId::new("same-call"), json!("ok"))),
    );

    assert_ne!(call.id(), output.id());
    assert_eq!(call.call_id(), output.call_id());
    assert_eq!(call.call_id().map(CallId::as_str), Some("same-call"));
}

#[test]
fn test_item_model_05() {
    let reasoning = Reasoning::new()
        .with_id("r-1")
        .with_content(vec![String::new(), "visible".into()])
        .with_encrypted_content("sig-a\nsig-b")
        .with_provider_data(json!([
            {"type": "thinking", "thinking": "", "signature": "sig-a"},
            {"type": "redacted_thinking", "data": "opaque"}
        ]));
    let item = item("reasoning", RunItemKind::Reasoning(reasoning.clone()));

    let Some(ModelInputItem::Reasoning(projected)) = item.to_model_input() else {
        panic!("reasoning 应投影为 reasoning");
    };
    assert_eq!(projected, reasoning);
    assert_eq!(projected.encrypted_content(), Some("sig-a\nsig-b"));
    assert_eq!(
        projected
            .provider_data()
            .and_then(Value::as_array)
            .map(Vec::len),
        Some(2)
    );
}

#[test]
fn test_item_model_06() {
    let response = ModelResponse::new(vec![item(
        "message",
        RunItemKind::Message(Message::assistant("结果", OutputPhase::Final)),
    )])
    .with_usage(Usage::from_request(
        RequestUsage::new(120, 30)
            .with_cached_input_tokens(80)
            .with_reasoning_tokens(10),
    ))
    .with_response_id("response-1")
    .with_request_id("request-1");

    assert_eq!(response.response_id(), Some("response-1"));
    assert_eq!(response.request_id(), Some("request-1"));
    assert_eq!(response.usage().total_tokens(), 150);
    assert_eq!(response.usage().cached_input_tokens(), 80);
    assert_eq!(response.to_input_items().len(), 1);
}

#[test]
fn test_item_model_07() {
    let commentary = Message::assistant("正在检查", OutputPhase::Commentary);
    let final_message = Message::assistant("检查完成", OutputPhase::Final);

    assert_eq!(commentary.role(), MessageRole::Assistant);
    assert_eq!(commentary.phase(), Some(OutputPhase::Commentary));
    assert_eq!(final_message.phase(), Some(OutputPhase::Final));
    assert_eq!(commentary.text_content(), "正在检查");
    assert_eq!(OutputPhase::Commentary.to_string(), "commentary");
}

#[test]
fn test_item_model_08() {
    let original = item(
        "message",
        RunItemKind::Message(Message::assistant("完成", OutputPhase::Final)),
    )
    .with_provenance(ItemProvenance::new(AgentId::new("coder")).with_agent_name("Coder"))
    .with_raw_provider_item(RawProviderItem::new("provider", json!({"id": "raw-1"})))
    .with_session_data("ui", json!({"expanded": true}));

    let rewritten = original.clone().with_output_phase(OutputPhase::Commentary);
    assert_eq!(rewritten.id(), original.id());
    assert_eq!(rewritten.provenance(), original.provenance());
    assert_eq!(rewritten.raw_provider_item(), original.raw_provider_item());
    assert_eq!(rewritten.session_data(), original.session_data());
    let RunItemKind::Message(message) = rewritten.kind() else {
        panic!("改写后必须仍是 message");
    };
    assert_eq!(message.phase(), Some(OutputPhase::Commentary));

    let user = item("user", RunItemKind::Message(Message::user("继续")))
        .with_output_phase(OutputPhase::Final);
    let RunItemKind::Message(message) = user.kind() else {
        panic!("必须仍是 message");
    };
    assert_eq!(message.phase(), None);
}

#[test]
fn test_item_model_09() {
    let original = item("item", RunItemKind::Message(Message::user("hello")));
    let mut value = serde_json::to_value(original).expect("应可转 JSON");
    let object = value.as_object_mut().expect("RunItem 应是对象");
    object.insert("future_envelope".into(), json!({"keep": true}));
    object
        .get_mut("kind")
        .and_then(Value::as_object_mut)
        .and_then(|kind| kind.get_mut("data"))
        .and_then(Value::as_object_mut)
        .expect("message data 应是对象")
        .insert("future_message".into(), json!([1, 2, 3]));

    let old_reader: RunItem = serde_json::from_value(value).expect("旧代码应能读新版 item");
    assert!(old_reader.unknown().get("future_envelope").is_some());
    let RunItemKind::Message(message) = old_reader.kind() else {
        panic!("应仍是 message");
    };
    assert!(message.unknown().get("future_message").is_some());

    let rewritten = serde_json::to_value(old_reader).expect("旧代码应能回写");
    assert_eq!(rewritten["future_envelope"]["keep"], true);
    assert_eq!(rewritten["kind"]["data"]["future_message"][2], 3);
}

#[test]
fn test_item_model_10() {
    let item = item("item", RunItemKind::Message(Message::system("system")))
        .with_session_data("z", json!(1))
        .with_session_data("a", json!(2));

    let keys: Vec<&str> = item
        .session_data()
        .iter()
        .map(|(key, _)| key.as_str())
        .collect();
    assert_eq!(keys, vec!["a", "z"]);
    assert!(item.unknown().is_empty());
}
