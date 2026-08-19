//! Contract tests for the reference in-memory `Session` implementation, and for the requirement
//! that a second, unrelated implementation is interchangeable with it.
//!
//! The port's own shape lives in `tests/it-core/tests/session_port.rs`. What is asserted here is
//! that the implementation this crate actually ships honours it: tail reads project without
//! mutating, and a `RunItem` survives a round trip with its provenance, its isolated raw provider
//! payload, and its unknown fields intact.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ra_core::{
    error::Result,
    item::{
        AgentId, CallId, Compaction, HandoffCall, HandoffOutput, ItemId, ItemProvenance,
        McpApprovalRequest, McpApprovalResponse, McpListTools, McpTool, Message, OutputPhase,
        RawProviderItem, Reasoning, RunItem, RunItemKind, ToolApproval, ToolCall, ToolCallOutput,
    },
};
use ra_session::{InMemorySession, Session, SessionId};
use serde_json::json;
use tokio::sync::Barrier;

/// A session backed by neither SQLite nor a JSONL rollout log, standing in for a product that
/// brings its own storage. It also counts calls, so interchangeability can be asserted on the
/// traffic reaching the implementation rather than only on the values coming back.
#[derive(Debug)]
struct RecordingSession {
    session_id: SessionId,
    items: Mutex<Vec<RunItem>>,
    get_count: Mutex<usize>,
    add_count: Mutex<usize>,
}

impl RecordingSession {
    fn new(session_id: impl Into<SessionId>) -> Self {
        Self {
            session_id: session_id.into(),
            items: Mutex::new(Vec::new()),
            get_count: Mutex::new(0),
            add_count: Mutex::new(0),
        }
    }

    fn get_call_count(&self) -> usize {
        *self.get_count.lock().unwrap()
    }

    fn add_call_count(&self) -> usize {
        *self.add_count.lock().unwrap()
    }
}

#[async_trait]
impl Session for RecordingSession {
    fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    async fn get_items(&self, limit: Option<usize>) -> Result<Vec<RunItem>> {
        *self.get_count.lock().unwrap() += 1;
        let guard = self.items.lock().unwrap();
        let items = match limit {
            None => guard.clone(),
            Some(n) if n >= guard.len() => guard.clone(),
            Some(n) => guard[guard.len() - n..].to_vec(),
        };
        Ok(items)
    }

    async fn add_items(&self, items: Vec<RunItem>) -> Result<()> {
        *self.add_count.lock().unwrap() += 1;
        let mut guard = self.items.lock().unwrap();
        guard.extend(items);
        Ok(())
    }

    async fn pop_item(&self) -> Result<Option<RunItem>> {
        let mut guard = self.items.lock().unwrap();
        Ok(guard.pop())
    }

    async fn clear(&self) -> Result<()> {
        let mut guard = self.items.lock().unwrap();
        guard.clear();
        Ok(())
    }
}

fn make_item(id: &str, kind: RunItemKind) -> RunItem {
    RunItem::new(ItemId::new(id), kind)
}

/// One item of every kind a session has to carry, so a round trip cannot pass by handling only
/// the two message shapes.
fn sample_items() -> Vec<RunItem> {
    vec![
        make_item("item-msg-1", RunItemKind::Message(Message::user("Hello"))),
        make_item(
            "item-reasoning-1",
            RunItemKind::Reasoning(
                Reasoning::new()
                    .with_id("reasoning-1")
                    .with_summary(vec!["Plan first step".into()])
                    .with_content(vec!["Analyze workspace structure".into()])
                    .with_encrypted_content("signature-proof")
                    .with_provider_data(json!({"thinking_blocks": [{"sig": "abc"}]})),
            ),
        ),
        make_item(
            "item-call-1",
            RunItemKind::ToolCall(ToolCall::new(
                CallId::new("call-1"),
                "read_file",
                json!({"path": "Cargo.toml"}),
            )),
        ),
        make_item(
            "item-output-1",
            RunItemKind::ToolCallOutput(ToolCallOutput::new(
                CallId::new("call-1"),
                json!({"content": "[workspace]"}),
            )),
        ),
        make_item(
            "item-handoff-1",
            RunItemKind::HandoffCall(HandoffCall::new(
                CallId::new("handoff-1"),
                AgentId::new("reviewer"),
                json!({"target": "security"}),
            )),
        ),
        make_item(
            "item-handoff-out-1",
            RunItemKind::HandoffOutput(
                HandoffOutput::new(
                    CallId::new("handoff-1"),
                    AgentId::new("planner"),
                    AgentId::new("reviewer"),
                )
                .with_note("Please check permissions"),
            ),
        ),
        make_item(
            "item-mcp-list-1",
            RunItemKind::McpListTools(McpListTools::new(
                "filesystem",
                vec![McpTool::new("list_dir", json!({"type": "object"}))],
            )),
        ),
        make_item(
            "item-mcp-req-1",
            RunItemKind::McpApprovalRequest(McpApprovalRequest::new(
                "appr-1",
                "filesystem",
                "delete_file",
                json!({"path": "temp.txt"}),
            )),
        ),
        make_item(
            "item-mcp-res-1",
            RunItemKind::McpApprovalResponse(
                McpApprovalResponse::new("appr-1", true).with_reason("Safe operation"),
            ),
        ),
        make_item(
            "item-compact-1",
            RunItemKind::Compaction(Compaction::new(
                "Summarized previous 3 turns",
                vec![ItemId::new("item-msg-1")],
            )),
        ),
        make_item(
            "item-approval-1",
            RunItemKind::ToolApproval(ToolApproval::new(
                CallId::new("call-2"),
                "bash",
                json!({"command": "rm -rf /"}),
            )),
        ),
        make_item(
            "item-msg-final",
            RunItemKind::Message(Message::assistant("Done", OutputPhase::Final)),
        ),
    ]
}

