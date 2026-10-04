//! The `Session` contract a storage backend in `ra-session` has to honour, so that it is
//! interchangeable with `InMemorySession`.
//!
//! It sits under `support/` so that Cargo does not build it as a test binary of its own.

#![allow(dead_code)]

use ra_core::item::{
    AgentId, CallId, ItemId, ItemProvenance, Message, OutputPhase, RawProviderItem, Reasoning,
    RunItem, RunItemKind, ToolApproval, ToolCall, ToolCallOutput,
};
use ra_session::Session;
use serde_json::json;

pub fn user(id: &str, text: &str) -> RunItem {
    RunItem::new(ItemId::new(id), RunItemKind::Message(Message::user(text)))
}

pub fn assistant(id: &str, text: &str) -> RunItem {
    RunItem::new(
        ItemId::new(id),
        RunItemKind::Message(Message::assistant(text, OutputPhase::Final)),
    )
}

/// Malformed JSON whose typed decoder can report a data error before reaching the bad syntax.
pub const MALFORMED_ITEM_JSON: [&str; 3] = [
    "null trailing-garbage",
    r#"{"schema_version":"wrong","id":"unfinished""#,
    r#"{"schema_version":1,"id":"n","kind":{"type":"from_the_future"}} trailing-garbage"#,
];

/// Items of several kinds, so a backend cannot pass by storing only messages.
pub fn sample_items() -> Vec<RunItem> {
    vec![
        user("item-msg-1", "Hello"),
        RunItem::new(
            ItemId::new("item-reasoning-1"),
            RunItemKind::Reasoning(
                Reasoning::new()
                    .with_id("reasoning-1")
                    .with_summary(vec!["Plan first step".into()])
                    .with_encrypted_content("signature-proof")
                    .with_provider_data(json!({"thinking_blocks": [{"sig": "abc"}]})),
            ),
        ),
        RunItem::new(
            ItemId::new("item-call-1"),
            RunItemKind::ToolCall(ToolCall::new(
                CallId::new("call-1"),
                "read_file",
                json!({"path": "Cargo.toml"}),
            )),
        ),
        RunItem::new(
            ItemId::new("item-output-1"),
            RunItemKind::ToolCallOutput(ToolCallOutput::new(
                CallId::new("call-1"),
                json!({"content": "[workspace]"}),
            )),
        ),
        RunItem::new(
            ItemId::new("item-approval-1"),
            RunItemKind::ToolApproval(ToolApproval::new(
                CallId::new("call-2"),
                "bash",
                json!({"command": "ls"}),
            )),
        ),
        assistant("item-msg-final", "Done"),
    ]
}

/// An item carrying provenance, an isolated raw provider payload, session data, and a field this
/// build has never heard of.
pub fn item_with_metadata() -> RunItem {
    let item = RunItem::new(
        ItemId::new("item-complex"),
        RunItemKind::ToolCallOutput(ToolCallOutput::new(
            CallId::new("call-10"),
            json!({"result": "ok"}),
        )),
    )
    .with_provenance(
        ItemProvenance::new(AgentId::new("specialist-coder")).with_agent_name("Code Expert"),
    )
    .with_raw_provider_item(RawProviderItem::new(
        "openai",
        json!({"id": "call_123", "type": "function"}),
    ))
    .with_session_data("host_trace_id", json!("trace-uuid-999"));

    let mut value = serde_json::to_value(&item).unwrap();
    value
        .as_object_mut()
        .unwrap()
        .insert("future_extension_field".into(), json!({"flag": true}));
    let item: RunItem = serde_json::from_value(value).unwrap();
    assert!(item.unknown().get("future_extension_field").is_some());
    item
}

