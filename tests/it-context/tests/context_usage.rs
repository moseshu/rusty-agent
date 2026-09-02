//! Context-window usage estimates grouped by model-visible source.

use std::collections::BTreeMap;

use ra_context::{
    compaction::ContextUsage,
    usage::{ContextUsageBreakdown, ContextUsageCategory},
    window::{ContextWindowConfig, ContextWindowThresholdRatio},
};
use ra_core::{
    item::{
        AgentId, CallId, Compaction, HandoffCall, HandoffOutput, McpApprovalRequest,
        McpApprovalResponse, McpListTools, McpTool, Message, ModelInputItem, Reasoning, ToolCall,
        ToolCallOutput,
    },
    model::{
        ModelHandoffDefinition, ModelOutputSchema, ModelRequest, ModelSettings,
        ModelToolDefinition, ProviderKey, ResolvedModelSettings,
    },
};
use serde_json::json;

fn settings() -> ResolvedModelSettings {
    let empty = ModelSettings::new();
    empty.resolve(&ProviderKey::new("test"), &empty, &empty, &empty)
}

fn item_tokens(item: ModelInputItem) -> usize {
    ContextUsage::estimate_model_input(&[item])
        .expect("test item should serialize")
        .total_tokens()
}

fn breakdown(request: &ModelRequest) -> ContextUsageBreakdown {
    ContextUsageBreakdown::estimate_model_request(request).expect("request should be measurable")
}

fn category_of(item: ModelInputItem) -> ContextUsageCategory {
    let usage = breakdown(&ModelRequest::new(vec![item], settings()));
    let charged: Vec<ContextUsageCategory> = ContextUsageCategory::ALL
        .into_iter()
        .filter(|category| usage.tokens(*category) > 0)
        .collect();
    assert_eq!(
        charged.len(),
        1,
        "one item must be charged to exactly one category, not {charged:?}"
    );
    charged[0]
}

#[test]
fn separates_every_model_visible_source_without_double_counting() {
    let message = ModelInputItem::Message(Message::user("hello"));
    let tool_call = ModelInputItem::ToolCall(ToolCall::new(
        CallId::new("call-1"),
        "search",
        json!({"query": "rust"}),
    ));
    let tool_result = ModelInputItem::ToolCallOutput(ToolCallOutput::new(
        CallId::new("call-1"),
        json!({"text": "abcdefgh"}),
    ));
    let reasoning = ModelInputItem::Reasoning(Reasoning::new().with_content(vec!["think".into()]));
    let expected_messages = item_tokens(message.clone()) + item_tokens(tool_call.clone());
    let expected_results = item_tokens(tool_result.clone());
    let expected_reasoning = item_tokens(reasoning.clone());

    let tool = ModelToolDefinition::new(
        "search",
        json!({"type": "object", "properties": {"query": {"type": "string"}}}),
    )
    .with_description("Find files");
    let handoff = ModelHandoffDefinition::new(
        AgentId::new("reviewer"),
        "delegate",
        json!({"type": "object"}),
    )
    .with_description("Pass review");
    let output_schema = ModelOutputSchema::new("result", json!({"type": "object"}));
    // Every definition is rendered, then the table is rounded to tokens once: rounding each part
    // on its own would charge a tool up to three tokens it does not cost.
    let definition_chars = tool.advertised_chars().expect("tool renders")
        + handoff.advertised_chars().expect("handoff renders")
        + output_schema.advertised_chars().expect("schema renders");
    let expected_tools = definition_chars.div_ceil(4);

    let request = ModelRequest::new(
        vec![message, tool_call, tool_result, reasoning],
        settings(),
    )
    .with_system_instructions("abcd")
    .with_tools(vec![tool])
    .with_handoffs(vec![handoff])
    .with_output_schema(output_schema);

    let usage = breakdown(&request);

    assert_eq!(usage.system_tokens(), 1);
    assert_eq!(usage.tool_tokens(), expected_tools);
    assert_eq!(usage.message_tokens(), expected_messages);
    assert_eq!(usage.tool_result_tokens(), expected_results);
    assert_eq!(usage.reasoning_tokens(), expected_reasoning);
    assert_eq!(
        usage.total_tokens(),
        usage.system_tokens()
            + usage.tool_tokens()
            + expected_messages
            + expected_results
            + expected_reasoning
    );
    assert_eq!(
        ContextUsageCategory::ALL.map(|category| usage.tokens(category)),
        [
            usage.system_tokens(),
            usage.tool_tokens(),
            expected_messages,
            expected_results,
            expected_reasoning,
        ]
    );
    assert_eq!(
        ContextUsageCategory::ALL
            .into_iter()
            .map(|category| usage.tokens(category))
            .sum::<usize>(),
        usage.total_tokens(),
        "the categories a UI iterates over must add up to the reported total"
    );
}

