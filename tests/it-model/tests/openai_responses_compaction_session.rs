//! Responses compaction session parity, transport, mutation recovery, and runner integration.
//!
//! Ported from the reference's compaction session, model visibility, and suffix test suites.
//! HTTP mocks assert the provider wire rather than replacing its client with a mock object.

use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use ra_core::{
    agent::{AgentId, AgentSpec},
    cancel::{CancelReason, CancelScope},
    error::{Error, Result},
    item::{
        CallId, ContentBlock, ItemId, Message, MessageRole, ModelInputItem, ModelResponse,
        OutputPhase, Reasoning, RunItem, RunItemKind, ToolCall, ToolCallOutput,
    },
    model::{
        ApiProtocol, Model, ModelRequest, ModelResolver, ModelSelector, ModelSettings, ProviderKey,
        ResolvedModel,
    },
    session::{
        CompactionSnapshot, CompactionSnapshotReplacement, Session, SessionCompaction,
        SessionCompactionContext, SessionId, SessionSettings,
    },
    state::RunId,
    tool::{Tool, ToolContext, ToolOptions, ToolOrigin, ToolOutput, ToolSchema},
};
use ra_model::openai::{
    auth::OpenAiAuth,
    compaction::{
        OpenAiResponsesCompactionArgs as Args, OpenAiResponsesCompactionMode as Mode,
        OpenAiResponsesCompactionSession as CompactSession, is_openai_model_name,
        select_compaction_candidate_items,
    },
    conversations::OpenAiConversationsSession,
};
use ra_runtime::{
    agent::AgentBinding,
    runner::{RunConfig, RunRequest, Runner},
};
use ra_session::{InMemorySession, SqliteSession};
use serde_json::{Value, json};
use tokio::sync::Notify;
use wiremock::{
    Mock, MockServer, Request, Respond, ResponseTemplate,
    matchers::{method, path},
};

fn user(id: &str, text: &str) -> RunItem {
    RunItem::new(ItemId::new(id), RunItemKind::Message(Message::user(text)))
}
fn assistant(id: &str, text: &str) -> RunItem {
    RunItem::new(
        ItemId::new(id),
        RunItemKind::Message(Message::assistant(text, OutputPhase::Final)),
    )
}
fn call(id: &str) -> RunItem {
    RunItem::new(
        ItemId::new(format!("item-{id}")),
        RunItemKind::ToolCall(ToolCall::new(CallId::new(id), "lookup", json!({}))),
    )
}
fn output(id: &str) -> RunItem {
    RunItem::new(
        ItemId::new(format!("output-{id}")),
        RunItemKind::ToolCallOutput(ToolCallOutput::new(CallId::new(id), json!("local output"))),
    )
}
fn compact_output() -> Value {
    json!({"output": [{"type":"compaction", "id":"cmp-1", "encrypted_content":"opaque", "created_by":"internal"}], "usage":{"input_tokens":12,"output_tokens":3,"input_tokens_details":{"cached_tokens":4}}})
}
fn force() -> Args {
    Args {
        force: true,
        ..Default::default()
    }
}

struct Api {
    server: MockServer,
    replies: Arc<Mutex<VecDeque<ResponseTemplate>>>,
}
struct Reply(Arc<Mutex<VecDeque<ResponseTemplate>>>);
impl Respond for Reply {
    fn respond(&self, _request: &Request) -> ResponseTemplate {
        self.0
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| ResponseTemplate::new(200).set_body_json(compact_output()))
    }
}
impl Api {
    async fn new() -> Self {
        let server = MockServer::start().await;
        let replies = Arc::new(Mutex::new(VecDeque::new()));
        Mock::given(method("POST"))
            .and(path("/v1/responses/compact"))
            .respond_with(Reply(replies.clone()))
            .mount(&server)
            .await;
        Self { server, replies }
    }
    fn auth(&self) -> OpenAiAuth {
        OpenAiAuth::new("test-key").with_base_url(format!("{}/v1", self.server.uri()))
    }
    fn wrap(&self, backend: Arc<dyn Session>) -> CompactSession {
        CompactSession::new("compact", backend, self.auth()).unwrap()
    }
    fn reply(&self, reply: ResponseTemplate) {
        self.replies.lock().unwrap().push_back(reply);
    }
    async fn bodies(&self) -> Vec<Value> {
        self.server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|request| serde_json::from_slice(&request.body).unwrap())
            .collect()
    }
    async fn wait_request(&self) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if !self.server.received_requests().await.unwrap().is_empty() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }
}

async fn evidence(
    session: &CompactSession,
    input: Vec<RunItem>,
    response_id: &str,
    stored: bool,
) -> SessionCompactionContext {
    let (_, generation) = session.get_items_with_generation(Some(0)).await.unwrap();
    let mut model_exchange = Vec::new();
    for item in input {
        model_exchange.push(
            session
                .model_item_digest(&item.to_model_input().unwrap())
                .await
                .unwrap(),
        );
    }
    SessionCompactionContext::new(
        generation,
        model_exchange,
        Some(response_id.into()),
        Some(stored),
    )
}

#[test]
fn model_validation_matches_reference() {
    for name in [
        "gpt-4.1",
        "gpt-5.4",
        "o1",
        "o3-mini",
        "ft:gpt-4.1:org:project:suffix",
        "ft:o3:org",
        " gpt-x ",
    ] {
        assert!(is_openai_model_name(name), "{name}");
    }
    for name in ["", " ", "claude", "gemini", "o", "other", "ft:claude:org"] {
        assert!(!is_openai_model_name(name), "{name}");
    }
}

#[test]
fn candidate_selection_excludes_both_user_shapes_and_compaction() {
    let items = vec![
        json!({"role":"user","content":"easy"}),
        json!({"type":"message","role":"user","content":[]}),
        json!({"type":"compaction","encrypted_content":"x"}),
        json!({"type":"reasoning"}),
        json!({"type":"function_call"}),
        json!({"role":"assistant","content":"answer"}),
    ];
    assert_eq!(select_compaction_candidate_items(&items), items[3..]);
}

#[tokio::test]
async fn constructor_defaults_and_invalid_options() {
    let api = Api::new().await;
    let backend = Arc::new(InMemorySession::new("base"));
    let session = api.wrap(backend.clone());
    assert_eq!(session.model(), "gpt-4.1");
    assert_eq!(session.compaction_mode(), Mode::Auto);
    assert_eq!(session.session_id().as_str(), "compact");
    assert!(session.clone().with_model("claude").is_err());
    assert!(session.clone().with_model("ft:gpt-4.1:org").is_ok());
    assert!(session.with_max_rollback_items(Some(0)).is_err());
    let remote = Arc::new(OpenAiConversationsSession::new("remote", api.auth()).unwrap());
    assert!(CompactSession::new("compact", remote, api.auth()).is_err());
    assert!(CompactSession::new("compact", backend.clone(), OpenAiAuth::new("")).is_err());
    assert!(CompactSession::new("compact", backend, OpenAiAuth::keyless("file:///local")).is_err());
}

#[tokio::test]
async fn get_add_pop_clear_delegate() {
    let api = Api::new().await;
    let backend = Arc::new(InMemorySession::new("base"));
    let session = api.wrap(backend.clone());
    let items = vec![user("1", "one"), assistant("2", "two")];
    session.add_items(items.clone()).await.unwrap();
    assert_eq!(session.get_items(None).await.unwrap(), items);
    assert_eq!(session.get_items(Some(1)).await.unwrap(), items[1..]);
    assert_eq!(session.pop_item().await.unwrap(), Some(items[1].clone()));
    session.clear().await.unwrap();
    assert!(backend.get_items(None).await.unwrap().is_empty());
    assert!(api.bodies().await.is_empty());
}

#[tokio::test]
async fn auto_without_response_and_explicit_input_send_local_history() {
    for mode in [Mode::Auto, Mode::Input] {
        let api = Api::new().await;
        let backend = Arc::new(InMemorySession::new_with_items(
            "base",
            vec![user("1", "hello")],
        ));
        let session = api.wrap(backend.clone()).with_compaction_mode(mode);
        session.run_compaction(Some(force())).await.unwrap();
        let body = &api.bodies().await[0];
        assert_eq!(body["model"], "gpt-4.1");
        assert!(body.get("previous_response_id").is_none());
        assert_eq!(body["input"][0]["content"][0]["text"], "hello");
        assert!(matches!(
            backend.get_items(None).await.unwrap()[0].kind(),
            RunItemKind::ProviderCompaction(_)
        ));
    }
}

