//! `OpenAiConversationsSession` against a fake Conversations API.
//!
//! Ported from `openai-agents-python`'s `tests/memory/test_openai_conversations_session.py`, the
//! `OpenAIConversationsSession` cases of `prepare_input_with_session` and `save_result_to_session`
//! in `tests/test_agent_runner.py`, and its runner integration cases. The reference mocks its
//! client object; here a stateful fake server answers the HTTP calls, so a case asserts what was
//! sent over the wire and what the conversation holds afterwards. The reference hands the session
//! wire dicts and lets its runner clean them; this port hands it run records, so the cleaning
//! cases append through the session itself.

use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use ra_core::{
    agent::{AgentId, AgentSpec},
    cancel::CancelScope,
    item::{
        CallId, ItemId, McpApprovalRequest, Message, MessageRole, ModelInputItem, OutputPhase,
        Reasoning, RunItem, RunItemKind, ToolApproval, ToolCall, ToolCallOutput,
    },
    model::{
        ApiProtocol, Model, ModelResolver, ModelSelector, ModelSettings, ProviderConversationId,
        ProviderKey, ResolvedModel,
    },
    session::{Session, SessionSettings},
    state::RunId,
    tool::{Tool, ToolContext, ToolOptions, ToolOrigin, ToolOutput, ToolOutputBlock, ToolSchema},
};
use ra_model::openai::{
    auth::OpenAiAuth,
    conversations::{OpenAiConversationsSession, start_openai_conversations_session},
    responses::OpenAiResponsesModel,
};
use ra_runtime::{
    agent::AgentBinding,
    runner::{RunRequest, Runner, session_persistence::prepare_input_with_session},
};
use serde_json::{Value, json};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate, matchers::path_regex};

// ---------------------------------------------------------------------------------------------
// A fake Conversations API
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Route {
    Create,
    List,
    CreateItems,
    DeleteItem,
    Delete,
    Responses,
}

struct Failure {
    route: Route,
    status: u16,
    /// Apply the operation before failing: the server committed and the reply was lost.
    commit: bool,
    /// Calls on the route to let through first.
    after: usize,
}

#[derive(Default)]
struct State {
    conversations: BTreeMap<String, Vec<Value>>,
    created: usize,
    next_item: usize,
    page_size: Option<usize>,
    log: Vec<(Route, String, Instant)>,
    failures: VecDeque<Failure>,
    delays: BTreeMap<Route, Duration>,
    responses: VecDeque<Value>,
    response_bodies: Vec<Value>,
    created_items: Vec<(String, Vec<Value>)>,
}

#[derive(Clone)]
struct Fake {
    server: Arc<MockServer>,
    state: Arc<Mutex<State>>,
}

impl Fake {
    async fn start() -> Self {
        let server = MockServer::start().await;
        let state = Arc::new(Mutex::new(State::default()));
        Mock::given(path_regex("^/v1/"))
            .respond_with(Responder(Arc::clone(&state)))
            .mount(&server)
            .await;
        Self {
            server: Arc::new(server),
            state,
        }
    }

    fn auth(&self) -> OpenAiAuth {
        OpenAiAuth::new("test-key").with_base_url(format!("{}/v1", self.server.uri()))
    }

    fn session(&self) -> OpenAiConversationsSession {
        OpenAiConversationsSession::new("sess-test", self.auth()).unwrap()
    }

    /// Creates a conversation holding `items`, each given an id when it has none.
    fn seed(&self, conversation: &str, items: Vec<Value>) {
        let mut state = self.state.lock().unwrap();
        let stored = items
            .into_iter()
            .map(|item| state.store(item))
            .collect::<Vec<_>>();
        state.conversations.insert(conversation.to_owned(), stored);
    }

    fn items(&self, conversation: &str) -> Vec<Value> {
        self.state.lock().unwrap().conversations[conversation].clone()
    }

    fn has_conversation(&self, conversation: &str) -> bool {
        self.state
            .lock()
            .unwrap()
            .conversations
            .contains_key(conversation)
    }

    fn page_size(&self, size: usize) {
        self.state.lock().unwrap().page_size = Some(size);
    }

    fn delay(&self, route: Route, delay: Duration) {
        self.state.lock().unwrap().delays.insert(route, delay);
    }

    fn fail(&self, route: Route, status: u16, commit: bool) {
        self.fail_after(route, 0, status, commit);
    }

    fn fail_after(&self, route: Route, after: usize, status: u16, commit: bool) {
        self.state.lock().unwrap().failures.push_back(Failure {
            route,
            status,
            commit,
            after,
        });
    }

    fn respond_with(&self, payload: Value) {
        self.state.lock().unwrap().responses.push_back(payload);
    }

    fn calls(&self, route: Route) -> Vec<String> {
        self.state
            .lock()
            .unwrap()
            .log
            .iter()
            .filter(|(logged, _, _)| *logged == route)
            .map(|(_, target, _)| target.clone())
            .collect()
    }

    fn received_at(&self, route: Route) -> Vec<Instant> {
        self.state
            .lock()
            .unwrap()
            .log
            .iter()
            .filter(|(logged, _, _)| *logged == route)
            .map(|(_, _, at)| *at)
            .collect()
    }

    fn created_items(&self) -> Vec<(String, Vec<Value>)> {
        self.state.lock().unwrap().created_items.clone()
    }

    fn response_bodies(&self) -> Vec<Value> {
        self.state.lock().unwrap().response_bodies.clone()
    }

    /// Waits until the server has received a call on `route`.
    async fn received(&self, route: Route) {
        for _ in 0..500 {
            if !self.calls(route).is_empty() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        panic!("the server never received a {route:?} call");
    }
}

impl State {
    fn store(&mut self, mut item: Value) -> Value {
        if item.get("id").is_none() {
            self.next_item += 1;
            item["id"] = json!(format!("item_{}", self.next_item));
        }
        if matches!(
            item["type"].as_str(),
            Some("message" | "function_call" | "function_call_output")
        ) && item.get("status").is_none()
        {
            item["status"] = json!("completed");
        }
        item
    }