#[test]
fn a_definition_is_charged_for_the_parameter_names_the_model_reads() {
    let schema = json!({
        "type": "object",
        "properties": {
            "file_path": {"type": "string"},
            "line_limit": {"type": "integer"}
        },
        "required": ["file_path"]
    });
    let tool = ModelToolDefinition::new("read_file", schema.clone());
    let request = ModelRequest::new(Vec::new(), settings()).with_tools(vec![tool.clone()]);

    // The content-only walk replay items use would see only the values — "object", "string",
    // "integer", "file_path" — and charge `properties`, `required`, and the `line_limit`
    // parameter name nothing at all.
    let values_only = ["object", "string", "integer", "file_path"]
        .map(str::len)
        .iter()
        .sum::<usize>();
    let rendered = tool.advertised_chars().expect("tool renders");
    assert!(
        rendered > values_only * 2,
        "a schema's keys carry most of its text: {rendered} rendered vs {values_only} in values"
    );

    let usage = breakdown(&request);
    assert_eq!(usage.tool_tokens(), rendered.div_ceil(4));
    assert_eq!(usage.total_tokens(), usage.tool_tokens());
    assert_eq!(usage.model_input().total_tokens(), 0);
}

#[test]
fn every_model_input_variant_lands_in_a_deliberate_category() {
    let call = CallId::new("call-1");
    let cases = [
        (
            ModelInputItem::Message(Message::user("hello")),
            ContextUsageCategory::Messages,
        ),
        (
            ModelInputItem::ToolCall(ToolCall::new(call.clone(), "search", json!({"q": "rust"}))),
            ContextUsageCategory::Messages,
        ),
        (
            ModelInputItem::ToolCallOutput(ToolCallOutput::new(call.clone(), json!({"t": "ok"}))),
            ContextUsageCategory::ToolResults,
        ),
        (
            ModelInputItem::Reasoning(Reasoning::new().with_content(vec!["think".into()])),
            ContextUsageCategory::Reasoning,
        ),
        (
            ModelInputItem::HandoffCall(HandoffCall::new(
                call.clone(),
                AgentId::new("reviewer"),
                json!({}),
            )),
            ContextUsageCategory::Messages,
        ),
        (
            ModelInputItem::HandoffOutput(HandoffOutput::new(
                call.clone(),
                AgentId::new("planner"),
                AgentId::new("reviewer"),
            )),
            ContextUsageCategory::Messages,
        ),
        (
            // An MCP catalog advertises name, description, and input schema exactly as a tool
            // definition does, so it is priced against the tool surface rather than the history.
            ModelInputItem::McpListTools(McpListTools::new(
                "files",
                vec![McpTool::new("read_file", json!({"type": "object"}))],
            )),
            ContextUsageCategory::Tools,
        ),
        (
            ModelInputItem::McpApprovalRequest(McpApprovalRequest::new(
                "req-1",
                "files",
                "read_file",
                json!({"path": "src/lib.rs"}),
            )),
            ContextUsageCategory::Messages,
        ),
        (
            ModelInputItem::McpApprovalResponse(McpApprovalResponse::new("req-1", true)),
            ContextUsageCategory::Messages,
        ),
        (
            ModelInputItem::Compaction(Compaction::new(
                "a summary of earlier work",
                Vec::new(),
            )),
            ContextUsageCategory::Messages,
        ),
    ];

    for (item, expected) in cases {
        let label = item.label();
        assert_eq!(
            category_of(item),
            expected,
            "`{label}` must be charged to `{}`",
            expected.label()
        );
    }
}