#[tokio::test]
async fn previous_mode_requires_id_even_below_threshold() {
    let api = Api::new().await;
    let session = api
        .wrap(Arc::new(InMemorySession::new("base")))
        .with_compaction_mode(Mode::PreviousResponseId);
    assert!(
        session
            .run_compaction(None)
            .await
            .unwrap_err()
            .to_string()
            .contains("requires a response_id")
    );
    assert!(api.bodies().await.is_empty());
}

#[tokio::test]
async fn auto_stored_response_uses_chain_and_explicit_override_is_preserved() {
    let api = Api::new().await;
    let session = api.wrap(Arc::new(InMemorySession::new("base")));
    session
        .run_compaction(Some(Args {
            response_id: Some("resp-1".into()),
            store: Some(true),
            ..force()
        }))
        .await
        .unwrap();
    session
        .run_compaction(Some(Args {
            response_id: Some("resp-2".into()),
            compaction_mode: Some(Mode::PreviousResponseId),
            store: Some(false),
            ..force()
        }))
        .await
        .unwrap();
    let bodies = api.bodies().await;
    assert_eq!(
        bodies[0],
        json!({"model":"gpt-4.1","previous_response_id":"resp-1"})
    );
    // Explicit mode overrides store evidence in the reference; auto is the protective mode.
    assert_eq!(bodies[1]["previous_response_id"], "resp-2");
}

#[tokio::test]
async fn auto_remembers_unstored_response_until_explicit_store_true() {
    let api = Api::new().await;
    let session = api.wrap(Arc::new(InMemorySession::new("base")));
    session
        .run_compaction(Some(Args {
            response_id: Some("resp-1".into()),
            store: Some(false),
            ..force()
        }))
        .await
        .unwrap();
    session.run_compaction(Some(force())).await.unwrap();
    session
        .run_compaction(Some(Args {
            store: Some(true),
            ..force()
        }))
        .await
        .unwrap();
    let bodies = api.bodies().await;
    assert!(bodies[0].get("input").is_some());
    assert!(bodies[1].get("input").is_some());
    assert_eq!(bodies[2]["previous_response_id"], "resp-1");
}

#[tokio::test]
async fn default_threshold_counts_candidates_and_force_bypasses_hook() {
    let api = Api::new().await;
    let backend = Arc::new(InMemorySession::new_with_items(
        "base",
        (0..20)
            .map(|n| user(&format!("u{n}"), "question"))
            .collect(),
    ));
    let session = api.wrap(backend.clone());
    session.run_compaction(None).await.unwrap();
    assert!(api.bodies().await.is_empty());
    session
        .add_items(
            (0..9)
                .map(|n| assistant(&format!("a{n}"), "answer"))
                .collect(),
        )
        .await
        .unwrap();
    session.run_compaction(None).await.unwrap();
    assert!(api.bodies().await.is_empty());
    session
        .add_items(vec![assistant("a9", "tenth")])
        .await
        .unwrap();
    session.run_compaction(None).await.unwrap();
    assert_eq!(api.bodies().await.len(), 1);
    let session = session.with_should_trigger_compaction(|_| false);
    session.run_compaction(None).await.unwrap();
    assert_eq!(api.bodies().await.len(), 1);
    session.run_compaction(Some(force())).await.unwrap();
    assert_eq!(api.bodies().await.len(), 2);
}

#[tokio::test]
async fn empty_pop_retains_response_chain_but_pop_and_clear_revoke_it() {
    let api = Api::new().await;
    api.reply(ResponseTemplate::new(200).set_body_json(json!({"output":[]})));
    let session = api.wrap(Arc::new(InMemorySession::new("base")));
    session
        .run_compaction(Some(Args {
            response_id: Some("resp-1".into()),
            ..force()
        }))
        .await
        .unwrap();
    assert!(session.pop_item().await.unwrap().is_none());
    session.run_compaction(Some(force())).await.unwrap();
    assert_eq!(api.bodies().await[1]["previous_response_id"], "resp-1");
    session.pop_item().await.unwrap();
    session.run_compaction(Some(force())).await.unwrap();
    assert!(api.bodies().await[2].get("input").is_some());
    session
        .run_compaction(Some(Args {
            response_id: Some("resp-2".into()),
            ..force()
        }))
        .await
        .unwrap();
    session.clear().await.unwrap();
    session.run_compaction(Some(force())).await.unwrap();
    assert!(api.bodies().await[4].get("input").is_some());
}

#[tokio::test]
async fn rollback_budget_checks_full_history_before_request_including_force() {
    let api = Api::new().await;
    let original = vec![
        user("1", "old"),
        assistant("2", "answer"),
        user("3", "visible"),
    ];
    let backend = Arc::new(
        InMemorySession::new_with_items("base", original.clone())
            .with_session_settings(SessionSettings::new().with_limit(1)),
    );
    let session = api
        .wrap(backend.clone())
        .with_max_rollback_items(Some(2))
        .unwrap();
    assert!(
        session
            .run_compaction(Some(force()))
            .await
            .unwrap_err()
            .to_string()
            .contains("max_rollback_items")
    );
    assert_eq!(backend.get_items(Some(100)).await.unwrap(), original);
    assert!(api.bodies().await.is_empty());
    session
        .with_max_rollback_items(Some(3))
        .unwrap()
        .run_compaction(Some(force()))
        .await
        .unwrap();
    assert_eq!(api.bodies().await[0]["input"].as_array().unwrap().len(), 1);
    assert_eq!(backend.get_items(Some(100)).await.unwrap().len(), 1);
}

#[tokio::test]
async fn output_normalization_strips_internal_metadata_and_orphan_assistant_ids() {
    let api = Api::new().await;
    api.reply(ResponseTemplate::new(200).set_body_json(json!({"output":[
        {"type":"message","id":"msg-orphan","role":"assistant","content":[{"type":"output_text","text":"answer"}]},
        {"type":"message","role":"user","content":[{"type":"input_image","image_url":"https://example.com/image.png","file_id":"unused","detail":"high","extra":"drop"}]},
        {"type":"compaction","encrypted_content":"opaque","created_by":"internal"}
    ]})));
    let backend = Arc::new(InMemorySession::new("base"));
    let session = api.wrap(backend.clone());
    session.run_compaction(Some(force())).await.unwrap();
    let history = backend.get_items(None).await.unwrap();
    assert!(
        history[0]
            .raw_provider_item()
            .unwrap()
            .payload()
            .get("id")
            .is_none()
    );
    let image = &history[1].raw_provider_item().unwrap().payload()["content"][0];
    assert_eq!(
        image,
        &json!({"type":"input_image","image_url":"https://example.com/image.png","detail":"high"})
    );
    session.run_compaction(Some(force())).await.unwrap();
    assert!(
        api.bodies().await[1]["input"][2]
            .get("created_by")
            .is_none()
    );
}

#[tokio::test]
async fn output_reasoning_preserves_assistant_raw_id() {
    let api = Api::new().await;
    api.reply(ResponseTemplate::new(200).set_body_json(json!({"output":[
        {"type":"reasoning","encrypted_content":"reason","summary":[]},
        {"type":"message","id":"msg-paired","role":"assistant","content":[{"type":"output_text","text":"answer"}]}
    ]})));
    let backend = Arc::new(InMemorySession::new("base"));
    api.wrap(backend.clone())
        .run_compaction(Some(force()))
        .await
        .unwrap();
    let history = backend.get_items(None).await.unwrap();
    assert!(
        matches!(history[0].kind(), RunItemKind::Reasoning(reasoning) if reasoning.id().is_none())
    );
    assert_eq!(
        history[1].raw_provider_item().unwrap().payload()["id"],
        "msg-paired"
    );
}