    fn list(&self, conversation: &str, request: &Request) -> Value {
        let query: BTreeMap<String, String> = request.url.query_pairs().into_owned().collect();
        assert!(
            !query.contains_key("limit"),
            "the session must not forward its bound as the page size"
        );
        let mut items = self.conversations[conversation].clone();
        if query.get("order").map(String::as_str) != Some("asc") {
            items.reverse();
        }
        let start = query.get("after").map_or(0, |after| {
            items
                .iter()
                .position(|item| item["id"] == json!(after))
                .map_or(items.len(), |index| index + 1)
        });
        let size = self.page_size.unwrap_or(20);
        let page: Vec<Value> = items.iter().skip(start).take(size).cloned().collect();
        json!({
            "object": "list",
            "data": page,
            "first_id": page.first().map(|item| item["id"].clone()),
            "last_id": page.last().map(|item| item["id"].clone()),
            "has_more": start + page.len() < items.len(),
        })
    }
}

/// A finished response as the frames a streamed turn arrives in.
fn sse_body(payload: &Value) -> String {
    let mut frames = vec![json!({
        "type": "response.created",
        "response": {"id": payload["id"], "status": "in_progress"}
    })];
    for (index, item) in payload["output"].as_array().unwrap().iter().enumerate() {
        frames.push(json!({
            "type": "response.output_item.done",
            "output_index": index,
            "item": item
        }));
    }
    frames.push(json!({"type": "response.completed", "response": payload}));
    frames
        .iter()
        .map(|frame| format!("data: {frame}\n\n"))
        .collect()
}

struct Responder(Arc<Mutex<State>>);

impl Respond for Responder {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let mut state = self.0.lock().unwrap();
        let segments: Vec<String> = request
            .url
            .path_segments()
            .unwrap()
            .skip(1)
            .map(str::to_owned)
            .collect();
        let segments: Vec<&str> = segments.iter().map(String::as_str).collect();
        let route = match (request.method.as_str(), segments.as_slice()) {
            ("POST", ["responses"]) => Route::Responses,
            ("POST", ["conversations"]) => Route::Create,
            ("GET", ["conversations", _, "items"]) => Route::List,
            ("POST", ["conversations", _, "items"]) => Route::CreateItems,
            ("DELETE", ["conversations", _, "items", _]) => Route::DeleteItem,
            ("DELETE", ["conversations", _]) => Route::Delete,
            other => panic!("unexpected request {other:?}"),
        };
        state.log.push((
            route,
            format!("{} {}", request.method, request.url.path()),
            Instant::now(),
        ));
        let delay = state.delays.get(&route).copied().unwrap_or_default();
        let failure = match state
            .failures
            .iter()
            .position(|failure| failure.route == route)
        {
            Some(index) if state.failures[index].after > 0 => {
                state.failures[index].after -= 1;
                None
            }
            Some(index) => state.failures.remove(index),
            None => None,
        };
        let failed = |status: u16| {
            ResponseTemplate::new(status)
                .set_body_json(
                    json!({"error": {"message": "injected failure", "type": "server_error"}}),
                )
                .set_delay(delay)
        };
        if let Some(failure) = &failure
            && !failure.commit
        {
            return failed(failure.status);
        }

        let body = match (route, segments.as_slice()) {
            (Route::Responses, _) => {
                let body: Value = request.body_json().unwrap();
                let streaming = body["stream"] == json!(true);
                state.response_bodies.push(body);
                let payload = state
                    .responses
                    .pop_front()
                    .expect("no scripted response left");
                if streaming {
                    return ResponseTemplate::new(200)
                        .set_body_raw(sse_body(&payload), "text/event-stream");
                }
                payload
            }
            (Route::Create, _) => {
                let body: Value = request.body_json().unwrap();
                assert_eq!(body, json!({"items": []}));
                state.created += 1;
                let id = format!("conv_{}", state.created);
                state.conversations.insert(id.clone(), Vec::new());
                json!({"id": id, "object": "conversation", "created_at": 0, "metadata": {}})
            }
            (Route::List, [_, conversation, _]) => {
                if !state.conversations.contains_key(*conversation) {
                    return ResponseTemplate::new(404);
                }
                state.list(conversation, request)
            }
            (Route::CreateItems, [_, conversation, _]) => {
                let body: Value = request.body_json().unwrap();
                let items = body["items"].as_array().unwrap().clone();
                state
                    .created_items
                    .push(((*conversation).to_owned(), items.clone()));
                let stored: Vec<Value> = items.into_iter().map(|item| state.store(item)).collect();
                state
                    .conversations
                    .get_mut(*conversation)
                    .expect("items posted to a missing conversation")
                    .extend(stored.clone());
                json!({"object": "list", "data": stored})
            }
            (Route::DeleteItem, [_, conversation, _, item]) => {
                let items = state.conversations.get_mut(*conversation).unwrap();
                let Some(index) = items.iter().position(|stored| stored["id"] == json!(item))
                else {
                    return ResponseTemplate::new(404);
                };
                items.remove(index);
                json!({"id": conversation, "object": "conversation"})
            }
            (Route::Delete, [_, conversation]) => {
                state.conversations.remove(*conversation);
                json!({"id": conversation, "object": "conversation.deleted", "deleted": true})
            }
            _ => unreachable!(),
        };
        if let Some(failure) = failure {
            return failed(failure.status);
        }
        ResponseTemplate::new(200)
            .set_body_json(body)
            .set_delay(delay)
    }
}

// ---------------------------------------------------------------------------------------------
// Items
// ---------------------------------------------------------------------------------------------

fn user(id: &str, text: &str) -> RunItem {
    RunItem::new(ItemId::new(id), RunItemKind::Message(Message::user(text)))
}

fn assistant(id: &str, text: &str) -> RunItem {
    RunItem::new(
        ItemId::new(id),
        RunItemKind::Message(Message::assistant(text, OutputPhase::Final)),
    )
}

fn reasoning(id: &str, reasoning: Reasoning) -> RunItem {
    RunItem::new(ItemId::new(id), RunItemKind::Reasoning(reasoning))
}

fn wire_user(text: &str) -> Value {
    json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": text}]})
}

fn wire_assistant(text: &str) -> Value {
    json!({
        "type": "message",
        "role": "assistant",
        "phase": "final_answer",
        "content": [{"type": "output_text", "text": text, "annotations": []}]
    })
}

fn text_of(item: &RunItem) -> String {
    match item.kind() {
        RunItemKind::Message(message) => message.text_content(),
        other => other.label().to_owned(),
    }
}

fn texts(items: &[RunItem]) -> Vec<String> {
    items.iter().map(text_of).collect()
}