/// Runs the whole contract against an empty session.
pub async fn assert_session_contract(session: &dyn Session) {
    assert!(session.get_items(None).await.unwrap().is_empty());
    assert_eq!(session.pop_item().await.unwrap(), None);

    let items = sample_items();
    session.add_items(items.clone()).await.unwrap();
    session.add_items(Vec::new()).await.unwrap();
    assert_eq!(session.get_items(None).await.unwrap(), items);

    // A limit reads the newest items in order and deletes nothing.
    assert_eq!(session.get_items(Some(2)).await.unwrap(), items[4..]);
    assert!(session.get_items(Some(0)).await.unwrap().is_empty());
    assert_eq!(session.get_items(Some(100)).await.unwrap(), items);
    assert_eq!(session.get_items(None).await.unwrap(), items);

    let popped = session.pop_item().await.unwrap().unwrap();
    assert_eq!(popped, items[5]);
    assert_eq!(session.get_items(None).await.unwrap(), items[..5]);

    session.clear().await.unwrap();
    assert!(session.get_items(None).await.unwrap().is_empty());
    assert_eq!(session.pop_item().await.unwrap(), None);

    // Provenance, the raw provider payload, session data and unknown fields all survive.
    let item = item_with_metadata();
    session.add_items(vec![item.clone()]).await.unwrap();
    let stored = session.get_items(None).await.unwrap();
    assert_eq!(stored, vec![item.clone()]);
    let stored = &stored[0];
    assert_eq!(
        stored.provenance().unwrap().agent_name(),
        Some("Code Expert")
    );
    assert_eq!(
        stored.raw_provider_item().unwrap().payload()["id"],
        "call_123"
    );
    assert_eq!(
        stored.session_data().get("host_trace_id"),
        Some(&json!("trace-uuid-999"))
    );
    assert_eq!(
        stored.unknown().get("future_extension_field"),
        Some(&json!({"flag": true}))
    );
    assert_eq!(session.pop_item().await.unwrap(), Some(item));
}

/// Six messages `Message 1` / `Response 1` … `Response 3`, the reference's limit fixture.
pub fn conversation() -> Vec<RunItem> {
    (1..=3)
        .flat_map(|n| {
            [
                user(&format!("m{n}"), &format!("Message {n}")),
                assistant(&format!("r{n}"), &format!("Response {n}")),
            ]
        })
        .collect()
}

/// The texts of message items, in order.
pub fn texts(items: &[RunItem]) -> Vec<String> {
    items
        .iter()
        .map(|item| match item.kind() {
            RunItemKind::Message(message) => message.text_content(),
            other => panic!("expected a message, got {}", other.label()),
        })
        .collect()
}

/// The reference's `get_items` limit test.
pub async fn assert_limit_reads(session: &dyn Session) {
    session.add_items(conversation()).await.unwrap();

    let all = texts(&session.get_items(None).await.unwrap());
    assert_eq!(all.len(), 6);
    assert_eq!(all[0], "Message 1");
    assert_eq!(all[5], "Response 3");

    assert_eq!(
        texts(&session.get_items(Some(2)).await.unwrap()),
        ["Message 3", "Response 3"]
    );
    assert_eq!(
        texts(&session.get_items(Some(4)).await.unwrap()),
        ["Message 2", "Response 2", "Message 3", "Response 3"]
    );
    assert_eq!(session.get_items(Some(10)).await.unwrap().len(), 6);
    assert!(session.get_items(Some(0)).await.unwrap().is_empty());
}

/// Unicode and text that looks like SQL come back byte for byte.
pub async fn assert_text_round_trip(session: &dyn Session) {
    let contents = [
        "こんにちは",
        "😊👍",
        "Привет",
        "O'Reilly",
        "DROP TABLE sessions;",
        "\"SELECT * FROM users WHERE name = \"admin\";\"",
        "Robert'); DROP TABLE students;--",
        "line one\nline two",
    ];
    let items: Vec<_> = contents
        .iter()
        .enumerate()
        .map(|(index, text)| user(&format!("u{index}"), text))
        .collect();
    session.add_items(items).await.unwrap();
    assert_eq!(texts(&session.get_items(None).await.unwrap()), contents);
}