#[tokio::test]
async fn test_in_memory_session_lifecycle() {
    let sid = SessionId::generate();
    let session = InMemorySession::new(sid.clone());

    assert_eq!(session.session_id(), &sid);

    let items = session
        .get_items(None)
        .await
        .expect("get_items should succeed");
    assert!(items.is_empty());

    let pop_empty = session.pop_item().await.expect("pop_item should succeed");
    assert!(pop_empty.is_none());

    let all_test_items = sample_items();
    let total_count = all_test_items.len();
    assert_eq!(total_count, 12);

    session
        .add_items(all_test_items.clone())
        .await
        .expect("add_items should succeed");

    let loaded = session
        .get_items(None)
        .await
        .expect("get_items should succeed");
    assert_eq!(loaded.len(), total_count);
    assert_eq!(loaded, all_test_items);

    let popped = session.pop_item().await.expect("pop_item should succeed");
    assert!(popped.is_some());
    assert_eq!(popped.unwrap().id().as_str(), "item-msg-final");

    let after_pop = session
        .get_items(None)
        .await
        .expect("get_items should succeed");
    assert_eq!(after_pop.len(), total_count - 1);
    assert_eq!(after_pop.last().unwrap().id().as_str(), "item-approval-1");

    session.clear().await.expect("clear should succeed");
    let after_clear = session
        .get_items(None)
        .await
        .expect("get_items should succeed");
    assert!(after_clear.is_empty());
}

#[tokio::test]
async fn test_in_memory_session_seeded_with_existing_items() {
    let seed = sample_items();
    let session = InMemorySession::new_with_items(SessionId::generate(), seed.clone());

    let loaded = session
        .get_items(None)
        .await
        .expect("get_items should succeed");
    assert_eq!(loaded, seed);

    let tail = session
        .get_items(Some(2))
        .await
        .expect("get_items should succeed");
    assert_eq!(tail.len(), 2);
    assert_eq!(tail[0].id().as_str(), "item-approval-1");
    assert_eq!(tail[1].id().as_str(), "item-msg-final");
}

#[tokio::test]
async fn test_in_memory_session_get_items_limit_is_tail_read_projection() {
    let session = InMemorySession::new(SessionId::generate());
    let all_test_items = sample_items();
    let total_count = all_test_items.len();

    session
        .add_items(all_test_items.clone())
        .await
        .expect("add_items should succeed");

    let limit_zero = session
        .get_items(Some(0))
        .await
        .expect("limit 0 should succeed");
    assert!(limit_zero.is_empty());

    let limit_3 = session
        .get_items(Some(3))
        .await
        .expect("limit 3 should succeed");
    assert_eq!(limit_3.len(), 3);
    assert_eq!(limit_3[0].id().as_str(), "item-compact-1");
    assert_eq!(limit_3[1].id().as_str(), "item-approval-1");
    assert_eq!(limit_3[2].id().as_str(), "item-msg-final");

    let limit_all = session
        .get_items(Some(total_count))
        .await
        .expect("exact limit should succeed");
    assert_eq!(limit_all.len(), total_count);
    assert_eq!(limit_all, all_test_items);

    let limit_oversized = session
        .get_items(Some(total_count + 100))
        .await
        .expect("oversized limit should succeed");
    assert_eq!(limit_oversized.len(), total_count);
    assert_eq!(limit_oversized, all_test_items);

    // A limited read is a projection, so the stored history is untouched by all of the above.
    let unprojected = session
        .get_items(None)
        .await
        .expect("get_items should succeed");
    assert_eq!(unprojected.len(), total_count);
    assert_eq!(unprojected, all_test_items);
}