fn stored_texts(items: &[Value]) -> Vec<String> {
    items
        .iter()
        .map(|item| {
            item["content"][0]["text"]
                .as_str()
                .map_or_else(|| item["type"].as_str().unwrap().to_owned(), str::to_owned)
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Starting a conversation
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn start_creates_an_empty_conversation() {
    let fake = Fake::start().await;
    let id = start_openai_conversations_session(&fake.auth())
        .await
        .unwrap();
    assert_eq!(id, ProviderConversationId::new("conv_1"));
    assert_eq!(fake.calls(Route::Create), ["POST /v1/conversations"]);
    assert!(fake.items("conv_1").is_empty());
}

#[tokio::test]
async fn an_existing_conversation_id_is_used_without_creating_one() {
    let fake = Fake::start().await;
    fake.seed("existing_id", vec![wire_user("hello")]);
    let session = fake.session().with_conversation_id("existing_id");
    assert_eq!(texts(&session.get_items(None).await.unwrap()), ["hello"]);
    assert!(fake.calls(Route::Create).is_empty());
    assert_eq!(
        session.conversation_id(),
        Some(ProviderConversationId::new("existing_id"))
    );
}

#[tokio::test]
async fn the_first_operation_creates_the_conversation() {
    let fake = Fake::start().await;
    let session = fake.session();
    assert_eq!(session.conversation_id(), None);
    assert!(session.get_items(None).await.unwrap().is_empty());
    assert_eq!(
        session.conversation_id(),
        Some(ProviderConversationId::new("conv_1"))
    );
    assert_eq!(fake.calls(Route::Create).len(), 1);
    // The local session identity is unaffected by the remote one.
    assert_eq!(session.session_id().as_str(), "sess-test");
}

#[tokio::test]
async fn repeated_operations_create_one_conversation() {
    let fake = Fake::start().await;
    let session = fake.session();
    for _ in 0..3 {
        session.get_items(None).await.unwrap();
    }
    assert_eq!(fake.calls(Route::Create).len(), 1);
}

// ---------------------------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_zero_limit_returns_nothing_without_listing() {
    let fake = Fake::start().await;
    let session = fake.session();
    assert!(session.get_items(Some(0)).await.unwrap().is_empty());
    assert_eq!(fake.calls(Route::Create).len(), 1);
    assert!(fake.calls(Route::List).is_empty());
}

#[tokio::test]
async fn a_bounded_read_pages_newest_first_and_stops_at_the_bound() {
    let fake = Fake::start().await;
    fake.page_size(2);
    fake.seed(
        "conv_x",
        (1..=7).map(|n| wire_user(&format!("m{n}"))).collect(),
    );
    let session = fake.session().with_conversation_id("conv_x");
    let items = session.get_items(Some(5)).await.unwrap();
    assert_eq!(texts(&items), ["m3", "m4", "m5", "m6", "m7"]);
    let lists = fake.calls(Route::List);
    assert_eq!(
        lists.len(),
        3,
        "five items at two per page take three pages"
    );
}

#[tokio::test]
async fn an_unbounded_read_pages_oldest_first_through_every_page() {
    let fake = Fake::start().await;
    fake.page_size(2);
    fake.seed(
        "conv_x",
        (1..=5).map(|n| wire_user(&format!("m{n}"))).collect(),
    );
    let session = fake.session().with_conversation_id("conv_x");
    let items = session.get_items(None).await.unwrap();
    assert_eq!(texts(&items), ["m1", "m2", "m3", "m4", "m5"]);
    assert_eq!(fake.calls(Route::List).len(), 3);
}

#[tokio::test]
async fn the_session_settings_bound_a_read_without_an_explicit_limit() {
    let fake = Fake::start().await;
    fake.seed(
        "conv_x",
        (1..=4).map(|n| wire_user(&format!("m{n}"))).collect(),
    );
    let session = fake
        .session()
        .with_conversation_id("conv_x")
        .with_session_settings(SessionSettings::new().with_limit(2));
    assert_eq!(texts(&session.get_items(None).await.unwrap()), ["m3", "m4"]);
    assert_eq!(
        texts(&session.get_items(Some(3)).await.unwrap()),
        ["m2", "m3", "m4"]
    );
}

#[tokio::test]
async fn session_settings_default_to_unbounded() {
    let fake = Fake::start().await;
    assert_eq!(fake.session().session_settings().unwrap().limit(), None);
    let bounded = fake
        .session()
        .with_session_settings(SessionSettings::new().with_limit(0));
    assert_eq!(bounded.session_settings().unwrap().limit(), Some(0));
}

#[tokio::test]
async fn items_read_back_carry_the_conversations_ids_and_raw_items() {
    let fake = Fake::start().await;
    fake.seed("conv_x", vec![wire_user("hello")]);
    let session = fake.session().with_conversation_id("conv_x");
    let items = session.get_items(None).await.unwrap();
    assert_eq!(items[0].id().as_str(), "item_1");
    let raw = items[0].raw_provider_item().unwrap();
    assert_eq!(raw.provider(), "openai");
    assert_eq!(raw.payload()["status"], "completed");
}

#[tokio::test]
async fn instruction_messages_read_back_as_system_messages() {
    let fake = Fake::start().await;
    fake.seed(
        "conv_x",
        vec![
            json!({"type": "message", "role": "developer", "content": [{"type": "input_text", "text": "be brief"}]}),
            json!({"type": "message", "role": "system", "content": "be kind"}),
        ],
    );
    let session = fake.session().with_conversation_id("conv_x");
    let items = session.get_items(None).await.unwrap();
    for (item, text) in items.iter().zip(["be brief", "be kind"]) {
        let RunItemKind::Message(message) = item.kind() else {
            panic!("expected a message");
        };
        assert_eq!(message.role(), MessageRole::System);
        assert_eq!(message.text_content(), text);
    }
}

#[tokio::test]
async fn a_hosted_tool_item_in_the_history_is_an_error() {
    let fake = Fake::start().await;
    fake.seed(
        "conv_x",
        vec![json!({"type": "web_search_call", "status": "completed"})],
    );
    let session = fake.session().with_conversation_id("conv_x");
    let error = session.get_items(None).await.unwrap_err();
    assert!(error.to_string().contains("web_search_call"), "{error}");
}

// ---------------------------------------------------------------------------------------------
// Appending
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn add_items_posts_the_lowered_items_without_ids() {
    let fake = Fake::start().await;
    fake.seed("test_id", Vec::new());
    let session = fake.session().with_conversation_id("test_id");
    session
        .add_items(vec![user("u1", "Hello"), assistant("a1", "Hi there!")])
        .await
        .unwrap();
    assert_eq!(
        fake.created_items(),
        [(
            "test_id".to_owned(),
            vec![
                wire_user("Hello"),
                json!({
                    "type": "message",
                    "role": "assistant",
                    "phase": "final_answer",
                    "content": [{"type": "output_text", "text": "Hi there!"}]
                })
            ]
        )]
    );
}

#[tokio::test]
async fn add_items_creates_the_conversation_first() {
    let fake = Fake::start().await;
    let session = fake.session();
    session.add_items(vec![user("u1", "Hello")]).await.unwrap();
    assert_eq!(fake.calls(Route::Create).len(), 1);
    assert_eq!(fake.created_items()[0].0, "conv_1");
}

#[tokio::test]
async fn an_empty_append_neither_creates_nor_posts() {
    let fake = Fake::start().await;
    let session = fake.session();
    session.add_items(Vec::new()).await.unwrap();
    assert!(fake.calls(Route::Create).is_empty());
    assert!(fake.calls(Route::CreateItems).is_empty());
    assert_eq!(session.conversation_id(), None);

    fake.seed("test_id", Vec::new());
    let existing = fake.session().with_conversation_id("test_id");
    existing.add_items(Vec::new()).await.unwrap();
    assert!(fake.calls(Route::CreateItems).is_empty());
    assert_eq!(
        existing.conversation_id(),
        Some(ProviderConversationId::new("test_id"))
    );
}

/// Ported from the `save_result_to_session` cases for `OpenAIConversationsSession`.
#[tokio::test]
async fn reasoning_keeps_its_id_and_encrypted_content_and_loses_provider_data() {
    let fake = Fake::start().await;
    fake.seed("conv_x", Vec::new());
    let session = fake.session().with_conversation_id("conv_x");
    session
        .add_items(vec![
            reasoning(
                "r1",
                Reasoning::new()
                    .with_id("rs_openai_conversation")
                    .with_summary(vec!["thinking".to_owned()])
                    .with_provider_data(json!({"model": "litellm/test"})),
            ),
            reasoning("r2", Reasoning::new().with_encrypted_content("encrypted")),
        ])
        .await
        .unwrap();
    let posted = &fake.created_items()[0].1;
    assert_eq!(
        posted[0],
        json!({
            "type": "reasoning",
            "id": "rs_openai_conversation",
            "summary": [{"type": "summary_text", "text": "thinking"}]
        })
    );
    assert_eq!(posted[1]["encrypted_content"], "encrypted");
    assert!(posted[1].get("id").is_none());
}

#[tokio::test]
async fn reasoning_the_conversation_cannot_store_is_dropped() {
    let fake = Fake::start().await;
    fake.seed("conv_x", Vec::new());
    let session = fake.session().with_conversation_id("conv_x");
    // Neither an id nor encrypted content; and a placeholder id, which is no server identity.
    session
        .add_items(vec![
            reasoning("r1", Reasoning::new()),
            reasoning(
                "r2",
                Reasoning::new()
                    .with_id("__fake_id__")
                    .with_summary(vec!["thinking".to_owned()]),
            ),
        ])
        .await
        .unwrap();
    assert!(
        fake.calls(Route::CreateItems).is_empty(),
        "nothing storable is left"
    );

    session
        .add_items(vec![
            reasoning(
                "r3",
                Reasoning::new()
                    .with_id("__fake_id__")
                    .with_encrypted_content("encrypted"),
            ),
            user("u1", "kept"),
        ])
        .await
        .unwrap();
    let posted = &fake.created_items()[0].1;
    assert_eq!(posted.len(), 2);
    assert_eq!(posted[0]["encrypted_content"], "encrypted");
    assert!(posted[0].get("id").is_none(), "a placeholder id is removed");
}

/// The create-item schema requires the id of an MCP approval request, as the reference's
/// `program` items in `test_runner_persists_program_item_ids`.
#[tokio::test]
async fn an_item_type_that_requires_its_id_keeps_it() {
    let fake = Fake::start().await;
    fake.seed("conv_x", Vec::new());
    let session = fake.session().with_conversation_id("conv_x");
    session
        .add_items(vec![RunItem::new(
            ItemId::new("local"),
            RunItemKind::McpApprovalRequest(McpApprovalRequest::new(
                "mcpr_1",
                "docs",
                "search",
                json!({"q": "rust"}),
            )),
        )])
        .await
        .unwrap();
    assert_eq!(fake.created_items()[0].1[0]["id"], "mcpr_1");
}

#[tokio::test]
async fn an_approval_record_is_refused() {
    let fake = Fake::start().await;
    fake.seed("conv_x", Vec::new());
    let session = fake.session().with_conversation_id("conv_x");
    let error = session
        .add_items(vec![RunItem::new(
            ItemId::new("approval"),
            RunItemKind::ToolApproval(ToolApproval::new(CallId::new("c1"), "charge", json!({}))),
        )])
        .await
        .unwrap_err();
    assert!(error.to_string().contains("tool_approval"), "{error}");
    assert!(fake.calls(Route::CreateItems).is_empty());
}

#[tokio::test]
async fn appended_items_read_back_as_the_same_model_input() {
    let fake = Fake::start().await;
    fake.seed("conv_x", Vec::new());
    let session = fake.session().with_conversation_id("conv_x");
    let structured = ToolOutput::new(vec![
        ToolOutputBlock::text("chart"),
        ToolOutputBlock::Image(ra_core::item::ImageBlock::new(
            ra_core::item::ImageSource::url("https://example.test/chart.png"),
        )),
    ])
    .unwrap();
    let written = vec![
        user("u1", "look this up"),
        reasoning(
            "r1",
            Reasoning::new()
                .with_id("rs_1")
                .with_summary(vec!["plan".to_owned()])
                .with_encrypted_content("enc"),
        ),
        RunItem::new(
            ItemId::new("c1"),
            RunItemKind::ToolCall(ToolCall::new(
                CallId::new("call_1"),
                "lookup",
                json!({"id": 7}),
            )),
        ),
        RunItem::new(
            ItemId::new("o1"),
            RunItemKind::ToolCallOutput(ToolCallOutput::new(CallId::new("call_1"), json!("ok"))),
        ),
        RunItem::new(
            ItemId::new("c2"),
            RunItemKind::ToolCall(ToolCall::custom(
                CallId::new("call_2"),
                "patch",
                "raw input",
            )),
        ),
        RunItem::new(
            ItemId::new("o2"),
            RunItemKind::ToolCallOutput(
                ToolCallOutput::new(
                    CallId::new("call_2"),
                    serde_json::to_value(&structured).unwrap(),
                )
                .with_kind(ra_core::item::ToolCallKind::Custom),
            ),
        ),
        assistant("a1", "done"),
    ];
    session.add_items(written.clone()).await.unwrap();
    let read = session.get_items(None).await.unwrap();
    let as_input = |items: &[RunItem]| -> Vec<ModelInputItem> {
        items.iter().filter_map(RunItem::to_model_input).collect()
    };
    assert_eq!(as_input(&read), as_input(&written));
    assert!(
        read.iter()
            .all(|item| item.id().as_str().starts_with("item_") || item.id().as_str() == "rs_1")
    );
}

#[tokio::test]
async fn a_failed_append_surfaces_the_providers_error() {
    let fake = Fake::start().await;
    fake.seed("test_id", Vec::new());
    fake.fail(Route::CreateItems, 500, false);
    let session = fake.session().with_conversation_id("test_id");
    let error = session
        .add_items(vec![user("u1", "Hello")])
        .await
        .unwrap_err();
    assert!(error.to_string().contains("injected failure"), "{error}");
    assert!(error.is_retryable());
}

#[tokio::test]
async fn sessions_with_different_conversations_are_isolated() {
    let fake = Fake::start().await;
    fake.seed("conversation_1", Vec::new());
    fake.seed("conversation_2", Vec::new());
    let first = fake.session().with_conversation_id("conversation_1");
    let second = fake.session().with_conversation_id("conversation_2");
    first
        .add_items(vec![user("u1", "Session 1 message")])
        .await
        .unwrap();
    second
        .add_items(vec![user("u1", "Session 2 message")])
        .await
        .unwrap();
    assert_eq!(
        stored_texts(&fake.items("conversation_1")),
        ["Session 1 message"]
    );
    assert_eq!(
        stored_texts(&fake.items("conversation_2")),
        ["Session 2 message"]
    );
}

// ---------------------------------------------------------------------------------------------
// Popping
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn pop_removes_and_returns_the_newest_item() {
    let fake = Fake::start().await;
    fake.seed(
        "test_id",
        vec![wire_user("first"), wire_assistant("Latest message")],
    );
    let session = fake.session().with_conversation_id("test_id");
    let popped = session.pop_item().await.unwrap().unwrap();
    assert_eq!(text_of(&popped), "Latest message");
    assert_eq!(popped.id().as_str(), "item_2");
    assert_eq!(
        fake.calls(Route::DeleteItem),
        ["DELETE /v1/conversations/test_id/items/item_2"]
    );
    assert_eq!(stored_texts(&fake.items("test_id")), ["first"]);
}

#[tokio::test]
async fn pop_on_an_empty_conversation_deletes_nothing() {
    let fake = Fake::start().await;
    fake.seed("test_id", Vec::new());
    let session = fake.session().with_conversation_id("test_id");
    assert!(session.pop_item().await.unwrap().is_none());
    assert!(fake.calls(Route::DeleteItem).is_empty());
}

#[tokio::test]
async fn pop_of_an_unreadable_item_deletes_nothing() {
    let fake = Fake::start().await;
    fake.seed(
        "test_id",
        vec![json!({"type": "web_search_call", "status": "completed"})],
    );
    let session = fake.session().with_conversation_id("test_id");
    assert!(session.pop_item().await.is_err());
    assert!(fake.calls(Route::DeleteItem).is_empty());
    assert_eq!(fake.items("test_id").len(), 1);
}

// ---------------------------------------------------------------------------------------------
// Clearing
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn clear_deletes_the_conversation_and_forgets_its_id() {
    let fake = Fake::start().await;
    fake.seed("test_id", vec![wire_user("hello")]);
    let session = fake.session().with_conversation_id("test_id");
    session.clear().await.unwrap();
    assert_eq!(
        fake.calls(Route::Delete),
        ["DELETE /v1/conversations/test_id"]
    );
    assert!(!fake.has_conversation("test_id"));
    assert_eq!(session.conversation_id(), None);
}

#[tokio::test]
async fn clearing_a_session_without_a_conversation_calls_nothing() {
    let fake = Fake::start().await;
    fake.fail(Route::Create, 500, false);
    let session = fake.session();
    session.clear().await.unwrap();
    assert!(fake.calls(Route::Create).is_empty());
    assert!(fake.calls(Route::Delete).is_empty());
    assert_eq!(session.conversation_id(), None);
}

#[tokio::test]
async fn a_failed_delete_keeps_the_id_and_a_retry_targets_it() {
    let fake = Fake::start().await;
    fake.seed("test_id", Vec::new());
    fake.fail(Route::Delete, 500, false);
    let session = fake.session().with_conversation_id("test_id");
    let error = session.clear().await.unwrap_err();
    assert!(error.to_string().contains("injected failure"), "{error}");
    assert_eq!(
        session.conversation_id(),
        Some(ProviderConversationId::new("test_id"))
    );

    session.clear().await.unwrap();
    assert!(fake.calls(Route::Create).is_empty());
    assert_eq!(
        fake.calls(Route::Delete),
        [
            "DELETE /v1/conversations/test_id",
            "DELETE /v1/conversations/test_id"
        ]
    );
    assert_eq!(session.conversation_id(), None);
}

const SLOW: Duration = Duration::from_millis(300);

#[tokio::test]
async fn a_cancelled_clear_settles_its_delete_before_the_session_is_reused() {
    let fake = Fake::start().await;
    fake.seed("old_id", Vec::new());
    fake.delay(Route::Delete, SLOW);
    let session = Arc::new(fake.session().with_conversation_id("old_id"));
    let clearing = tokio::spawn({
        let session = Arc::clone(&session);
        async move { session.clear().await }
    });
    fake.received(Route::Delete).await;
    clearing.abort();
    assert!(clearing.await.unwrap_err().is_cancelled());

    session
        .add_items(vec![user("u1", "Next turn")])
        .await
        .unwrap();
    let deleted = fake.received_at(Route::Delete)[0];
    let created = fake.received_at(Route::Create)[0];
    assert!(
        created.duration_since(deleted) >= SLOW - Duration::from_millis(20),
        "the next conversation must wait for the delete to settle"
    );
    assert_eq!(
        session.conversation_id(),
        Some(ProviderConversationId::new("conv_1"))
    );
    assert_eq!(fake.calls(Route::Delete).len(), 1);
    assert_eq!(fake.created_items()[0].0, "conv_1");
}

#[tokio::test]
async fn a_cancelled_clear_keeps_a_replacement_conversation() {
    let fake = Fake::start().await;
    fake.seed("old_id", Vec::new());
    fake.seed("replacement_id", Vec::new());
    fake.delay(Route::Delete, SLOW);
    let session = Arc::new(fake.session().with_conversation_id("old_id"));
    let clearing = tokio::spawn({
        let session = Arc::clone(&session);
        async move { session.clear().await }
    });
    fake.received(Route::Delete).await;
    clearing.abort();
    session.set_conversation_id("replacement_id");
    let _ = clearing.await;

    session
        .add_items(vec![user("u1", "Next turn")])
        .await
        .unwrap();
    assert_eq!(
        session.conversation_id(),
        Some(ProviderConversationId::new("replacement_id"))
    );
    assert!(fake.calls(Route::Create).is_empty());
    assert_eq!(fake.created_items()[0].0, "replacement_id");
}

#[tokio::test]
async fn a_read_during_a_clear_starts_a_new_conversation_and_keeps_it() {
    let fake = Fake::start().await;
    fake.seed("old_id", Vec::new());
    fake.delay(Route::Delete, SLOW);
    let session = Arc::new(fake.session().with_conversation_id("old_id"));
    let clearing = tokio::spawn({
        let session = Arc::clone(&session);
        async move { session.clear().await }
    });
    fake.received(Route::Delete).await;
    let reading = tokio::spawn({
        let session = Arc::clone(&session);
        async move { session.get_items(None).await }
    });
    clearing.await.unwrap().unwrap();
    reading.await.unwrap().unwrap();
    assert_eq!(
        session.conversation_id(),
        Some(ProviderConversationId::new("conv_1"))
    );
    assert_eq!(fake.calls(Route::Create).len(), 1);
}

// ---------------------------------------------------------------------------------------------
// Creating the conversation concurrently
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn concurrent_first_writes_share_one_conversation() {
    let fake = Fake::start().await;
    fake.delay(Route::Create, SLOW);
    let session = Arc::new(fake.session());
    let first = tokio::spawn({
        let session = Arc::clone(&session);
        async move { session.add_items(vec![user("u1", "First message")]).await }
    });
    fake.received(Route::Create).await;
    let second = tokio::spawn({
        let session = Arc::clone(&session);
        async move { session.add_items(vec![user("u2", "Second message")]).await }
    });
    first.await.unwrap().unwrap();
    second.await.unwrap().unwrap();
    assert_eq!(fake.calls(Route::Create).len(), 1);
    let writes = fake.created_items();
    assert_eq!(writes.len(), 2);
    assert!(
        writes
            .iter()
            .all(|(conversation, _)| conversation == "conv_1")
    );
}

#[tokio::test]
async fn a_waiting_writer_recovers_when_the_first_creation_fails() {
    let fake = Fake::start().await;
    fake.delay(Route::Create, SLOW);
    fake.fail(Route::Create, 500, false);
    let session = Arc::new(fake.session());
    let failed = tokio::spawn({
        let session = Arc::clone(&session);
        async move { session.add_items(vec![user("u1", "Failed writer")]).await }
    });
    fake.received(Route::Create).await;
    let surviving = tokio::spawn({
        let session = Arc::clone(&session);
        async move {
            session
                .add_items(vec![user("u2", "Surviving writer")])
                .await
        }
    });
    assert_eq!(fake.calls(Route::Create).len(), 1);
    assert!(failed.await.unwrap().is_err());
    surviving.await.unwrap().unwrap();
    assert_eq!(fake.calls(Route::Create).len(), 2);
    assert_eq!(
        fake.created_items(),
        [("conv_1".to_owned(), vec![wire_user("Surviving writer")])]
    );
    assert_eq!(
        session.conversation_id(),
        Some(ProviderConversationId::new("conv_1"))
    );
}

#[tokio::test]
async fn a_failed_creation_is_retried_by_the_next_operation() {
    let fake = Fake::start().await;
    fake.fail(Route::Create, 500, false);
    let session = fake.session();
    assert!(session.get_items(None).await.is_err());
    session.get_items(None).await.unwrap();
    assert_eq!(fake.calls(Route::Create).len(), 2);
    assert_eq!(
        session.conversation_id(),
        Some(ProviderConversationId::new("conv_1"))
    );
}

// ---------------------------------------------------------------------------------------------
// With the runner
// ---------------------------------------------------------------------------------------------

struct Resolver(Arc<dyn Model>);

impl ModelResolver for Resolver {
    fn resolve_model(&self, _model_name: Option<&str>) -> ra_core::error::Result<ResolvedModel> {
        Ok(ResolvedModel::new(
            ModelSelector::new(
                ProviderKey::new("openai"),
                Some("gpt-test".to_owned()),
                ApiProtocol::OpenAiResponses,
            ),
            Arc::clone(&self.0),
            ModelSettings::new(),
            ModelSettings::new(),
        ))
    }
}

fn request(fake: &Fake, session: Arc<dyn Session>, input: Vec<ModelInputItem>) -> RunRequest {
    request_with_tools(fake, session, input, Vec::new())
}

fn request_with_tools(
    fake: &Fake,
    session: Arc<dyn Session>,
    input: Vec<ModelInputItem>,
    tools: Vec<Arc<dyn Tool>>,
) -> RunRequest {
    let agent = AgentSpec::builder()
        .id(AgentId::new("assistant"))
        .name("Assistant")
        .instructions("help")
        .tools(tools)
        .build()
        .unwrap();
    let model = OpenAiResponsesModel::new("gpt-test", fake.auth()).unwrap();
    RunRequest::new(
        AgentBinding::direct(agent),
        Arc::new(Resolver(Arc::new(model))),
        RunId::new("run-1"),
        CancelScope::root(),
        input,
    )
    .with_session(session)
}

/// A tool that answers every call with the same text and counts the calls.
struct Lookup {
    origin: ToolOrigin,
    schema: ToolSchema,
    calls: Mutex<usize>,
}

impl Lookup {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            origin: ToolOrigin::new("lookup").unwrap(),
            schema: ToolSchema::new(
                "lookup",
                json!({"type": "object", "properties": {}, "required": [], "additionalProperties": false}),
            )
            .unwrap(),
            calls: Mutex::new(0),
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

    async fn call(&self, _context: ToolContext<'_>) -> ra_core::error::Result<ToolOutput> {
        *self.calls.lock().unwrap() += 1;
        Ok(ToolOutput::text("tool_result"))
    }
}

fn response(output: Vec<Value>) -> Value {
    json!({
        "id": "resp_1",
        "object": "response",
        "status": "completed",
        "output": output,
        "usage": {"input_tokens": 1, "output_tokens": 1}
    })
}

fn input_user(text: &str) -> ModelInputItem {
    ModelInputItem::Message(Message::user(text))
}

/// Ported from `test_runner_with_conversation_history` and `test_runner_integration_basic`.
#[tokio::test]
async fn a_run_reads_the_conversation_and_appends_its_turn() {
    let fake = Fake::start().await;
    fake.seed(
        "conv_x",
        vec![
            wire_user("What city is the Golden Gate Bridge in?"),
            wire_assistant("San Francisco"),
        ],
    );
    fake.respond_with(response(vec![
        json!({
            "id": "rs_1",
            "type": "reasoning",
            "summary": [{"type": "summary_text", "text": "recall"}],
            "encrypted_content": "enc"
        }),
        json!({
            "id": "msg_9",
            "type": "message",
            "role": "assistant",
            "phase": "final_answer",
            "status": "completed",
            "content": [{"type": "output_text", "text": "California"}]
        }),
    ]));
    let session = Arc::new(fake.session().with_conversation_id("conv_x"));
    let result = Runner::run(request(
        &fake,
        session.clone(),
        vec![input_user("What state is it in?")],
    ))
    .await
    .unwrap();
    assert_eq!(result.final_text(), "California");

    let sent = &fake.response_bodies()[0]["input"];
    let sent_texts: Vec<&str> = sent
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["content"][0]["text"].as_str().unwrap())
        .collect();
    assert_eq!(
        sent_texts,
        [
            "What city is the Golden Gate Bridge in?",
            "San Francisco",
            "What state is it in?"
        ]
    );
    assert!(
        sent.as_array()
            .unwrap()
            .iter()
            .all(|item| item.get("id").is_none()),
        "history replayed to the model carries no conversation ids"
    );

    assert_eq!(
        stored_texts(&fake.items("conv_x")),
        [
            "What city is the Golden Gate Bridge in?",
            "San Francisco",
            "What state is it in?",
            "reasoning",
            "California"
        ]
    );
    let writes = fake.created_items();
    assert_eq!(writes.len(), 2, "the input, then the settled turn");
    assert_eq!(writes[0].1, [wire_user("What state is it in?")]);
    assert_eq!(
        writes[1].1[0],
        json!({
            "type": "reasoning",
            "id": "rs_1",
            "summary": [{"type": "summary_text", "text": "recall"}],
            "encrypted_content": "enc"
        })
    );
    assert!(writes[1].1[1].get("id").is_none());
}

/// Ported from `test_prepare_input_with_openai_conversation_callback_matches_assistant_no_ids`
/// and `..._keeps_user_ids_distinct`: a history item a callback rebuilds without its server
/// identity is history only when it is an assistant message.
#[tokio::test]
async fn a_rebuilt_history_item_is_history_only_for_an_assistant_message() {
    for (history, expected_append) in [
        (wire_assistant("history"), vec!["new"]),
        (wire_user("history"), vec!["history", "new"]),
    ] {
        let fake = Fake::start().await;
        fake.seed("conv_x", vec![history]);
        let session = fake.session().with_conversation_id("conv_x");
        let callback = |history: &mut Vec<RunItem>, new_input: &mut Vec<RunItem>| {
            Ok(vec![
                RunItem::new(ItemId::new("rebuilt"), history[0].kind().clone()),
                new_input[0].clone(),
            ])
        };
        let plan = prepare_input_with_session(
            &RunId::new("run-prepare"),
            &[input_user("new")],
            &session,
            Some(&callback),
            None,
        )
        .await
        .unwrap();
        let prepared: Vec<String> = plan
            .prepared_for_model()
            .iter()
            .map(|item| match item {
                ModelInputItem::Message(message) => message.text_content(),
                other => other.label().to_owned(),
            })
            .collect();
        assert_eq!(prepared, ["history", "new"]);
        assert_eq!(texts(plan.append_for_turn()), expected_append);
    }
}

/// An append the server committed but whose reply was lost is acknowledged on resume rather
/// than repeated: the comparison ignores the identities the conversation assigned.
#[tokio::test]
async fn a_lost_reply_to_a_committed_append_is_acknowledged_on_resume() {
    for committed in [true, false] {
        let fake = Fake::start().await;
        fake.seed("conv_x", Vec::new());
        fake.fail(Route::CreateItems, 500, committed);
        fake.respond_with(response(vec![json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "phase": "final_answer",
            "content": [{"type": "output_text", "text": "done"}]
        })]));
        let session: Arc<dyn Session> = Arc::new(fake.session().with_conversation_id("conv_x"));
        let error = Runner::run(request(&fake, session.clone(), vec![input_user("hello")]))
            .await
            .unwrap_err();
        let checkpoint = error
            .run_state()
            .expect("the checkpoint leaves with the error");
        assert!(checkpoint.pending_session_write().is_some());
        assert!(
            fake.response_bodies().is_empty(),
            "no model work before the append settles"
        );

        let checkpoint: ra_core::state::RunState =
            serde_json::from_value(serde_json::to_value(checkpoint).unwrap()).unwrap();
        let result = Runner::run(request(&fake, session, Vec::new()).with_state(checkpoint))
            .await
            .unwrap();
        assert_eq!(result.final_text(), "done");
        assert_eq!(stored_texts(&fake.items("conv_x")), ["hello", "done"]);
        // The failed input append, a retry only when it had not been committed, then the answer.
        assert_eq!(
            fake.calls(Route::CreateItems).len(),
            if committed { 2 } else { 3 }
        );
    }
}