#[test]
fn the_history_measurement_is_reported_without_a_second_walk() {
    let items = vec![
        ModelInputItem::Message(Message::user("one two three four")),
        ModelInputItem::ToolCallOutput(ToolCallOutput::new(
            CallId::new("call-1"),
            json!({"text": "a much longer observation than the message"}),
        )),
        ModelInputItem::Reasoning(Reasoning::new().with_content(vec!["think".into()])),
    ];
    let expected = ContextUsage::estimate_model_input(&items).expect("items should serialize");

    let request = ModelRequest::new(items, settings())
        .with_system_instructions("stable instructions")
        .with_tools(vec![ModelToolDefinition::new(
            "search",
            json!({"type": "object"}),
        )]);
    let usage = breakdown(&request);

    assert_eq!(
        usage.model_input(),
        expected,
        "the breakdown must derive exactly what the compaction estimator would report"
    );
    assert_eq!(
        usage.model_input().total_tokens(),
        usage.message_tokens() + usage.tool_result_tokens() + usage.reasoning_tokens(),
        "history categories must add up to the history measurement"
    );
    assert_eq!(
        usage.total_tokens(),
        usage.system_tokens() + usage.tool_tokens() + usage.model_input().total_tokens(),
        "the request total is the history plus what compaction cannot shrink"
    );
    assert!(usage.total_tokens() > usage.model_input().total_tokens());
}

#[test]
fn reports_capacity_only_for_a_model_with_a_configured_window() {
    let input = ModelInputItem::Message(Message::user("one two three four"));
    let expected_total = item_tokens(input.clone());
    let request = ModelRequest::new(vec![input], settings());
    let usage = breakdown(&request);
    let windows = ContextWindowConfig::new(
        BTreeMap::from([("test-model".to_owned(), 4_u64)]),
        ContextWindowThresholdRatio::new(6_000).expect("valid ratio"),
    )
    .expect("valid context-window configuration");

    let occupied = usage
        .for_model(&windows, "test-model")
        .expect("configured model has a context window");
    assert_eq!(occupied.breakdown(), usage);
    assert_eq!(occupied.context_window(), 4);
    assert_eq!(occupied.total_tokens(), expected_total);
    assert_eq!(
        occupied.occupancy_basis_points(),
        (expected_total as u64).saturating_mul(10_000) / 4
    );
    assert!(usage.for_model(&windows, "unknown-model").is_none());
}

#[test]
fn occupancy_reports_both_fractional_and_whole_multiples_of_a_window() {
    let request = ModelRequest::new(
        vec![ModelInputItem::Message(Message::user("one two three four"))],
        settings(),
    );
    let usage = breakdown(&request);
    let total = usage.total_tokens() as u64;
    assert!(total > 0, "the fixture must occupy some context");

    // The ordinary case: a request far below its window reports a fraction, never a whole.
    let roomy = ContextWindowConfig::new(
        BTreeMap::from([("roomy".to_owned(), 200_000_u64)]),
        ContextWindowThresholdRatio::new(6_000).expect("valid ratio"),
    )
    .expect("valid context-window configuration");
    let occupied = usage
        .for_model(&roomy, "roomy")
        .expect("configured model has a context window");
    assert_eq!(
        occupied.occupancy_basis_points(),
        total * 10_000 / 200_000,
        "a request below its window is a fractional occupancy"
    );
    assert!(occupied.occupancy_basis_points() < 10_000);

    // A window of one token drives the total into the whole part, which is the half of the split
    // the fractional-only case never reaches. A total large enough to make `saturating_mul`
    // actually clamp cannot be built from a real request, so the split itself is the guard under
    // test: it keeps the scaling multiplication off the total.
    let tiny = ContextWindowConfig::new(
        BTreeMap::from([("tiny".to_owned(), 1_u64)]),
        ContextWindowThresholdRatio::new(6_000).expect("valid ratio"),
    )
    .expect("valid context-window configuration");
    let cramped = usage
        .for_model(&tiny, "tiny")
        .expect("configured model has a context window");
    assert_eq!(cramped.occupancy_basis_points(), total * 10_000);
    assert!(cramped.occupancy_basis_points() > 10_000);
}

#[test]
fn categories_have_stable_machine_labels() {
    assert_eq!(
        ContextUsageCategory::ALL.map(ContextUsageCategory::label),
        ["system", "tools", "messages", "tool_results", "reasoning"]
    );
    assert_eq!(ContextUsageCategory::ALL.len(), ContextUsageCategory::COUNT);
}