#[tokio::test]
async fn invalid_output_preserves_history_and_reports_billed_usage() {
    for output in [
        json!([{"type":"message","role":"user","content":[{"type":"input_image","image_url":"","file_id":null}]}]),
        json!([{"type":"unknown"}]),
        json!("invalid"),
    ] {
        let api = Api::new().await;
        api.reply(
            ResponseTemplate::new(200).set_body_json(
                json!({"output":output,"usage":{"input_tokens":12,"output_tokens":3}}),
            ),
        );
        let original = vec![user("1", "keep")];
        let backend = Arc::new(InMemorySession::new_with_items("base", original.clone()));
        let outcome = api
            .wrap(backend.clone())
            .run_compaction_with_usage(Some(force()))
            .await;
        assert_eq!(outcome.usage().total_tokens(), 15);
        assert!(outcome.into_result().is_err());
        assert_eq!(backend.get_items(None).await.unwrap(), original);
    }
}

#[tokio::test]
async fn transport_headers_and_provider_errors_are_shared() {
    let api = Api::new().await;
    api.reply(
        ResponseTemplate::new(429)
            .insert_header("retry-after", "1")
            .set_body_json(json!({"error":{"message":"rate limited"}})),
    );
    let session = CompactSession::new(
        "compact",
        Arc::new(InMemorySession::new("base")),
        api.auth()
            .with_organization("org-test")
            .with_project("proj-test")
            .with_default_header("x-test", "yes"),
    )
    .unwrap();
    let error = session.run_compaction(Some(force())).await.unwrap_err();
    assert!(error.to_string().contains("rate limited"));
    let requests = api.server.received_requests().await.unwrap();
    assert_eq!(requests[0].headers["authorization"], "Bearer test-key");
    assert_eq!(requests[0].headers["openai-organization"], "org-test");
    assert_eq!(requests[0].headers["openai-project"], "proj-test");
    assert_eq!(requests[0].headers["x-test"], "yes");
}

/// A legacy store exposing failures both before and after a committed mutation.
struct FaultSession {
    inner: InMemorySession,
    fail_compacted_add: AtomicUsize,
    fail_clear: AtomicUsize,
    fail_tool_output_add: AtomicUsize,
    clear_commits: bool,
    gate_compacted_add: bool,
    compacted_add_started: Notify,
    release_compacted_add: Notify,
}
impl FaultSession {
    fn new(items: Vec<RunItem>) -> Self {
        Self {
            inner: InMemorySession::new_with_items("fault", items),
            fail_compacted_add: AtomicUsize::new(0),
            fail_clear: AtomicUsize::new(0),
            fail_tool_output_add: AtomicUsize::new(0),
            clear_commits: false,
            gate_compacted_add: false,
            compacted_add_started: Notify::new(),
            release_compacted_add: Notify::new(),
        }
    }
}
#[async_trait]
impl Session for FaultSession {
    fn session_id(&self) -> &SessionId {
        self.inner.session_id()
    }
    async fn get_items(&self, limit: Option<usize>) -> Result<Vec<RunItem>> {
        self.inner.get_items(limit).await
    }
    async fn add_items(&self, items: Vec<RunItem>) -> Result<()> {
        let compacted = items
            .iter()
            .any(|item| matches!(item.kind(), RunItemKind::ProviderCompaction(_)));
        if compacted && self.gate_compacted_add {
            self.compacted_add_started.notify_one();
            self.release_compacted_add.notified().await;
        }
        if compacted
            && self
                .fail_compacted_add
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                .is_ok()
        {
            self.inner.add_items(items).await?;
            return Err(Error::caller("replacement add reply lost"));
        }
        if items
            .iter()
            .any(|item| matches!(item.kind(), RunItemKind::ToolCallOutput(_)))
            && self
                .fail_tool_output_add
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                .is_ok()
        {
            self.inner.add_items(items).await?;
            return Err(Error::caller("tool append reply lost"));
        }
        self.inner.add_items(items).await
    }
    async fn pop_item(&self) -> Result<Option<RunItem>> {
        self.inner.pop_item().await
    }
    async fn clear(&self) -> Result<()> {
        if self
            .fail_clear
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
        {
            if self.clear_commits {
                self.inner.clear().await?;
            }
            return Err(Error::caller("clear failed"));
        }
        self.inner.clear().await
    }
}

#[tokio::test]
async fn replacement_add_failure_restores_full_original_history() {
    let api = Api::new().await;
    let original = vec![user("1", "one"), assistant("2", "two")];
    let backend = Arc::new(FaultSession::new(original.clone()));
    backend.fail_compacted_add.store(1, Ordering::SeqCst);
    let outcome = api
        .wrap(backend.clone())
        .run_compaction_with_usage(Some(force()))
        .await;
    assert_eq!(outcome.usage().total_tokens(), 15);
    assert!(
        outcome
            .into_result()
            .unwrap_err()
            .to_string()
            .contains("replacement add reply lost")
    );
    assert_eq!(backend.get_items(None).await.unwrap(), original);
}

#[tokio::test]
async fn clear_failure_before_or_after_commit_restores_without_duplicating() {
    for committed in [false, true] {
        let api = Api::new().await;
        let original = vec![user("1", "one"), assistant("2", "two")];
        let mut backend = FaultSession::new(original.clone());
        backend.clear_commits = committed;
        backend.fail_clear.store(1, Ordering::SeqCst);
        let backend = Arc::new(backend);
        assert!(
            api.wrap(backend.clone())
                .run_compaction(Some(force()))
                .await
                .is_err()
        );
        assert_eq!(backend.get_items(None).await.unwrap(), original);
    }
}

#[tokio::test]
async fn dropped_legacy_replacement_restores_before_a_new_wrapper_append() {
    let api = Api::new().await;
    let original = vec![user("1", "original")];
    let mut backend = FaultSession::new(original.clone());
    backend.gate_compacted_add = true;
    let backend = Arc::new(backend);
    let session = Arc::new(api.wrap(backend.clone()));
    let compacting = tokio::spawn({
        let session = session.clone();
        async move { session.run_compaction(Some(force())).await }
    });
    tokio::time::timeout(
        Duration::from_secs(5),
        backend.compacted_add_started.notified(),
    )
    .await
    .unwrap();
    compacting.abort();
    let _ = compacting.await;
    backend.release_compacted_add.notify_one();
    session.add_items(vec![user("2", "newer")]).await.unwrap();
    let mut expected = original;
    expected.push(user("2", "newer"));
    assert_eq!(backend.get_items(None).await.unwrap(), expected);
}

#[tokio::test]
async fn concurrent_wrapper_mutations_wait_for_compaction_and_survive() {
    for clear in [false, true] {
        let api = Api::new().await;
        api.reply(
            ResponseTemplate::new(200)
                .set_body_json(compact_output())
                .set_delay(Duration::from_millis(150)),
        );
        let backend = Arc::new(InMemorySession::new_with_items(
            "base",
            vec![user("1", "original")],
        ));
        let session = Arc::new(api.wrap(backend.clone()));
        let compacting = tokio::spawn({
            let session = session.clone();
            async move { session.run_compaction(Some(force())).await }
        });
        api.wait_request().await;
        let mut writing = tokio::spawn({
            let session = session.clone();
            async move {
                if clear {
                    session.clear().await
                } else {
                    session.add_items(vec![user("2", "newer")]).await
                }
            }
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut writing)
                .await
                .is_err()
        );
        compacting.await.unwrap().unwrap();
        writing.await.unwrap().unwrap();
        let items = backend.get_items(None).await.unwrap();
        if clear {
            assert!(items.is_empty());
        } else {
            assert_eq!(items.len(), 2);
            assert_eq!(items[1], user("2", "newer"));
        }
    }
}