/// The same for a turn carrying reasoning, whose provider replay data and id the comparison
/// leaves aside, and a tool result, which reads back as the structured result it was.
#[tokio::test]
async fn a_lost_reply_to_a_committed_tool_turn_is_acknowledged_on_resume() {
    for (reasoning_item, persistable) in [
        (
            json!({"id": "rs_1", "type": "reasoning", "summary": [{"type": "summary_text", "text": "look it up"}], "encrypted_content": "enc"}),
            true,
        ),
        (
            json!({"id": "__fake_id__", "type": "reasoning", "summary": [{"type": "summary_text", "text": "look it up"}]}),
            false,
        ),
    ] {
        let fake = Fake::start().await;
        fake.seed("conv_x", Vec::new());
        // The input append goes through; the tool turn's append commits and its reply is lost.
        fake.fail_after(Route::CreateItems, 1, 500, true);
        fake.respond_with(response(vec![
            reasoning_item,
            json!({
                "id": "fc_1",
                "type": "function_call",
                "call_id": "call_1",
                "name": "lookup",
                "arguments": "{}",
                "status": "completed"
            }),
        ]));
        fake.respond_with(response(vec![wire_assistant("done")]));
        let tool = Lookup::new();
        let session: Arc<dyn Session> = Arc::new(fake.session().with_conversation_id("conv_x"));
        let error = Runner::run(request_with_tools(
            &fake,
            session.clone(),
            vec![input_user("hello")],
            vec![tool.clone()],
        ))
        .await
        .unwrap_err();
        let checkpoint: ra_core::state::RunState =
            serde_json::from_value(serde_json::to_value(error.run_state().unwrap()).unwrap())
                .unwrap();
        assert_eq!(
            checkpoint
                .pending_session_write()
                .unwrap()
                .persisted_count(),
            3
        );
        assert_eq!(
            checkpoint.pending_session_write().unwrap().items().len(),
            if persistable { 3 } else { 2 }
        );

        let result = Runner::run(
            request_with_tools(&fake, session, Vec::new(), vec![tool.clone()])
                .with_state(checkpoint),
        )
        .await
        .unwrap();
        assert_eq!(result.final_text(), "done");
        assert_eq!(*tool.calls.lock().unwrap(), 1);
        let mut expected = vec!["hello"];
        if persistable {
            expected.push("reasoning");
        }
        expected.extend(["function_call", "function_call_output", "done"]);
        assert_eq!(stored_texts(&fake.items("conv_x")), expected);
        assert_eq!(fake.calls(Route::CreateItems).len(), 3);
    }
}

