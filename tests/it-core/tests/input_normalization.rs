use ra_core::item::{
    CallId, InputItemDigest, InputItemNormalizer, ItemId, Message, ModelInputItem,
    NormalizedInputItem, OrphanPolicy, OutputPhase, RawProviderItem, Reasoning, ReasoningIdPolicy,
    RunItem, RunItemKind, ToolApproval, ToolCall, ToolCallOutput,
};
use serde_json::json;

fn run_item(id: &str, kind: RunItemKind) -> RunItem {
    RunItem::new(ItemId::new(id), kind)
}

fn labels(entries: &[NormalizedInputItem]) -> Vec<&'static str> {
    entries.iter().map(|entry| entry.item().label()).collect()
}

#[test]
fn session_projection_strips_control_plane_and_provider_conversation_data() {
    let message = RunItem::new(
        ItemId::new("message-1"),
        RunItemKind::Message(Message::user("hello")),
    )
    .with_session_data("_rusty_agent_title", json!("UI only"))
    .with_raw_provider_item(RawProviderItem::new(
        "openai",
        json!({
            "id": "msg_remote",
            "conversation_id": "conv_remote",
            "content": "hello"
        }),
    ));
    let approval = run_item(
        "approval-1",
        RunItemKind::ToolApproval(ToolApproval::new(
            CallId::new("call-approval"),
            "dangerous_tool",
            json!({}),
        )),
    );

    let normalized = InputItemNormalizer::new()
        .normalize_run_items(&[message, approval])
        .expect("typed model input should always serialize");

    assert_eq!(labels(normalized.entries()), vec!["message"]);
    let encoded = serde_json::to_value(normalized.entries()[0].item())
        .expect("normalized item should serialize");
    let encoded_text = encoded.to_string();
    assert!(!encoded_text.contains("conversation_id"));
    assert!(!encoded_text.contains("msg_remote"));
    assert!(!encoded_text.contains("UI only"));
    assert_eq!(
        normalized.entries()[0]
            .occurrence_key()
            .expect("session projection should have coordinates")
            .item_id()
            .as_str(),
        "message-1"
    );
}

#[test]
fn default_prunes_unanswered_calls_and_their_dangling_reasoning() {
    let items = vec![
        ModelInputItem::Reasoning(
            Reasoning::new()
                .with_id("rs-orphan")
                .with_encrypted_content("encrypted-orphan"),
        ),
        ModelInputItem::ToolCall(ToolCall::new(
            CallId::new("call-orphan"),
            "orphan",
            json!({}),
        )),
        ModelInputItem::Message(Message::user("continue")),
        ModelInputItem::ToolCall(ToolCall::new(
            CallId::new("call-paired"),
            "paired",
            json!({}),
        )),
        ModelInputItem::ToolCallOutput(ToolCallOutput::new(
            CallId::new("call-paired"),
            json!("done"),
        )),
        // Output-only input is valid when the matching call lives behind previous_response_id.
        ModelInputItem::ToolCallOutput(ToolCallOutput::new(
            CallId::new("call-server-side"),
            json!("continued"),
        )),
    ];

    let normalized = InputItemNormalizer::new()
        .normalize_model_items(&items)
        .expect("typed model input should always serialize");

    assert_eq!(
        labels(normalized.entries()),
        vec!["message", "tool_call", "tool_call_output", "tool_call_output"]
    );
    assert!(normalized.entries().iter().all(|entry| {
        entry
            .item()
            .call_id()
            .is_none_or(|call_id| call_id.as_str() != "call-orphan")
    }));
}