#[tokio::test]
async fn local_tool_outputs_defer_across_turns_and_force_only_after_reaching_model() {
    let api = Api::new().await;
    let backend = Arc::new(InMemorySession::new_with_items(
        "base",
        vec![user("1", "question"), call("c1"), output("c1")],
    ));
    let calls = Arc::new(AtomicUsize::new(0));
    let session = api.wrap(backend.clone()).with_should_trigger_compaction({
        let calls = calls.clone();
        move |decision| {
            assert_eq!(decision.compaction_mode(), Mode::Input);
            calls.fetch_add(1, Ordering::SeqCst) == 0
        }
    });
    let context = evidence(
        &session,
        backend.get_items(None).await.unwrap(),
        "resp-tool-1",
        false,
    )
    .await;
    session
        .after_turn(&context, true, &CancelScope::root())
        .await
        .into_result()
        .unwrap();
    assert_eq!(
        session.deferred_compaction_response_id().await.as_deref(),
        Some("resp-tool-1")
    );
    let context = evidence(
        &session,
        backend.get_items(None).await.unwrap(),
        "resp-tool-2",
        false,
    )
    .await;
    session
        .after_turn(&context, true, &CancelScope::root())
        .await
        .into_result()
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(api.bodies().await.is_empty());
    session
        .add_items(vec![assistant("a1", "done")])
        .await
        .unwrap();
    let context = evidence(
        &session,
        backend.get_items(None).await.unwrap(),
        "resp-final",
        false,
    )
    .await;
    session
        .after_turn(&context, false, &CancelScope::root())
        .await
        .into_result()
        .unwrap();
    assert_eq!(api.bodies().await.len(), 1);
    assert!(session.deferred_compaction_response_id().await.is_none());
}

#[tokio::test]
async fn failed_forced_compaction_retains_marker_for_retry() {
    let api = Api::new().await;
    api.reply(ResponseTemplate::new(500).set_body_json(json!({"error":{"message":"try again"}})));
    let backend = Arc::new(InMemorySession::new_with_items(
        "base",
        vec![call("c1"), output("c1")],
    ));
    let hook_calls = Arc::new(AtomicUsize::new(0));
    let session = api.wrap(backend.clone()).with_should_trigger_compaction({
        let calls = hook_calls.clone();
        move |_| calls.fetch_add(1, Ordering::SeqCst) == 0
    });
    let context = evidence(
        &session,
        backend.get_items(None).await.unwrap(),
        "resp-tool",
        false,
    )
    .await;
    session
        .after_turn(&context, true, &CancelScope::root())
        .await
        .into_result()
        .unwrap();
    let context = evidence(
        &session,
        backend.get_items(None).await.unwrap(),
        "resp-final",
        false,
    )
    .await;
    assert!(
        session
            .after_turn(&context, false, &CancelScope::root())
            .await
            .into_result()
            .is_err()
    );
    assert_eq!(
        session.deferred_compaction_response_id().await.as_deref(),
        Some("resp-tool")
    );
    session
        .after_turn(&context, false, &CancelScope::root())
        .await
        .into_result()
        .unwrap();
    assert_eq!(api.bodies().await.len(), 2);
    assert!(session.deferred_compaction_response_id().await.is_none());
}

#[tokio::test]
async fn automatic_legacy_compaction_requires_complete_ordered_coverage() {
    for visible in [
        vec![user("u", "visible"), assistant("a", "answer")],
        vec![user("u", "same"), assistant("a", "answer")],
        vec![
            assistant("a", "answer"),
            user("u", "same"),
            user("u", "same"),
        ],
    ] {
        let api = Api::new().await;
        let original = vec![
            user("hidden", "same"),
            user("u", "same"),
            assistant("a", "answer"),
        ];
        let backend = Arc::new(InMemorySession::new_with_items("base", original.clone()));
        let session = api
            .wrap(backend.clone())
            .with_should_trigger_compaction(|_| true);
        let context = evidence(&session, visible, "resp-final", false).await;
        session
            .after_turn(&context, false, &CancelScope::root())
            .await
            .into_result()
            .unwrap();
        assert!(api.bodies().await.is_empty());
        assert_eq!(backend.get_items(None).await.unwrap(), original);
    }
}

#[tokio::test]
async fn hidden_tool_output_remains_until_full_replay() {
    let api = Api::new().await;
    let original = vec![
        user("u", "question"),
        call("c1"),
        output("c1"),
        assistant("a", "answer"),
    ];
    let backend = Arc::new(SqliteSession::open_in_memory("base").unwrap());
    backend.add_items(original.clone()).await.unwrap();
    let session = api
        .wrap(backend.clone())
        .with_should_trigger_compaction(|_| true);
    let context = evidence(
        &session,
        vec![
            original[0].clone(),
            original[1].clone(),
            original[3].clone(),
        ],
        "resp-1",
        false,
    )
    .await;
    session
        .after_turn(&context, false, &CancelScope::root())
        .await
        .into_result()
        .unwrap();
    assert!(api.bodies().await.is_empty());
    assert_eq!(backend.get_items(None).await.unwrap(), original);
    let context = evidence(&session, original, "resp-2", false).await;
    session
        .after_turn(&context, false, &CancelScope::root())
        .await
        .into_result()
        .unwrap();
    assert_eq!(api.bodies().await.len(), 1);
    assert_eq!(backend.get_items(None).await.unwrap().len(), 1);
}

#[tokio::test]
async fn native_partial_compaction_preserves_prefix_and_ignores_whole_history_budget() {
    let api = Api::new().await;
    let original = vec![
        RunItem::new(
            ItemId::new("reason"),
            RunItemKind::Reasoning(Reasoning::new().with_id("rs-old")),
        ),
        assistant("old", "paired"),
        user("u", "visible"),
        assistant("a", "answer"),
    ];
    let backend = Arc::new(SqliteSession::open_in_memory("base").unwrap());
    backend.add_items(original.clone()).await.unwrap();
    let session = api
        .wrap(backend.clone())
        .with_max_rollback_items(Some(1))
        .unwrap()
        .with_should_trigger_compaction(|_| true);
    let context = evidence(&session, original[2..].to_vec(), "resp-final", true).await;
    session
        .after_turn(&context, false, &CancelScope::root())
        .await
        .into_result()
        .unwrap();
    let body = &api.bodies().await[0];
    assert!(body.get("previous_response_id").is_none());
    assert_eq!(body["input"].as_array().unwrap().len(), 2);
    let retained = backend.get_items(None).await.unwrap();
    assert_eq!(retained.len(), 3);
    assert_eq!(retained[..2], original[..2]);
}

#[tokio::test]
async fn partial_previous_response_mode_and_broken_tool_groups_preserve_history() {
    for previous_mode in [false, true] {
        let api = Api::new().await;
        let original = if previous_mode {
            vec![
                user("old", "hidden"),
                user("u", "visible"),
                assistant("a", "answer"),
            ]
        } else {
            vec![
                call("c1"),
                user("u", "visible"),
                output("c1"),
                assistant("a", "answer"),
            ]
        };
        let backend = Arc::new(SqliteSession::open_in_memory("base").unwrap());
        backend.add_items(original.clone()).await.unwrap();
        let session = api
            .wrap(backend.clone())
            .with_should_trigger_compaction(|_| true)
            .with_compaction_mode(if previous_mode {
                Mode::PreviousResponseId
            } else {
                Mode::Auto
            });
        let context = evidence(&session, original[1..].to_vec(), "resp-final", true).await;
        session
            .after_turn(&context, false, &CancelScope::root())
            .await
            .into_result()
            .unwrap();
        assert!(api.bodies().await.is_empty());
        assert_eq!(backend.get_items(None).await.unwrap(), original);
    }
}

#[tokio::test]
async fn decision_hook_must_approve_reloaded_snapshot() {
    let api = Api::new().await;
    let original = vec![user("secret", "withheld"), assistant("a", "visible")];
    let backend = Arc::new(
        SqliteSession::builder("base")
            .session_settings(SessionSettings::new().with_limit(1))
            .open()
            .unwrap(),
    );
    backend.add_items(original.clone()).await.unwrap();
    let decisions = Arc::new(Mutex::new(Vec::new()));
    let session = api.wrap(backend.clone()).with_should_trigger_compaction({
        let decisions = decisions.clone();
        move |decision| {
            decisions
                .lock()
                .unwrap()
                .push(decision.session_items().to_vec());
            !decision
                .session_items()
                .iter()
                .any(|item| item.to_string().contains("withheld"))
        }
    });
    let context = evidence(&session, original.clone(), "resp-final", false).await;
    session
        .after_turn(&context, false, &CancelScope::root())
        .await
        .into_result()
        .unwrap();
    assert_eq!(decisions.lock().unwrap().len(), 2);
    assert!(api.bodies().await.is_empty());
    assert_eq!(backend.get_items(Some(100)).await.unwrap(), original);
}

