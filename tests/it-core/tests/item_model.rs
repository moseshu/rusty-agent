//! R1-1 contract tests for the item model, authoritative session records, and model-input views.

use ra_core::{
    item::{
        AgentId, CallId, Compaction, HandoffCall, HandoffOutput, ItemId, ItemProvenance,
        McpApprovalRequest, McpApprovalResponse, McpListTools, McpTool, Message, MessageRole,
        ModelInputItem, ModelResponse, OutputPhase, RawProviderItem, Reasoning, RunItem,
        RunItemKind, ToolApproval, ToolCall, ToolCallOutput,
    },
    usage::Usage,
};
use serde_json::{Value, json};

fn 记录(id: &str, kind: RunItemKind) -> RunItem {
    RunItem::new(ItemId::new(id), kind)
}

fn 所有项() -> Vec<RunItem> {
    vec![
        记录(
            "item-message",
            RunItemKind::Message(Message::assistant("完成", OutputPhase::Final)),
        ),
        记录(
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
        记录(
            "item-call",
            RunItemKind::ToolCall(ToolCall::new(
                CallId::new("call-1"),
                "read_file",
                json!({"path": "Cargo.toml"}),
            )),
        ),
        记录(
            "item-output",
            RunItemKind::ToolCallOutput(ToolCallOutput::new(
                CallId::new("call-1"),
                json!({"text": "workspace"}),
            )),
        ),
        记录(
            "item-handoff-call",
            RunItemKind::HandoffCall(HandoffCall::new(
                CallId::new("handoff-1"),
                AgentId::new("reviewer"),
                json!({"focus": "correctness"}),
            )),
        ),
        记录(
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
        记录(
            "item-mcp-list",
            RunItemKind::McpListTools(McpListTools::new(
                "filesystem",
                vec![McpTool::new(
                    "read_text_file",
                    json!({"type": "object"}),
                )
                .with_description("读取文本")],
            )),
        ),
        记录(
            "item-mcp-request",
            RunItemKind::McpApprovalRequest(McpApprovalRequest::new(
                "approval-1",
                "filesystem",
                "write_file",
                json!({"path": "a.txt"}),
            )),
        ),
        记录(
            "item-mcp-response",
            RunItemKind::McpApprovalResponse(
                McpApprovalResponse::new("approval-1", false).with_reason("只读任务"),
            ),
        ),
        记录(
            "item-compaction",
            RunItemKind::Compaction(Compaction::new(
                "此前已检查 workspace",
                vec![ItemId::new("item-message")],
            )),
        ),
        记录(
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
fn 十一种_run_item_都能稳定往返() {
    let items = 所有项();
    assert_eq!(items.len(), 11);

    for item in items {
        let json = serde_json::to_string(&item).expect("RunItem 应可序列化");
        let back: RunItem = serde_json::from_str(&json).expect("RunItem 应可反序列化");
        assert_eq!(back, item, "往返改变了 {}", item.kind().label());
    }
}

#[test]
fn 模型投影在类型层排除工具审批项() {
    let items = 所有项();
    let response = ModelResponse::new(items);
    let input = response.to_input_items();

    assert_eq!(response.output().len(), 11, "session 权威记录不能被过滤");
    assert_eq!(input.len(), 10, "只有 ToolApproval 属于 session 控制面");
    assert!(input.iter().all(|item| item.label() != "tool_approval"));
}

#[test]
fn 模型投影不会泄漏归属_raw_provider_与_session_data() {
    let item = 记录(
        "item-1",
        RunItemKind::Message(Message::user("检查项目")),
    )
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
        assert!(!sent.contains(forbidden), "模型输入泄漏了 {forbidden}: {sent}");
    }
    assert!(sent.contains("检查项目"));
}

#[test]
fn 工具调用与输出只按_call_id_配对() {
    let call = 记录(
        "a",
        RunItemKind::ToolCall(ToolCall::new(
            CallId::new("same-call"),
            "read_file",
            json!({}),
        )),
    );
    let output = 记录(
        "完全不同的-item-id",
        RunItemKind::ToolCallOutput(ToolCallOutput::new(
            CallId::new("same-call"),
            json!("ok"),
        )),
    );

    assert_ne!(call.id(), output.id());
    assert_eq!(call.call_id(), output.call_id());
    assert_eq!(call.call_id().map(CallId::as_str), Some("same-call"));
}

#[test]
fn reasoning_签名和完整_provider_序列在投影中保真() {
    let reasoning = Reasoning::new()
        .with_id("r-1")
        .with_content(vec![String::new(), "visible".into()])
        .with_encrypted_content("sig-a\nsig-b")
        .with_provider_data(json!([
            {"type": "thinking", "thinking": "", "signature": "sig-a"},
            {"type": "redacted_thinking", "data": "opaque"}
        ]));
    let item = 记录(
        "reasoning",
        RunItemKind::Reasoning(reasoning.clone()),
    );

    let Some(ModelInputItem::Reasoning(projected)) = item.to_model_input() else {
        panic!("reasoning 应投影为 reasoning");
    };
    assert_eq!(projected, reasoning);
    assert_eq!(projected.encrypted_content(), Some("sig-a\nsig-b"));
    assert_eq!(projected.provider_data().and_then(Value::as_array).map(Vec::len), Some(2));
}

#[test]
fn model_response_保留双_id_usage_并生成下一轮输入() {
    let response = ModelResponse::new(vec![记录(
        "message",
        RunItemKind::Message(Message::assistant("结果", OutputPhase::Final)),
    )])
    .with_usage(
        Usage::new(120, 30)
            .with_cached_input_tokens(80)
            .with_reasoning_tokens(10),
    )
    .with_response_id("response-1")
    .with_request_id("request-1");

    assert_eq!(response.response_id(), Some("response-1"));
    assert_eq!(response.request_id(), Some("request-1"));
    assert_eq!(response.usage().total_tokens(), 150);
    assert_eq!(response.usage().cached_input_tokens(), 80);
    assert_eq!(response.to_input_items().len(), 1);
}

#[test]
fn message_角色与双通道是协议字段而不是文本约定() {
    let commentary = Message::assistant("正在检查", OutputPhase::Commentary);
    let final_message = Message::assistant("检查完成", OutputPhase::Final);

    assert_eq!(commentary.role(), MessageRole::Assistant);
    assert_eq!(commentary.phase(), Some(OutputPhase::Commentary));
    assert_eq!(final_message.phase(), Some(OutputPhase::Final));
    assert_eq!(commentary.text_content(), "正在检查");
    assert_eq!(OutputPhase::Commentary.to_string(), "commentary");
}

#[test]
fn 新版未知字段在_run_item_和_payload_两层都原样回写() {
    let original = 记录("item", RunItemKind::Message(Message::user("hello")));
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
fn session_data_序列化次序确定且不会混入未知字段() {
    let item = 记录(
        "item",
        RunItemKind::Message(Message::system("system")),
    )
    .with_session_data("z", json!(1))
    .with_session_data("a", json!(2));

    let keys: Vec<&str> = item.session_data().iter().map(|(key, _)| key.as_str()).collect();
    assert_eq!(keys, vec!["a", "z"]);
    assert!(item.unknown().is_empty());
}