#[test]
fn reasoning_without_a_surviving_follower_is_dropped() {
    // Rewind and compaction both leave reasoning at the tail. Responses-compatible endpoints
    // reject a reasoning item that is not followed by the item it was emitted with.
    let truncated = vec![
        ModelInputItem::Message(Message::user("earlier turn")),
        ModelInputItem::Reasoning(
            Reasoning::new()
                .with_id("rs-trailing")
                .with_encrypted_content("encrypted-trailing"),
        ),
    ];
    let normalized = InputItemNormalizer::new()
        .normalize_model_items(&truncated)
        .expect("typed model input should always serialize");
    assert_eq!(labels(normalized.entries()), vec!["message"]);

    // Caller-owned input is never reinterpreted, so the same history survives under `Preserve`.
    let preserved = InputItemNormalizer::new()
        .with_orphan_policy(OrphanPolicy::Preserve)
        .normalize_model_items(&truncated)
        .expect("typed model input should always serialize");
    assert_eq!(labels(preserved.entries()), vec!["message", "reasoning"]);

    // A follower that survives keeps its reasoning, including consecutive reasoning items.
    let answered = vec![
        ModelInputItem::Reasoning(Reasoning::new().with_id("rs-first")),
        ModelInputItem::Reasoning(Reasoning::new().with_id("rs-second")),
        ModelInputItem::Message(Message::assistant("done", OutputPhase::Final)),
    ];
    let kept = InputItemNormalizer::new()
        .normalize_model_items(&answered)
        .expect("typed model input should always serialize");
    assert_eq!(
        labels(kept.entries()),
        vec!["reasoning", "reasoning", "message"]
    );
}