#[tokio::test]
async fn wrapper_mutation_revokes_automatic_generation_before_provider_call() {
    let api = Api::new().await;
    let backend = Arc::new(InMemorySession::new_with_items(
        "base",
        vec![user("u", "seen")],
    ));
    let session = api
        .wrap(backend.clone())
        .with_should_trigger_compaction(|_| true);
    let context = evidence(
        &session,
        backend.get_items(None).await.unwrap(),
        "resp-final",
        false,
    )
    .await;
    session
        .add_items(vec![user("new", "concurrent")])
        .await
        .unwrap();
    session
        .after_turn(&context, false, &CancelScope::root())
        .await
        .into_result()
        .unwrap();
    assert!(api.bodies().await.is_empty());
    assert_eq!(backend.get_items(None).await.unwrap().len(), 2);
}

#[tokio::test]
async fn native_backend_mutation_revokes_snapshot_after_provider_call() {
    let api = Api::new().await;
    api.reply(
        ResponseTemplate::new(200)
            .set_body_json(compact_output())
            .set_delay(Duration::from_millis(100)),
    );
    let original = vec![user("u", "seen")];
    let backend = Arc::new(SqliteSession::open_in_memory("base").unwrap());
    backend.add_items(original.clone()).await.unwrap();
    let session = Arc::new(
        api.wrap(backend.clone())
            .with_should_trigger_compaction(|_| true),
    );
    let context = evidence(&session, original, "resp-final", false).await;
    let compacting = tokio::spawn({
        let session = session.clone();
        async move {
            session
                .after_turn(&context, false, &CancelScope::root())
                .await
        }
    });
    api.wait_request().await;
    backend
        .add_items(vec![user("new", "concurrent")])
        .await
        .unwrap();
    let outcome = compacting.await.unwrap();
    assert_eq!(outcome.usage().total_tokens(), 15);
    outcome.into_result().unwrap();
    assert_eq!(backend.get_items(None).await.unwrap().len(), 2);
}

#[derive(Default)]
struct ScriptedModel {
    script: Mutex<VecDeque<ModelResponse>>,
    requests: Mutex<Vec<Vec<ModelInputItem>>>,
    gate_first: bool,
    started: Notify,
    release: Notify,
}
impl ScriptedModel {
    fn responses(responses: Vec<ModelResponse>) -> Self {
        Self {
            script: Mutex::new(responses.into()),
            ..Default::default()
        }
    }
}
#[async_trait]
impl Model for ScriptedModel {
    async fn get_response(&self, request: ModelRequest) -> Result<ModelResponse> {
        let first = {
            let mut requests = self.requests.lock().unwrap();
            let first = requests.is_empty();
            requests.push(request.input().to_vec());
            first
        };
        if self.gate_first && first {
            self.started.notify_one();
            self.release.notified().await;
        }
        self.script
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| Error::caller("script exhausted"))
    }
}
struct Resolver {
    model: Arc<ScriptedModel>,
    stored: bool,
}
impl ModelResolver for Resolver {
    fn resolve_model(&self, _: Option<&str>) -> Result<ResolvedModel> {
        Ok(ResolvedModel::new(
            ModelSelector::new(
                ProviderKey::new("openai"),
                Some("gpt-test".into()),
                ApiProtocol::OpenAiResponses,
            ),
            self.model.clone(),
            ModelSettings::new().with_extra_body(
                ProviderKey::new("openai"),
                [("store".into(), json!(self.stored))].into(),
            ),
            ModelSettings::new(),
        ))
    }
}
struct Lookup {
    origin: ToolOrigin,
    schema: ToolSchema,
    calls: AtomicUsize,
}
impl Lookup {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            origin: ToolOrigin::new("lookup").unwrap(),
            schema: ToolSchema::new(
                "lookup",
                json!({"type":"object","properties":{},"required":[],"additionalProperties":false}),
            )
            .unwrap(),
            calls: AtomicUsize::new(0),
        })
    }
}
#[async_trait]
impl Tool for Lookup {
    fn origin(&self) -> &ToolOrigin {
        &self.origin
    }
    fn schema(&self) -> &ToolSchema {
        &self.schema
    }
    fn options(&self) -> ToolOptions {
        ToolOptions::new()
    }
    async fn call(&self, _: ToolContext<'_>) -> Result<ToolOutput> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ToolOutput::text("local output"))
    }
}
fn response(id: &str, items: Vec<RunItem>) -> ModelResponse {
    ModelResponse::new(items)
        .with_response_id(id)
        .with_usage(ra_core::usage::Usage::from_request(
            ra_core::usage::RequestUsage::new(2, 1),
        ))
}
fn request(
    model: Arc<ScriptedModel>,
    session: Arc<dyn Session>,
    input: Vec<ModelInputItem>,
    tools: Vec<Arc<dyn Tool>>,
    stored: bool,
) -> RunRequest {
    let agent = AgentSpec::builder()
        .id(AgentId::new("worker"))
        .name("Worker")
        .instructions("help")
        .tools(tools)
        .build()
        .unwrap();
    RunRequest::new(
        AgentBinding::direct(agent),
        Arc::new(Resolver { model, stored }),
        RunId::new("run-compact"),
        CancelScope::root(),
        input,
    )
    .with_session(session)
}

#[tokio::test]
async fn runner_compacts_in_both_delivery_modes_and_settles_usage() {
    for partial in [false, true] {
        for stored in [false, true] {
            let api = Api::new().await;
            let backend = Arc::new(InMemorySession::new("base"));
            let session = Arc::new(
                api.wrap(backend.clone())
                    .with_should_trigger_compaction(|_| true),
            );
            let model = Arc::new(ScriptedModel::responses(vec![response(
                "resp-final",
                vec![assistant("a", "done")],
            )]));
            let result = Runner::run(
                request(
                    model,
                    session,
                    vec![ModelInputItem::Message(Message::user("hello"))],
                    Vec::new(),
                    stored,
                )
                .with_config(RunConfig::new().with_partial_messages(partial)),
            )
            .await
            .unwrap();
            assert_eq!(result.usage().total_tokens(), 18);
            assert_eq!(result.usage().requests(), 2);
            assert_eq!(result.state().usage_totals(), &result.usage());
            assert_eq!(result.model_responses().len(), 1);
            assert!(result.state().pending_session_write().is_none());
            let bodies = api.bodies().await;
            assert_eq!(bodies.len(), 1);
            assert_eq!(bodies[0].get("previous_response_id").is_some(), stored);
            assert_eq!(backend.get_items(None).await.unwrap().len(), 1);
        }
    }
}

struct GatedNativeSession {
    inner: SqliteSession,
    started: Arc<Notify>,
    release: Arc<Notify>,
}
struct GatedReplacement {
    snapshot: CompactionSnapshot,
    started: Arc<Notify>,
    release: Arc<Notify>,
}
#[async_trait]
impl CompactionSnapshotReplacement for GatedReplacement {
    async fn replace_suffix(&self, start: usize, items: Vec<RunItem>) -> Result<bool> {
        self.started.notify_one();
        self.release.notified().await;
        self.snapshot.replace_suffix(start, items).await
    }
}
#[async_trait]
impl Session for GatedNativeSession {
    fn session_id(&self) -> &SessionId {
        self.inner.session_id()
    }
    async fn get_items(&self, limit: Option<usize>) -> Result<Vec<RunItem>> {
        self.inner.get_items(limit).await
    }
    async fn add_items(&self, items: Vec<RunItem>) -> Result<()> {
        self.inner.add_items(items).await
    }
    async fn pop_item(&self) -> Result<Option<RunItem>> {
        self.inner.pop_item().await
    }
    async fn clear(&self) -> Result<()> {
        self.inner.clear().await
    }
    async fn get_compaction_snapshot(&self, limit: usize) -> Result<Option<CompactionSnapshot>> {
        Ok(self
            .inner
            .get_compaction_snapshot(limit)
            .await?
            .map(|snapshot| {
                CompactionSnapshot::new(
                    snapshot.items().to_vec(),
                    snapshot.complete(),
                    Arc::new(GatedReplacement {
                        snapshot,
                        started: self.started.clone(),
                        release: self.release.clone(),
                    }),
                )
            }))
    }
}