/// A detached checkpoint can acknowledge a committed input even when the storage projection
/// changes the Rust representation. An uncommitted input still needs exactly one retry.
async fn assert_projected_input_resumes(input: Vec<ModelInputItem>, persisted_len: usize) {
    for committed in [true, false] {
        let fake = Fake::start().await;
        fake.seed("conv_x", vec![wire_user("earlier")]);
        fake.fail(Route::CreateItems, 500, committed);
        fake.respond_with(response(vec![wire_assistant("done")]));
        let session: Arc<dyn Session> = Arc::new(fake.session().with_conversation_id("conv_x"));
        let error = Runner::run(request(&fake, session.clone(), input.clone()))
            .await
            .unwrap_err();
        let state = error.run_state().unwrap();
        assert_eq!(
            state.pending_session_write().unwrap().items().len(),
            persisted_len
        );
        assert!(fake.response_bodies().is_empty());
        let checkpoint = serde_json::from_value(serde_json::to_value(state).unwrap()).unwrap();
        let result = Runner::run(request(&fake, session, Vec::new()).with_state(checkpoint))
            .await
            .unwrap();
        assert_eq!(result.final_text(), "done");
        assert_eq!(fake.items("conv_x").len(), persisted_len + 2);
        assert_eq!(
            fake.calls(Route::CreateItems).len(),
            if committed { 2 } else { 3 }
        );
    }
}

