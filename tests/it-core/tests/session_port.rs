//! Contract tests for the `Session` port and for the three session identifier families that
//! must stay distinct.
//!
//! `ra-core` defines the port and ships no backing store for it, so the cases here run against a
//! stub declared in this file and assert only what the kernel owns: the shape of the four
//! methods, object safety, and that an implementation can live entirely outside `ra-core`.
//! Whether the reference in-memory implementation honours the contract is `it-session`'s job, in
//! `tests/it-session/tests/session_contract.rs`.

use std::{
    borrow::Borrow,
    collections::{BTreeSet, HashMap, HashSet},
    sync::{Arc, Mutex, PoisonError},
};

use async_trait::async_trait;
use ra_core::{
    error::Result,
    event::exec::ExecSessionId,
    item::{ItemId, Message, OutputPhase, RunItem, RunItemKind},
    model::{
        ConversationContinuation, ModelRequest, ModelSettings, ProviderConversationId, ProviderKey,
    },
    session::{Session, SessionId},
};

/// A session implemented outside `ra-core`, which is the only kind of implementation the kernel
/// crate admits.
#[derive(Debug)]
struct TestSession {
    session_id: SessionId,
    items: Mutex<Vec<RunItem>>,
}

impl TestSession {
    fn new(session_id: impl Into<SessionId>) -> Self {
        Self {
            session_id: session_id.into(),
            items: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl Session for TestSession {
    fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    async fn get_items(&self, limit: Option<usize>) -> Result<Vec<RunItem>> {
        let guard = self.items.lock().unwrap_or_else(PoisonError::into_inner);
        Ok(match limit {
            None => guard.clone(),
            Some(n) if n >= guard.len() => guard.clone(),
            Some(n) => guard[guard.len() - n..].to_vec(),
        })
    }

    async fn add_items(&self, items: Vec<RunItem>) -> Result<()> {
        let mut guard = self.items.lock().unwrap_or_else(PoisonError::into_inner);
        guard.extend(items);
        Ok(())
    }

    async fn pop_item(&self) -> Result<Option<RunItem>> {
        let mut guard = self.items.lock().unwrap_or_else(PoisonError::into_inner);
        Ok(guard.pop())
    }

    async fn clear(&self) -> Result<()> {
        let mut guard = self.items.lock().unwrap_or_else(PoisonError::into_inner);
        guard.clear();
        Ok(())
    }
}

fn make_item(id: &str, kind: RunItemKind) -> RunItem {
    RunItem::new(ItemId::new(id), kind)
}

#[test]
fn test_session_id_construction_and_formatting() {
    let id_str = "session-test-123";
    let session_id = SessionId::new(id_str);

    assert_eq!(session_id.as_str(), id_str);
    assert_eq!(format!("{session_id}"), id_str);
    assert_eq!(session_id.as_ref(), id_str);
    assert_eq!(Borrow::<str>::borrow(&session_id), id_str);

    let from_string = SessionId::from(id_str.to_string());
    assert_eq!(session_id, from_string);

    let from_str = SessionId::from(id_str);
    assert_eq!(session_id, from_str);

    let generated_1 = SessionId::generate();
    let generated_2 = SessionId::generate();
    assert_ne!(generated_1, generated_2);
    assert!(generated_1.as_str().starts_with("sess-"));
    assert!(generated_2.as_str().starts_with("sess-"));
}

#[test]
fn test_session_id_serde_transparent_roundtrip() {
    let id = SessionId::new("sess-uuid-456");
    let serialized = serde_json::to_string(&id).expect("serialization should succeed");
    assert_eq!(serialized, "\"sess-uuid-456\"");

    let deserialized: SessionId =
        serde_json::from_str(&serialized).expect("deserialization should succeed");
    assert_eq!(deserialized, id);
}

#[test]
fn test_session_id_collections_hash_and_ordering() {
    let id1 = SessionId::new("sess-a");
    let id2 = SessionId::new("sess-b");
    let id3 = SessionId::new("sess-a");

    let mut set = HashSet::new();
    set.insert(id1.clone());
    set.insert(id2.clone());
    set.insert(id3);
    assert_eq!(set.len(), 2);

    let mut btree = BTreeSet::new();
    btree.insert(id2.clone());
    btree.insert(id1.clone());
    let list: Vec<_> = btree.into_iter().collect();
    assert_eq!(list, vec![id1.clone(), id2.clone()]);

    let mut map = HashMap::new();
    map.insert(id1.clone(), 100);
    map.insert(id2.clone(), 200);
    assert_eq!(map.get(&id1), Some(&100));
    assert_eq!(map.get("sess-a"), Some(&100));
}

#[test]
fn test_session_identity_tripartite_separation() {
    let session_id = SessionId::new("hist-session-001");
    let exec_session_id = ExecSessionId::new("pty-exec-001");
    let provider_conv_id = ProviderConversationId::new("conv_remote_001");

    assert_eq!(session_id.as_str(), "hist-session-001");
    assert_eq!(exec_session_id.as_str(), "pty-exec-001");
    assert_eq!(provider_conv_id.as_str(), "conv_remote_001");

    let serialized_session = serde_json::to_string(&session_id).unwrap();
    let serialized_exec = serde_json::to_string(&exec_session_id).unwrap();
    let serialized_provider = serde_json::to_string(&provider_conv_id).unwrap();

    assert_eq!(serialized_session, "\"hist-session-001\"");
    assert_eq!(serialized_exec, "\"pty-exec-001\"");
    assert_eq!(serialized_provider, "\"conv_remote_001\"");

    // The provider-managed conversation slot on a model request takes the provider identifier and
    // nothing else, so a local session identifier cannot be assigned into it.
    let continuation = ConversationContinuation::ConversationId(provider_conv_id.clone());
    assert_eq!(continuation.conversation_id(), Some(&provider_conv_id));

    let empty = ModelSettings::new();
    let settings = empty.resolve(&ProviderKey::new("test"), &empty, &empty, &empty);
    let request =
        ModelRequest::new(vec![], settings).with_conversation_id(provider_conv_id.clone());
    assert_eq!(
        request.continuation().conversation_id(),
        Some(&provider_conv_id)
    );

    // The newtype is serde-transparent, so wrapping the wire string changes no payload.
    let continuation_json = serde_json::to_string(&continuation).unwrap();
    assert_eq!(
        continuation_json,
        "{\"type\":\"conversation_id\",\"id\":\"conv_remote_001\"}"
    );
    let parsed_continuation: ConversationContinuation =
        serde_json::from_str(&continuation_json).unwrap();
    assert_eq!(parsed_continuation, continuation);
}

#[tokio::test]
async fn test_session_port_is_object_safe_and_implementable_outside_ra_core() {
    let sid = SessionId::generate();
    let session: Arc<dyn Session> = Arc::new(TestSession::new(sid.clone()));

    assert_eq!(session.session_id(), &sid);
    assert!(session.get_items(None).await.unwrap().is_empty());
    assert!(session.pop_item().await.unwrap().is_none());

    let first = make_item("item-1", RunItemKind::Message(Message::user("Ask")));
    let second = make_item(
        "item-2",
        RunItemKind::Message(Message::assistant("Answer", OutputPhase::Final)),
    );
    session
        .add_items(vec![first.clone(), second.clone()])
        .await
        .unwrap();

    assert_eq!(
        session.get_items(None).await.unwrap(),
        vec![first.clone(), second.clone()]
    );
    assert_eq!(
        session.get_items(Some(1)).await.unwrap(),
        vec![second.clone()]
    );
    assert_eq!(session.pop_item().await.unwrap(), Some(second));

    session.clear().await.unwrap();
    assert!(session.get_items(None).await.unwrap().is_empty());
}

/// The port's `Send + Sync + 'static` bound is what lets a session be shared by the runtime, so
/// pin it here rather than leaving it to whichever implementation happens to satisfy it.
#[tokio::test]
async fn test_session_trait_object_crosses_a_task_boundary() {
    let sid = SessionId::generate();
    let session: Arc<dyn Session> = Arc::new(TestSession::new(sid.clone()));

    let moved = Arc::clone(&session);
    tokio::spawn(async move {
        moved
            .add_items(vec![make_item(
                "item-from-task",
                RunItemKind::Message(Message::user("From a spawned task")),
            )])
            .await
            .unwrap();
        assert_eq!(moved.session_id(), &sid);
    })
    .await
    .unwrap();

    let items = session.get_items(None).await.unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].id().as_str(), "item-from-task");
}