#[tokio::test]
async fn cancellation_during_native_replacement_settles_usage_and_clears_pending_write() {
    let api = Api::new().await;
    let backend = Arc::new(GatedNativeSession {
        inner: SqliteSession::open_in_memory("base").unwrap(),
        started: Arc::new(Notify::new()),
        release: Arc::new(Notify::new()),
    });
    let session = Arc::new(
        api.wrap(backend.clone())
            .with_should_trigger_compaction(|_| true),
    );
    let model = Arc::new(ScriptedModel::responses(vec![response(
        "resp-final",
        vec![assistant("a", "done")],
    )]));
    let agent = AgentSpec::builder()
        .id(AgentId::new("worker"))
        .name("Worker")
        .instructions("help")
        .build()
        .unwrap();
    let cancel = CancelScope::root();
    let running = tokio::spawn(Runner::run(
        RunRequest::new(
            AgentBinding::direct(agent),
            Arc::new(Resolver {
                model,
                stored: false,
            }),
            RunId::new("native-cancel"),
            cancel.clone(),
            vec![ModelInputItem::Message(Message::user("hello"))],
        )
        .with_session(session),
    ));
    tokio::time::timeout(Duration::from_secs(5), backend.started.notified())
        .await
        .unwrap();
    cancel.cancel(CancelReason::UserInterrupt);
    backend.release.notify_one();
    let error = running.await.unwrap().unwrap_err();
    let state = error.run_state().unwrap();
    assert!(state.pending_session_write().is_none());
    assert_eq!(state.usage_totals().total_tokens(), 18);
    assert_eq!(backend.get_items(None).await.unwrap().len(), 1);
    assert_eq!(error.code(), "cancelled");
}

#[tokio::test]
async fn adapter_pruned_or_duplicate_calls_are_not_counted_as_model_visible() {
    for history in [
        vec![user("u", "question"), call("orphan")],
        vec![user("u", "question"), call("c1"), call("c1"), output("c1")],
    ] {
        let api = Api::new().await;
        let backend = Arc::new(InMemorySession::new_with_items("base", history.clone()));
        let session = Arc::new(
            api.wrap(backend.clone())
                .with_should_trigger_compaction(|_| true),
        );
        let model = Arc::new(ScriptedModel::responses(vec![response(
            "resp-final",
            vec![assistant("a", "done")],
        )]));
        let restored = history.iter().filter_map(RunItem::to_model_input).collect();
        Runner::run(
            request(model, session, Vec::new(), Vec::new(), false).with_config(
                RunConfig::new().with_context_filter(Arc::new(RestorePrunedCalls(restored))),
            ),
        )
        .await
        .unwrap();
        assert!(api.bodies().await.is_empty());
        assert_eq!(
            backend.get_items(None).await.unwrap()[..history.len()],
            history
        );
    }
}

struct RestorePrunedCalls(Vec<ModelInputItem>);
impl ra_core::filter::ContextFilter for RestorePrunedCalls {
    fn name(&self) -> &str {
        "restore_pruned_calls"
    }
    fn filter_model_input(
        &self,
        _: &ra_core::filter::ContextFilterRequest<'_>,
        data: ra_core::filter::ModelInputData,
    ) -> Result<ra_core::filter::ModelInputData> {
        Ok(data.with_input(self.0.clone()))
    }
}

#[tokio::test]
async fn runner_tool_outputs_defer_then_force_compaction_at_latest_response() {
    let api = Api::new().await;
    let backend = Arc::new(InMemorySession::new("base"));
    let hook_calls = Arc::new(AtomicUsize::new(0));
    let session = Arc::new(api.wrap(backend.clone()).with_should_trigger_compaction({
        let calls = hook_calls.clone();
        move |_| calls.fetch_add(1, Ordering::SeqCst) == 0
    }));
    let model = Arc::new(ScriptedModel::responses(vec![
        response("resp-tool", vec![call("c1")]),
        response("resp-final", vec![assistant("a", "done")]),
    ]));
    let tool = Lookup::new();
    let result = Runner::run(request(
        model.clone(),
        session.clone(),
        vec![ModelInputItem::Message(Message::user("hello"))],
        vec![tool.clone()],
        true,
    ))
    .await
    .unwrap();
    assert_eq!(tool.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        api.bodies().await,
        vec![json!({"model":"gpt-4.1","previous_response_id":"resp-final"})]
    );
    assert!(
        model.requests.lock().unwrap()[1]
            .iter()
            .any(|item| matches!(item, ModelInputItem::ToolCallOutput(_)))
    );
    assert!(session.deferred_compaction_response_id().await.is_none());
    assert_eq!(result.usage().total_tokens(), 21);
}

#[tokio::test]
async fn unlowerable_history_skips_optional_compaction_without_failing_the_run() {
    // The reference omits items it cannot fingerprint instead of failing. A thinking block from
    // another protocol cannot be lowered to the Responses wire. Neither the model turn nor
    // compaction may fail the run because of it; a legacy store still needs full coverage.
    for with_tool in [false, true] {
        let api = Api::new().await;
        let backend = Arc::new(InMemorySession::new("base"));
        let session = Arc::new(
            api.wrap(backend.clone())
                .with_should_trigger_compaction(|_| true),
        );
        let mut script = Vec::new();
        if with_tool {
            script.push(response("resp-tool", vec![call("c1")]));
        }
        script.push(response("resp-final", vec![assistant("a", "done")]));
        let model = Arc::new(ScriptedModel::responses(script));
        let tool = Lookup::new();
        let tools: Vec<Arc<dyn Tool>> = if with_tool {
            vec![tool.clone()]
        } else {
            Vec::new()
        };
        let thinking = Message::new(
            MessageRole::Assistant,
            vec![ContentBlock::thinking("earlier", "signature")],
        );
        let result = Runner::run(request(
            model.clone(),
            session.clone(),
            vec![
                ModelInputItem::Message(thinking),
                ModelInputItem::Message(Message::user("hello")),
            ],
            tools,
            true,
        ))
        .await
        .unwrap();
        assert_eq!(tool.calls.load(Ordering::SeqCst), usize::from(with_tool));
        assert!(result.state().pending_session_write().is_none());
        assert!(api.bodies().await.is_empty());
        assert_eq!(
            session.deferred_compaction_response_id().await.is_some(),
            with_tool
        );
        assert_eq!(
            backend.get_items(None).await.unwrap().len(),
            if with_tool { 5 } else { 3 }
        );
        // Manual compaction still reports that this history cannot be sent.
        assert!(session.run_compaction(Some(force())).await.is_err());
        assert!(api.bodies().await.is_empty());
    }
}

fn thinking_prefix() -> Vec<RunItem> {
    vec![
        user("old", "old question"),
        RunItem::new(
            ItemId::new("thinking"),
            RunItemKind::Message(Message::new(
                MessageRole::Assistant,
                vec![ContentBlock::thinking("old thinking", "signature")],
            )),
        ),
    ]
}

struct HideThinkingPrefix;
impl ra_core::filter::ContextFilter for HideThinkingPrefix {
    fn name(&self) -> &str {
        "hide_thinking_prefix"
    }
    fn filter_model_input(
        &self,
        _: &ra_core::filter::ContextFilterRequest<'_>,
        data: ra_core::filter::ModelInputData,
    ) -> Result<ra_core::filter::ModelInputData> {
        let input = data.input()
            .iter()
            .filter(|item| {
                !matches!(item, ModelInputItem::Message(message) if message.content().iter().any(|block| matches!(block, ContentBlock::Thinking(_))))
            })
            .cloned()
            .collect();
        Ok(data.with_input(input))
    }
}