#[tokio::test]
async fn test_in_memory_session_preserves_provenance_raw_and_unknown_fields() {
    let session = InMemorySession::new(SessionId::generate());

    let mut item_with_metadata = make_item(
        "item-complex",
        RunItemKind::ToolCallOutput(ToolCallOutput::new(
            CallId::new("call-10"),
            json!({"result": "ok"}),
        )),
    );

    let provenance =
        ItemProvenance::new(AgentId::new("specialist-coder")).with_agent_name("Code Expert");
    item_with_metadata = item_with_metadata.with_provenance(provenance);

    let raw_provider = RawProviderItem::new(
        "openai",
        json!({"id": "call_123", "type": "function", "function": {"name": "test"}}),
    );
    item_with_metadata = item_with_metadata.with_raw_provider_item(raw_provider);

    item_with_metadata =
        item_with_metadata.with_session_data("host_trace_id", json!("trace-uuid-999"));

    // Route the item through serde first, so it carries a field this build has never heard of.
    let raw_json = serde_json::to_value(&item_with_metadata).unwrap();
    let mut obj = raw_json.as_object().unwrap().clone();
    obj.insert("future_extension_field".to_string(), json!({"flag": true}));
    let enriched_json = serde_json::Value::Object(obj);

    let parsed_item: RunItem = serde_json::from_value(enriched_json).unwrap();
    assert!(
        parsed_item
            .unknown()
            .get("future_extension_field")
            .is_some()
    );

    session
        .add_items(vec![parsed_item.clone()])
        .await
        .expect("add_items should succeed");

    let retrieved = session
        .get_items(None)
        .await
        .expect("get_items should succeed");
    assert_eq!(retrieved.len(), 1);
    let item = &retrieved[0];

    assert_eq!(item.id().as_str(), "item-complex");
    assert_eq!(item.kind().label(), "tool_call_output");

    let prov = item.provenance().expect("provenance must survive");
    assert_eq!(prov.agent_id().as_str(), "specialist-coder");
    assert_eq!(prov.agent_name(), Some("Code Expert"));

    let raw = item
        .raw_provider_item()
        .expect("raw_provider_item must survive");
    assert_eq!(raw.provider(), "openai");
    assert_eq!(raw.payload()["id"], "call_123");

    assert_eq!(
        item.session_data().get("host_trace_id"),
        Some(&json!("trace-uuid-999"))
    );

    assert_eq!(
        item.unknown().get("future_extension_field"),
        Some(&json!({"flag": true}))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_in_memory_session_concurrent_access() {
    let session_id = SessionId::generate();
    let session: Arc<dyn Session> = Arc::new(InMemorySession::new(session_id.clone()));
    let barrier = Arc::new(Barrier::new(10));

    let mut handles = Vec::new();
    for i in 0..10 {
        let s = Arc::clone(&session);
        let b = Arc::clone(&barrier);
        let handle = tokio::spawn(async move {
            // Release every task into the critical section at once, so the writers genuinely
            // contend rather than queuing up behind each other's scheduling.
            b.wait().await;
            let item = make_item(
                &format!("concurrent-item-{i}"),
                RunItemKind::Message(Message::user(format!("Message {i}"))),
            );
            s.add_items(vec![item]).await.unwrap();
            let items = s.get_items(Some(1)).await.unwrap();
            assert!(!items.is_empty());
        });
        handles.push(handle);
    }

    for handle in handles {
        handle.await.unwrap();
    }

    let all_items = session.get_items(None).await.unwrap();
    assert_eq!(all_items.len(), 10);
    assert_eq!(session.session_id(), &session_id);
}

#[tokio::test]
async fn test_ra_session_re_exports_and_contract() {
    let sid = SessionId::generate();
    let session: Box<dyn Session> = Box::new(InMemorySession::new(sid.clone()));

    assert_eq!(session.session_id(), &sid);

    let item = RunItem::new(
        ItemId::new("sess-msg-1"),
        RunItemKind::Message(Message::user("Hello through ra-session")),
    );

    session.add_items(vec![item.clone()]).await.unwrap();

    let items = session.get_items(None).await.unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0], item);

    let popped = session.pop_item().await.unwrap();
    assert_eq!(popped, Some(item));

    assert!(session.get_items(None).await.unwrap().is_empty());
}

/// Consumer logic written against `&dyn Session` alone, so the two implementations below are
/// exercised by exactly the same code.
async fn execute_session_roundtrip(session: &dyn Session) -> Vec<RunItem> {
    let seed = sample_items();

    session.add_items(seed[..6].to_vec()).await.unwrap();
    session.add_items(seed[6..].to_vec()).await.unwrap();

    let tail = session.get_items(Some(3)).await.unwrap();
    assert_eq!(tail, seed[seed.len() - 3..].to_vec());

    let popped = session.pop_item().await.unwrap();
    assert_eq!(popped.as_ref(), seed.last());

    session.get_items(None).await.unwrap()
}

#[tokio::test]
async fn test_session_implementation_interchangeability() {
    let in_memory = InMemorySession::new(SessionId::generate());
    let custom_stub = RecordingSession::new(SessionId::generate());

    let result_in_memory = execute_session_roundtrip(&in_memory).await;
    let result_custom = execute_session_roundtrip(&custom_stub).await;

    assert_eq!(result_in_memory.len(), sample_items().len() - 1);
    assert_eq!(result_in_memory, result_custom);

    // Same consumer, same traffic: two adds and two gets reached the implementation.
    assert_eq!(custom_stub.add_call_count(), 2);
    assert_eq!(custom_stub.get_call_count(), 2);

    let dyn_sessions: Vec<Arc<dyn Session>> = vec![Arc::new(in_memory), Arc::new(custom_stub)];
    for session in dyn_sessions {
        session.clear().await.unwrap();
        assert!(session.get_items(None).await.unwrap().is_empty());
    }
}