#[test]
fn digests_ignore_json_key_order_so_they_stay_stable_across_hosts() {
    // The digest is only a reconciliation key if it depends on the value, not on the byte order a
    // provider happened to send. This fails the moment anything enables `serde_json/preserve_order`.
    let ordered = |arguments: &str| {
        ModelInputItem::ToolCall(ToolCall::new(
            CallId::new("call-1"),
            "tool",
            serde_json::from_str(arguments).expect("fixture arguments should parse"),
        ))
    };
    let first = ordered(r#"{"alpha": 1, "beta": {"inner": 2, "also": 3}}"#);
    let second = ordered(r#"{"beta": {"also": 3, "inner": 2}, "alpha": 1}"#);

    assert_eq!(
        InputItemDigest::compute(&first).expect("typed model input should serialize"),
        InputItemDigest::compute(&second).expect("typed model input should serialize")
    );
}

#[test]
fn orphan_policy_distinguishes_caller_input_from_self_contained_history() {
    let items = vec![
        ModelInputItem::ToolCall(ToolCall::new(
            CallId::new("call-only"),
            "pending",
            json!({}),
        )),
        ModelInputItem::ToolCallOutput(ToolCallOutput::new(
            CallId::new("output-only"),
            json!("remote"),
        )),
    ];

    let preserved = InputItemNormalizer::new()
        .with_orphan_policy(OrphanPolicy::Preserve)
        .normalize_model_items(&items)
        .expect("typed model input should always serialize");
    let strict = InputItemNormalizer::new()
        .with_orphan_policy(OrphanPolicy::DropUnpaired)
        .normalize_model_items(&items)
        .expect("typed model input should always serialize");

    assert_eq!(preserved.entries().len(), 2);
    assert!(strict.entries().is_empty());
}

#[test]
fn dedupe_uses_explicit_identity_and_keeps_causal_anchors() {
    let repeated_message = ModelInputItem::Message(Message::user("same text is intentional"));
    let items = vec![
        ModelInputItem::ToolCall(ToolCall::new(
            CallId::new("call-1"),
            "old_name",
            json!({"version": 1}),
        )),
        ModelInputItem::ToolCallOutput(ToolCallOutput::new(
            CallId::new("call-1"),
            json!("old output"),
        )),
        repeated_message.clone(),
        repeated_message,
        ModelInputItem::ToolCall(ToolCall::new(
            CallId::new("call-1"),
            "new_name",
            json!({"version": 2}),
        )),
        ModelInputItem::ToolCallOutput(ToolCallOutput::new(
            CallId::new("call-1"),
            json!("new output"),
        )),
    ];

    let normalized = InputItemNormalizer::new()
        .normalize_model_items(&items)
        .expect("typed model input should always serialize");
    assert_eq!(
        labels(normalized.entries()),
        vec!["tool_call", "message", "message", "tool_call_output"]
    );

    let ModelInputItem::ToolCall(call) = normalized.entries()[0].item() else {
        panic!("first item should remain the causal call anchor");
    };
    assert_eq!(call.name(), "new_name");
    assert_eq!(call.arguments(), &json!({"version": 2}));

    let ModelInputItem::ToolCallOutput(output) = normalized.entries()[3].item() else {
        panic!("latest output should remain at its latest position");
    };
    assert_eq!(output.output(), &json!("new output"));
}

#[test]
fn omitting_reasoning_id_preserves_every_replay_bearing_field() {
    let item = ModelInputItem::Reasoning(
        Reasoning::new()
            .with_id("rs-1")
            .with_summary(vec!["summary".to_owned()])
            .with_content(vec!["private thought".to_owned()])
            .with_encrypted_content("encrypted")
            .with_provider_data(json!({
                "type": "reasoning",
                "encrypted_content": "encrypted",
                "signature": "signature"
            })),
    );

    let normalized = InputItemNormalizer::new()
        .with_orphan_policy(OrphanPolicy::Preserve)
        .with_reasoning_id_policy(ReasoningIdPolicy::Omit)
        .normalize_model_items(&[item])
        .expect("typed model input should always serialize");
    let ModelInputItem::Reasoning(reasoning) = normalized.entries()[0].item() else {
        panic!("reasoning should be retained");
    };

    assert_eq!(reasoning.id(), None);
    assert_eq!(reasoning.summary(), &["summary"]);
    assert_eq!(reasoning.content(), &["private thought"]);
    assert_eq!(reasoning.encrypted_content(), Some("encrypted"));
    assert_eq!(
        reasoning.provider_data(),
        Some(&json!({
            "type": "reasoning",
            "encrypted_content": "encrypted",
            "signature": "signature"
        }))
    );
}

#[test]
fn occurrence_coordinates_follow_the_latest_value_not_an_array_position() {
    let old = run_item(
        "call-old-occurrence",
        RunItemKind::ToolCall(ToolCall::new(
            CallId::new("call-1"),
            "tool",
            json!({"version": 1}),
        )),
    );
    let output = run_item(
        "output-occurrence",
        RunItemKind::ToolCallOutput(ToolCallOutput::new(
            CallId::new("call-1"),
            json!("done"),
        )),
    );
    let latest = run_item(
        "call-latest-occurrence",
        RunItemKind::ToolCall(ToolCall::new(
            CallId::new("call-1"),
            "tool",
            json!({"version": 2}),
        )),
    );

    let normalized = InputItemNormalizer::new()
        .normalize_run_items(&[old, output, latest])
        .expect("typed model input should always serialize");
    let call = &normalized.entries()[0];
    let key = call
        .occurrence_key()
        .expect("session-projected input should retain coordinates");

    assert_eq!(key.item_id().as_str(), "call-latest-occurrence");
    assert_eq!(
        key.digest(),
        &InputItemDigest::compute(call.item()).expect("typed model input should serialize")
    );

    let surrounded = vec![
        run_item(
            "prefix",
            RunItemKind::Message(Message::assistant("before", OutputPhase::Commentary)),
        ),
        run_item("call-copy", latest_kind(call.item())),
        run_item(
            "suffix",
            RunItemKind::Message(Message::assistant("after", OutputPhase::Final)),
        ),
        run_item(
            "output-copy",
            RunItemKind::ToolCallOutput(ToolCallOutput::new(
                CallId::new("call-1"),
                json!("done"),
            )),
        ),
    ];
    let moved = InputItemNormalizer::new()
        .normalize_run_items(&surrounded)
        .expect("typed model input should always serialize");
    let moved_call = moved
        .entries()
        .iter()
        .find(|entry| matches!(entry.item(), ModelInputItem::ToolCall(_)))
        .expect("call should remain paired");
    assert_eq!(
        moved_call
            .occurrence_key()
            .expect("session-projected input should retain coordinates")
            .digest(),
        key.digest()
    );
}

fn latest_kind(item: &ModelInputItem) -> RunItemKind {
    let ModelInputItem::ToolCall(call) = item else {
        panic!("test fixture should be a tool call");
    };
    RunItemKind::ToolCall(call.clone())
}