#[tokio::test]
async fn runner_compacts_visible_suffix_after_hidden_unlowerable_prefix() {
    for with_tool in [false, true] {
        for stored in [false, true] {
            let api = Api::new().await;
            let original = thinking_prefix();
            let backend = Arc::new(SqliteSession::open_in_memory("base").unwrap());
            backend.add_items(original.clone()).await.unwrap();
            let views = Arc::new(Mutex::new(Vec::new()));
            let session = Arc::new(api.wrap(backend.clone()).with_should_trigger_compaction({
                let views = views.clone();
                move |decision| {
                    views.lock().unwrap().push((
                        decision.compaction_mode(),
                        decision.session_items().to_vec(),
                    ));
                    true
                }
            }));
            let mut script = Vec::new();
            if with_tool {
                script.push(response("resp-tool", vec![call("c1")]));
            }
            script.push(response("resp-final", vec![assistant("a", "done")]));
            let model = Arc::new(ScriptedModel::responses(script));
            let tool = Lookup::new();
            let tools: Vec<Arc<dyn Tool>> = if with_tool {
                vec![tool.clone()]
            } else {
                Vec::new()
            };
            let result = Runner::run(
                request(
                    model.clone(),
                    session.clone(),
                    vec![ModelInputItem::Message(Message::user("new question"))],
                    tools,
                    stored,
                )
                .with_config(RunConfig::new().with_context_filter(Arc::new(HideThinkingPrefix))),
            )
            .await
            .unwrap();
            assert_eq!(
                result.model_responses().len(),
                if with_tool { 2 } else { 1 }
            );
            assert_eq!(tool.calls.load(Ordering::SeqCst), usize::from(with_tool));
            assert!(model.requests.lock().unwrap().iter().all(|request| {
                !request.iter().any(|item| {
                    matches!(item, ModelInputItem::Message(message) if message.content().iter().any(|block| matches!(block, ContentBlock::Thinking(_))))
                })
            }));
            let bodies = api.bodies().await;
            assert_eq!(bodies.len(), 1);
            let input = bodies[0]["input"].as_array().unwrap();
            assert_eq!(input.len(), if with_tool { 4 } else { 2 });
            assert_eq!(input[0]["content"][0]["text"], "new question");
            assert!(bodies[0].get("previous_response_id").is_none());
            let views = views.lock().unwrap().clone();
            assert_eq!(views.len(), 2);
            assert_eq!(
                views[0].0,
                if stored {
                    Mode::PreviousResponseId
                } else {
                    Mode::Input
                }
            );
            assert!(
                views[0]
                    .1
                    .iter()
                    .any(|item| item.to_string().contains("old thinking"))
            );
            assert_eq!(views[1], (Mode::Input, input.clone()));
            let remaining = backend.get_items(None).await.unwrap();
            assert_eq!(remaining.len(), original.len() + 1);
            assert_eq!(remaining[..original.len()], original);
            assert!(session.deferred_compaction_response_id().await.is_none());
        }
    }
}

#[tokio::test]
async fn unlowerable_prefix_preserves_initial_and_suffix_hook_approval() {
    // Force bypasses only the initial decision; a different suffix must still be approved.
    for (force, approve_initial, expected_calls) in
        [(false, false, 1), (false, true, 2), (true, true, 1)]
    {
        let api = Api::new().await;
        let backend = Arc::new(SqliteSession::open_in_memory("base").unwrap());
        let mut original = thinking_prefix();
        let visible = vec![user("new", "new question"), assistant("a", "done")];
        original.extend(visible.clone());
        backend.add_items(original.clone()).await.unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let session = api.wrap(backend.clone()).with_should_trigger_compaction({
            let calls = calls.clone();
            move |decision| {
                calls.fetch_add(1, Ordering::SeqCst);
                assert_eq!(
                    decision.compaction_candidate_items().len(),
                    if decision.session_items().len() == 4 {
                        2
                    } else {
                        1
                    }
                );
                approve_initial && decision.session_items().len() == 4
            }
        });
        let context = evidence(&session, visible, "resp-final", true).await;
        if force {
            session
                .after_turn(&context, true, &CancelScope::root())
                .await
                .into_result()
                .unwrap();
            assert!(session.deferred_compaction_response_id().await.is_some());
            calls.store(0, Ordering::SeqCst);
        }
        session
            .after_turn(&context, false, &CancelScope::root())
            .await
            .into_result()
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), expected_calls);
        assert!(api.bodies().await.is_empty());
        assert_eq!(backend.get_items(None).await.unwrap(), original);
        assert_eq!(
            session.deferred_compaction_response_id().await.is_some(),
            force
        );
    }
}

#[tokio::test]
async fn unsupported_prefix_candidates_do_not_bypass_default_suffix_threshold() {
    let api = Api::new().await;
    let backend = Arc::new(SqliteSession::open_in_memory("base").unwrap());
    let mut original = thinking_prefix();
    original.extend((0..10).map(|index| assistant(&format!("old-{index}"), "old answer")));
    let visible = vec![user("new", "new question"), assistant("a", "done")];
    original.extend(visible.clone());
    backend.add_items(original.clone()).await.unwrap();
    let session = api.wrap(backend.clone());
    let context = evidence(&session, visible, "resp-final", true).await;
    session
        .after_turn(&context, false, &CancelScope::root())
        .await
        .into_result()
        .unwrap();
    assert!(api.bodies().await.is_empty());
    assert_eq!(backend.get_items(None).await.unwrap(), original);
}

#[tokio::test]
async fn runner_resumes_lost_append_reply_without_repeating_tool_and_still_defers_compaction() {
    let api = Api::new().await;
    let backend = Arc::new(FaultSession::new(Vec::new()));
    backend.fail_tool_output_add.store(1, Ordering::SeqCst);
    let session = Arc::new(
        api.wrap(backend.clone())
            .with_should_trigger_compaction(|_| true),
    );
    let model = Arc::new(ScriptedModel::responses(vec![
        response("resp-tool", vec![call("c1")]),
        response("resp-final", vec![assistant("a", "done")]),
    ]));
    let tool = Lookup::new();
    let error = Runner::run(request(
        model.clone(),
        session.clone(),
        vec![ModelInputItem::Message(Message::user("hello"))],
        vec![tool.clone()],
        false,
    ))
    .await
    .unwrap_err();
    let checkpoint = error.run_state().unwrap().clone();
    assert!(checkpoint.pending_session_write().is_some());
    assert!(api.bodies().await.is_empty());
    let checkpoint = serde_json::from_value(serde_json::to_value(checkpoint).unwrap()).unwrap();
    let result = Runner::run(
        request(
            model,
            session.clone(),
            Vec::new(),
            vec![tool.clone()],
            false,
        )
        .with_state(checkpoint),
    )
    .await
    .unwrap();
    assert_eq!(tool.calls.load(Ordering::SeqCst), 1);
    assert_eq!(api.bodies().await.len(), 1);
    assert!(session.deferred_compaction_response_id().await.is_none());
    assert_eq!(result.usage().total_tokens(), 18);
    assert_eq!(result.state().usage_totals().total_tokens(), 21);
}

struct HideOutputs;
impl ra_core::filter::ContextFilter for HideOutputs {
    fn name(&self) -> &str {
        "hide_tool_outputs"
    }
    fn filter_model_input(
        &self,
        _: &ra_core::filter::ContextFilterRequest<'_>,
        data: ra_core::filter::ModelInputData,
    ) -> Result<ra_core::filter::ModelInputData> {
        let input = data
            .input()
            .iter()
            .filter(|item| !matches!(item, ModelInputItem::ToolCallOutput(_)))
            .cloned()
            .collect();
        Ok(data.with_input(input))
    }
}

#[tokio::test]
async fn runner_filter_keeps_hidden_tool_outputs_local() {
    let api = Api::new().await;
    let original = vec![user("u", "question"), call("c-old"), output("c-old")];
    let backend = Arc::new(InMemorySession::new_with_items("base", original.clone()));
    let session = Arc::new(
        api.wrap(backend.clone())
            .with_should_trigger_compaction(|_| true),
    );
    let model = Arc::new(ScriptedModel::responses(vec![response(
        "resp-final",
        vec![assistant("a", "done")],
    )]));
    Runner::run(
        request(
            model.clone(),
            session,
            vec![ModelInputItem::Message(Message::user("next"))],
            Vec::new(),
            false,
        )
        .with_config(RunConfig::new().with_context_filter(Arc::new(HideOutputs))),
    )
    .await
    .unwrap();
    assert!(
        !model.requests.lock().unwrap()[0]
            .iter()
            .any(|item| matches!(item, ModelInputItem::ToolCallOutput(_)))
    );
    assert!(api.bodies().await.is_empty());
    assert_eq!(backend.get_items(None).await.unwrap()[..3], original);
}