#[tokio::test]
async fn a_pending_input_excludes_unpersistable_reasoning() {
    for reasoning in [
        Reasoning::new().with_summary(vec!["thinking".into()]),
        Reasoning::new().with_id("__fake_id__"),
    ] {
        assert_projected_input_resumes(
            vec![ModelInputItem::Reasoning(reasoning), input_user("hello")],
            1,
        )
        .await;
    }
}

#[tokio::test]
async fn a_committed_base64_image_input_resumes_without_repeating_it() {
    let image =
        ra_core::item::ImageBlock::new(ra_core::item::ImageSource::base64("image/png", "aGVsbG8="));
    let message = Message::new(
        MessageRole::User,
        vec![ra_core::item::ContentBlock::Image(image)],
    );
    assert_projected_input_resumes(vec![ModelInputItem::Message(message)], 1).await;
}

#[tokio::test]
async fn a_committed_tool_output_with_projected_metadata_resumes() {
    let output = ToolOutput::text("result")
        .with_metadata(ra_core::tool::ObservationMetadata::new().with_guidance("look closer"));
    assert_projected_input_resumes(
        vec![
            ModelInputItem::ToolCall(ToolCall::new(CallId::new("call_1"), "lookup", json!({}))),
            ModelInputItem::ToolCallOutput(ToolCallOutput::new(
                CallId::new("call_1"),
                serde_json::to_value(output).unwrap(),
            )),
        ],
        2,
    )
    .await;
}