#[tokio::test]
async fn runner_skips_compaction_after_interleaved_model_wait() {
    let api = Api::new().await;
    let backend = Arc::new(InMemorySession::new("base"));
    let session = Arc::new(
        api.wrap(backend.clone())
            .with_should_trigger_compaction(|_| true),
    );
    let model = Arc::new(ScriptedModel {
        gate_first: true,
        ..ScriptedModel::responses(vec![response("resp-final", vec![assistant("a", "done")])])
    });
    let running = tokio::spawn(Runner::run(request(
        model.clone(),
        session.clone(),
        vec![ModelInputItem::Message(Message::user("hello"))],
        Vec::new(),
        false,
    )));
    tokio::time::timeout(Duration::from_secs(5), model.started.notified())
        .await
        .unwrap();
    session
        .add_items(vec![user("other", "concurrent")])
        .await
        .unwrap();
    model.release.notify_one();
    running.await.unwrap().unwrap();
    assert!(api.bodies().await.is_empty());
    assert_eq!(backend.get_items(None).await.unwrap().len(), 3);
}

#[tokio::test]
async fn runner_compaction_failure_retains_acknowledged_batch_and_billed_usage() {
    let api = Api::new().await;
    api.reply(ResponseTemplate::new(200).set_body_json(
        json!({"output":[{"type":"unknown"}],"usage":{"input_tokens":12,"output_tokens":3}}),
    ));
    let backend = Arc::new(InMemorySession::new("base"));
    let session = Arc::new(
        api.wrap(backend.clone())
            .with_should_trigger_compaction(|_| true),
    );
    let model = Arc::new(ScriptedModel::responses(vec![response(
        "resp-final",
        vec![assistant("a", "done")],
    )]));
    let error = Runner::run(request(
        model,
        session,
        vec![ModelInputItem::Message(Message::user("hello"))],
        Vec::new(),
        false,
    ))
    .await
    .unwrap_err();
    let checkpoint = error.run_state().unwrap();
    assert!(
        checkpoint
            .pending_session_write()
            .unwrap()
            .append_acknowledged()
    );
    assert_eq!(checkpoint.usage_totals().total_tokens(), 18);
    assert_eq!(backend.get_items(None).await.unwrap().len(), 2);
    assert!(checkpoint.terminal_unrecoverable());
}

#[tokio::test]
async fn compacted_user_files_replay_source_priority_and_metadata() {
    for source in [
        json!({"file_data":"data:application/pdf;base64,cGRm", "file_url":"unused", "file_id":"unused"}),
        json!({"file_url":"https://example.com/report.pdf", "file_id":null}),
        json!({"file_id":"file-report", "file_url":null}),
    ] {
        let api = Api::new().await;
        let mut file = source;
        file["type"] = json!("input_file");
        file["filename"] = json!("report.pdf");
        file["detail"] = json!("high");
        file["extra"] = json!("discard");
        api.reply(ResponseTemplate::new(200).set_body_json(json!({"output":[{"type":"message","role":"user","content":[{"type":"input_text","text":"read this"}, file]}]})));
        let backend = Arc::new(InMemorySession::new("base"));
        let session = api.wrap(backend.clone());
        session.run_compaction(Some(force())).await.unwrap();
        let history = backend.get_items(None).await.unwrap();
        assert!(
            matches!(history[0].kind(), RunItemKind::Message(message) if message.content()[1].as_file().is_some())
        );
        let stored = history[0].raw_provider_item().unwrap().payload()["content"][1].clone();
        assert!(stored.get("extra").is_none());
        assert_eq!(stored["filename"], "report.pdf");
        assert_eq!(stored["detail"], "high");
        assert_eq!(
            ["file_data", "file_url", "file_id"]
                .into_iter()
                .filter(|key| stored.get(*key).is_some())
                .count(),
            1
        );
        // Persist and restore the typed envelope before replaying it: raw provider data alone
        // cannot be what makes the new model-input message sendable.
        let restored = serde_json::from_value(serde_json::to_value(history).unwrap()).unwrap();
        backend.clear().await.unwrap();
        backend.add_items(restored).await.unwrap();
        let session = api.wrap(backend.clone());
        session.run_compaction(Some(force())).await.unwrap();
        assert_eq!(api.bodies().await[1]["input"][0]["content"][1], stored);
    }
}

#[tokio::test]
async fn expired_rollback_history_is_not_revived_after_provider_wait() {
    let api = Api::new().await;
    api.reply(
        ResponseTemplate::new(200)
            .set_body_json(compact_output())
            .set_delay(Duration::from_millis(100)),
    );
    let backend = Arc::new(FaultSession::new(vec![
        user("expired", "old"),
        user("live", "current"),
    ]));
    backend.fail_compacted_add.store(1, Ordering::SeqCst);
    let session = Arc::new(api.wrap(backend.clone()));
    let compacting = tokio::spawn({
        let session = session.clone();
        async move { session.run_compaction(Some(force())).await }
    });
    api.wait_request().await;
    backend.inner.clear().await.unwrap();
    backend
        .inner
        .add_items(vec![user("live", "current")])
        .await
        .unwrap();
    assert!(compacting.await.unwrap().is_err());
    assert_eq!(
        backend.get_items(None).await.unwrap(),
        vec![user("live", "current")]
    );
}

/// A minimal third-party compaction port using the default neutral digest and retention policy.
struct CustomCompactionSession {
    inner: InMemorySession,
    appends: AtomicUsize,
    post_writes: AtomicUsize,
}
#[async_trait]
impl Session for CustomCompactionSession {
    fn session_id(&self) -> &SessionId {
        self.inner.session_id()
    }
    fn compaction(&self) -> Option<&dyn SessionCompaction> {
        Some(self)
    }
    async fn get_items(&self, limit: Option<usize>) -> Result<Vec<RunItem>> {
        self.inner.get_items(limit).await
    }
    async fn add_items(&self, items: Vec<RunItem>) -> Result<()> {
        self.appends.fetch_add(1, Ordering::SeqCst);
        self.inner.add_items(items).await
    }
    async fn pop_item(&self) -> Result<Option<RunItem>> {
        self.inner.pop_item().await
    }
    async fn clear(&self) -> Result<()> {
        self.inner.clear().await
    }
}
#[async_trait]
impl SessionCompaction for CustomCompactionSession {
    async fn after_turn(
        &self,
        _: &SessionCompactionContext,
        _: bool,
        _: &CancelScope,
    ) -> ra_core::session::SessionCompactionOutcome {
        let n = self.post_writes.fetch_add(1, Ordering::SeqCst);
        ra_core::session::SessionCompactionOutcome::new(
            ra_core::usage::Usage::default(),
            if n == 0 {
                Err(Error::caller("post-write failed"))
            } else {
                Ok(())
            },
        )
    }
}

#[tokio::test]
async fn acknowledged_append_retry_does_not_repeat_the_batch_or_completed_tool() {
    let session = Arc::new(CustomCompactionSession {
        inner: InMemorySession::new("custom"),
        appends: AtomicUsize::new(0),
        post_writes: AtomicUsize::new(0),
    });
    let model = Arc::new(ScriptedModel::responses(vec![
        response("resp-tool", vec![call("c1")]),
        response("resp-final", vec![assistant("a", "done")]),
    ]));
    let tool = Lookup::new();
    let error = Runner::run(request(
        model.clone(),
        session.clone(),
        vec![ModelInputItem::Message(Message::user("hello"))],
        vec![tool.clone()],
        false,
    ))
    .await
    .unwrap_err();
    let checkpoint = error.run_state().unwrap().clone();
    assert!(
        checkpoint
            .pending_session_write()
            .unwrap()
            .append_acknowledged()
    );
    let checkpoint = serde_json::from_value(serde_json::to_value(checkpoint).unwrap()).unwrap();
    Runner::run(
        request(
            model,
            session.clone(),
            Vec::new(),
            vec![tool.clone()],
            false,
        )
        .with_state(checkpoint),
    )
    .await
    .unwrap();
    assert_eq!(session.appends.load(Ordering::SeqCst), 3);
    assert_eq!(session.post_writes.load(Ordering::SeqCst), 3);
    assert_eq!(tool.calls.load(Ordering::SeqCst), 1);
    assert_eq!(session.get_items(None).await.unwrap().len(), 4);
}