#[tokio::test]
async fn tool_output_files_round_trip_every_supported_source() {
    use ra_core::item::{Base64FileSource, FileBlock, FileSource};
    let fake = Fake::start().await;
    fake.seed("conv_x", Vec::new());
    let session = fake.session().with_conversation_id("conv_x");
    let output = ToolOutput::new(vec![
        ToolOutputBlock::File(FileBlock::new(FileSource::provider_file("file_123"))),
        ToolOutputBlock::File(FileBlock::new(FileSource::url(
            "https://example.test/report.pdf",
        ))),
        ToolOutputBlock::File(FileBlock::new(FileSource::Base64(
            Base64FileSource::new("aGVsbG8=").with_filename("report.pdf"),
        ))),
    ])
    .unwrap();
    let item = RunItem::new(
        ItemId::new("output"),
        RunItemKind::ToolCallOutput(ToolCallOutput::new(
            CallId::new("call_1"),
            serde_json::to_value(output).unwrap(),
        )),
    );
    session.add_items(vec![item.clone()]).await.unwrap();
    let read = session.get_items(None).await.unwrap();
    assert_eq!(read[0].to_model_input(), item.to_model_input());
    assert_eq!(
        session.pop_item().await.unwrap().unwrap().to_model_input(),
        item.to_model_input()
    );
    assert!(session.get_items(None).await.unwrap().is_empty());
    assert_projected_input_resumes(
        vec![
            ModelInputItem::ToolCall(ToolCall::new(CallId::new("call_1"), "lookup", json!({}))),
            item.to_model_input().unwrap(),
        ],
        2,
    )
    .await;
}

#[tokio::test]
async fn an_all_filtered_input_does_not_create_an_empty_pending_append() {
    let fake = Fake::start().await;
    fake.seed("conv_x", Vec::new());
    fake.respond_with(response(vec![wire_assistant("done")]));
    let session: Arc<dyn Session> = Arc::new(fake.session().with_conversation_id("conv_x"));
    let result = Runner::run(request(
        &fake,
        session,
        vec![ModelInputItem::Reasoning(Reasoning::new())],
    ))
    .await
    .unwrap();
    assert_eq!(result.final_text(), "done");
    assert_eq!(stored_texts(&fake.items("conv_x")), ["done"]);
    assert_eq!(fake.calls(Route::CreateItems).len(), 1);
    assert_eq!(
        fake.calls(Route::List).len(),
        2,
        "no tail read for a filtered append"
    );
}

#[tokio::test]
async fn a_changed_committed_projection_is_refused_before_model_work() {
    let fake = Fake::start().await;
    fake.seed("conv_x", Vec::new());
    fake.fail(Route::CreateItems, 500, true);
    let session: Arc<dyn Session> = Arc::new(fake.session().with_conversation_id("conv_x"));
    let error = Runner::run(request(&fake, session.clone(), vec![input_user("hello")]))
        .await
        .unwrap_err();
    fake.state
        .lock()
        .unwrap()
        .conversations
        .get_mut("conv_x")
        .unwrap()[0]["content"][0]["text"] = json!("changed");
    let result = Runner::run(
        request(&fake, session, Vec::new()).with_state(error.run_state().unwrap().clone()),
    )
    .await;
    assert!(result.unwrap_err().to_string().contains("cannot reconcile"));
    assert_eq!(fake.calls(Route::CreateItems).len(), 1);
    assert!(fake.response_bodies().is_empty());
}
