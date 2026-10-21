//! The turn methods and notifications, as Codex's app-server has them
//! (`app-server-protocol/src/protocol/v2/{turn,thread,thread_data}.rs`,
//! `app-server/src/request_processors/turn_processor.rs`,
//! `app-server/src/bespoke_event_handling.rs`).
//!
//! The wire cases pin each payload against the shape of Codex's generated schema. The server cases
//! run the server in this process with connections whose write queues the test holds, and threads
//! loaded through the crate's `test-api` door — `thread/start` is not ported yet — whose turns run
//! on a scripted model. They port Codex's `turn_start`, `turn_steer`, `turn_interrupt` and
//! `thread_settings_update` suites where those exercise what this port carries.
//!
//! A turn's start and its answer travel on different tasks, so their order on the wire is not
//! fixed; a case reads until the message it waits for and keeps what it passed over, as Codex's
//! test client does.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use futures::{FutureExt, future::BoxFuture};
use ra_core::{
    agent::{AgentId, AgentSpec},
    error::{Error, Result},
    item::{ItemId, Message, ModelInputItem, ModelResponse, OutputPhase, RunItem, RunItemKind},
    model::{
        ApiProtocol, Effort, JsonMap, Model, ModelOutputSchema, ModelRequest, ModelResolver,
        ModelSelector, ModelSettings, ProviderKey, ResolvedModel,
    },
    output::OutputSchema,
    session::SessionId,
};
use ra_protocol::{
    frame::{JsonRpcMessage, JsonRpcRequest, RequestId},
    server::{
        AppServer,
        protocol::{
            ClientInfo, ClientRequest, CommandExecutionRequestApprovalParams, ImageDetail,
            InitializeCapabilities, InitializeParams, ReasoningSummary, ServerRequestPayload,
            ThreadSettingsUpdateParams, Turn, TurnInterruptParams, TurnStartParams, TurnStatus,
            TurnSteerParams, UserInput,
        },
        test_api::{ThreadLoader, ThreadRequester, command_execution_approval_decision},
    },
    transport::{
        CHANNEL_CAPACITY, ConnectionId, ConnectionOrigin, QueuedOutgoingMessage, TransportEvent,
        test_api::connection_opened,
    },
};
use ra_runtime::{agent::AgentBinding, runner::RunConfig, thread::AgentThread};
use serde_json::{Value, json};
use tokio::{
    sync::{Notify, mpsc},
    task::JoinHandle,
    time::timeout,
};
use tokio_util::sync::CancellationToken;

/// How long a case waits for any one message.
const PATIENCE: Duration = Duration::from_secs(10);

const THREAD: &str = "thread-1";

/// Codex's `MAX_USER_INPUT_TEXT_CHARS`.
const MAX_USER_INPUT_TEXT_CHARS: usize = 1 << 20;

// ---------------------------------------------------------------------------------------------
// A scripted model
// ---------------------------------------------------------------------------------------------

enum Step {
    Respond(ModelResponse),
    After(BoxFuture<'static, ()>, ModelResponse),
    Hang,
}

#[derive(Debug, Clone)]
struct Call {
    model: Option<String>,
    effort: Option<Effort>,
    extra_body: JsonMap,
    input: Vec<ModelInputItem>,
    output_schema: Option<ModelOutputSchema>,
}

#[derive(Default)]
struct Script {
    steps: Mutex<VecDeque<Step>>,
    calls: Mutex<Vec<Call>>,
    called: Notify,
}

impl Script {
    fn push(&self, step: Step) {
        self.steps.lock().unwrap().push_back(step);
    }

    fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }

    async fn wait_for_calls(&self, count: usize) {
        timeout(PATIENCE, async {
            loop {
                let called = self.called.notified();
                if self.calls.lock().unwrap().len() >= count {
                    return;
                }
                called.await;
            }
        })
        .await
        .expect("the model is called");
    }
}

struct Resolver(Arc<Script>);

impl ModelResolver for Resolver {
    fn resolve_model(&self, model_name: Option<&str>) -> Result<ResolvedModel> {
        Ok(ResolvedModel::new(
            ModelSelector::new(
                ProviderKey::new("test-provider"),
                Some(model_name.unwrap_or("mock-model").to_owned()),
                ApiProtocol::OpenAiResponses,
            ),
            Arc::new(ScriptedModel {
                script: Arc::clone(&self.0),
                model: model_name.map(str::to_owned),
            }) as Arc<dyn Model>,
            ModelSettings::new(),
            ModelSettings::new(),
        ))
    }
}

struct ReasoningResolver {
    script: Arc<Script>,
    provider_defaults: ModelSettings,
    model_defaults: ModelSettings,
}

impl ModelResolver for ReasoningResolver {
    fn resolve_model(&self, model_name: Option<&str>) -> Result<ResolvedModel> {
        Ok(ResolvedModel::new(
            ModelSelector::new(
                ProviderKey::new("test-provider"),
                Some(model_name.unwrap_or("mock-model").to_owned()),
                ApiProtocol::OpenAiResponses,
            ),
            Arc::new(ScriptedModel {
                script: Arc::clone(&self.script),
                model: model_name.map(str::to_owned),
            }) as Arc<dyn Model>,
            self.provider_defaults.clone(),
            self.model_defaults.clone(),
        ))
    }
}

struct ScriptedModel {
    script: Arc<Script>,
    model: Option<String>,
}

#[async_trait]
impl Model for ScriptedModel {
    async fn get_response(&self, request: ModelRequest) -> Result<ModelResponse> {
        self.script.calls.lock().unwrap().push(Call {
            model: self.model.clone(),
            effort: request.model_settings().effort(),
            extra_body: request.model_settings().extra_body().clone(),
            input: request.input().to_vec(),
            output_schema: request.output_schema().cloned(),
        });
        self.script.called.notify_waiters();
        let step = self.script.steps.lock().unwrap().pop_front();
        match step {
            Some(Step::Respond(response)) => Ok(response),
            Some(Step::After(effect, response)) => {
                effect.await;
                Ok(response)
            }
            Some(Step::Hang) => std::future::pending().await,
            None => Err(Error::caller("the model ran out of responses")),
        }
    }
}

fn final_message(id: &str, text: &str) -> ModelResponse {
    ModelResponse::new(vec![RunItem::new(
        ItemId::new(id),
        RunItemKind::Message(Message::assistant(text, OutputPhase::Final)),
    )])
}

fn gate() -> (Arc<Notify>, BoxFuture<'static, ()>) {
    let gate = Arc::new(Notify::new());
    let waiter = Arc::clone(&gate);
    (gate, async move { waiter.notified().await }.boxed())
}

fn texts(input: &[ModelInputItem]) -> Vec<String> {
    input
        .iter()
        .filter_map(|item| match item {
            ModelInputItem::Message(message) => Some(message.text_content()),
            _ => None,
        })
        .collect()
}

fn agent_thread(script: &Arc<Script>) -> AgentThread {
    let agent = AgentSpec::builder()
        .id(AgentId::new("lead"))
        .name("Lead")
        .instructions("lead")
        .build()
        .unwrap();
    AgentThread::builder(
        SessionId::new(THREAD),
        AgentBinding::direct(agent),
        Arc::new(Resolver(Arc::clone(script))) as Arc<dyn ModelResolver>,
    )
    .with_config(RunConfig::new())
    .with_step_model_switching(true)
    .build()
}

// ---------------------------------------------------------------------------------------------
// The server in this process
// ---------------------------------------------------------------------------------------------

struct Server {
    events: mpsc::Sender<TransportEvent>,
    loader: ThreadLoader,
    _task: JoinHandle<()>,
}

impl Server {
    fn start() -> Self {
        Self::start_with(|_| ()).0
    }

    fn start_with<R>(before_run: impl FnOnce(&AppServer) -> R) -> (Self, R) {
        let server = AppServer::new(std::env::temp_dir().join("rusty-agent-turn-test")).unwrap();
        let loader = ThreadLoader::new(&server);
        let made = before_run(&server);
        let (events, transport_event_rx) = mpsc::channel(CHANNEL_CAPACITY);
        let task = tokio::spawn(server.run(transport_event_rx));
        (
            Self {
                events,
                loader,
                _task: task,
            },
            made,
        )
    }

    /// Opens and initializes a connection, opting into the experimental API if `experimental`.
    async fn connect(&self, id: u64, experimental: bool) -> Connection {
        let id = ConnectionId::new(id);
        let (writer, written) = mpsc::channel(CHANNEL_CAPACITY);
        self.events
            .send(connection_opened(
                id,
                ConnectionOrigin::WebSocket,
                writer,
                Some(CancellationToken::new()),
            ))
            .await
            .unwrap();
        let mut connection = Connection {
            id,
            written,
            passed: VecDeque::new(),
            next_id: 1,
        };
        let params = InitializeParams::new(ClientInfo::new("client", "0.1.0"))
            .with_capabilities(InitializeCapabilities::new().with_experimental_api(experimental));
        // Asked twice: the second answer, `Already initialized`, arrives once the router knows.
        for _ in 0..2 {
            self.send(
                &connection,
                JsonRpcRequest::new(0, "initialize")
                    .with_params(serde_json::to_value(&params).unwrap()),
            )
            .await;
            connection.next().await;
        }
        connection
    }

    /// Loads a thread on `script` with `connections` subscribed.
    async fn load(&self, script: &Arc<Script>, connections: &[&Connection]) -> AgentThread {
        self.load_thread(agent_thread(script), connections).await
    }

    /// Loads `thread` with `connections` subscribed.
    async fn load_thread(&self, thread: AgentThread, connections: &[&Connection]) -> AgentThread {
        let ids: Vec<_> = connections.iter().map(|connection| connection.id).collect();
        self.loader.load(thread.clone(), &ids).await;
        thread
    }

    async fn send(&self, connection: &Connection, message: impl Into<JsonRpcMessage>) {
        self.events
            .send(TransportEvent::IncomingMessage {
                connection_id: connection.id,
                message: message.into(),
            })
            .await
            .unwrap();
    }

    /// Sends `method` with `params` from `connection`, and returns the request's id.
    async fn request(&self, connection: &mut Connection, method: &str, params: Value) -> i64 {
        let id = connection.next_id;
        connection.next_id += 1;
        self.send(
            connection,
            JsonRpcRequest::new(id, method).with_params(params),
        )
        .await;
        id
    }

    async fn turn_start(&self, connection: &mut Connection, params: &TurnStartParams) -> i64 {
        self.request(
            connection,
            "turn/start",
            serde_json::to_value(params).unwrap(),
        )
        .await
    }
}

/// A connection, what the server writes to it, and what a case read past while waiting.
struct Connection {
    id: ConnectionId,
    written: mpsc::Receiver<QueuedOutgoingMessage>,
    passed: VecDeque<Value>,
    next_id: i64,
}

impl Connection {
    /// The next message, passing over the item notifications a turn sends as it runs: the cases
    /// here are about turns, and `items.rs` checks the items.
    async fn next(&mut self) -> Value {
        loop {
            let message = match self.passed.pop_front() {
                Some(message) => message,
                None => {
                    let queued = timeout(PATIENCE, self.written.recv())
                        .await
                        .expect("the server writes a message")
                        .expect("the server keeps the write queue");
                    serde_json::to_value(queued.message()).unwrap()
                }
            };
            if !is_item_notification(&message) {
                return message;
            }
        }
    }

    /// The first message `matches` accepts; the others are kept, in order, for later reads.
    async fn until(&mut self, matches: impl Fn(&Value) -> bool) -> Value {
        if let Some(index) = self.passed.iter().position(&matches) {
            return self.passed.remove(index).unwrap();
        }
        loop {
            let queued = timeout(PATIENCE, self.written.recv())
                .await
                .expect("the server writes the message waited for")
                .expect("the server keeps the write queue");
            let message = serde_json::to_value(queued.message()).unwrap();
            if matches(&message) {
                return message;
            }
            self.passed.push_back(message);
        }
    }

    async fn response(&mut self, id: i64) -> Value {
        let message = self
            .until(|message| message["id"] == id && message.get("method").is_none())
            .await;
        assert!(message.get("result").is_some(), "{message}");
        message["result"].clone()
    }

    async fn error(&mut self, id: i64) -> Value {
        let message = self
            .until(|message| message["id"] == id && message.get("method").is_none())
            .await;
        assert!(message.get("error").is_some(), "{message}");
        message["error"].clone()
    }

    async fn notification(&mut self, method: &str) -> Value {
        self.until(|message| message["method"] == method && message.get("id").is_none())
            .await["params"]
            .clone()
    }
}

fn is_item_notification(message: &Value) -> bool {
    message.get("id").is_none()
        && matches!(
            message["method"].as_str(),
            Some("item/started" | "item/completed")
        )
}

fn text_turn(text: &str) -> TurnStartParams {
    TurnStartParams::new(THREAD, vec![UserInput::text(text)])
}

fn invalid_request(message: &str) -> Value {
    json!({"code": -32600, "message": message})
}

// ---------------------------------------------------------------------------------------------
// Wire shapes
// ---------------------------------------------------------------------------------------------

#[test]
fn turn_start_params_read_codex_fields() {
    let request = JsonRpcRequest::new(3, "turn/start").with_params(json!({
        "threadId": THREAD,
        "clientUserMessageId": "client-message-1",
        "input": [
            {"type": "text", "text": "hello", "text_elements": [
                {"byteRange": {"start": 0, "end": 5}, "placeholder": null}
            ]},
            {"type": "image", "url": "data:image/png;base64,AAAA", "detail": "low"},
            {"type": "image", "fileId": "file-1"},
            {"type": "localImage", "path": "/tmp/shot.png"},
            {"type": "audio", "url": "data:audio/wav;base64,AAAA"},
            {"type": "localAudio", "path": "/tmp/clip.wav"},
            {"type": "skill", "name": "deploy", "path": "/skills/deploy"},
            {"type": "mention", "name": "app", "path": "app://x"}
        ],
        "model": "mock-model",
        "effort": "high",
        "personality": "friendly",
        "unknownField": true
    }));
    let ClientRequest::TurnStart { request_id, params } = ClientRequest::try_from(request).unwrap()
    else {
        panic!("expected turn/start");
    };
    assert_eq!(request_id, RequestId::Integer(3));
    assert_eq!(params.thread_id(), THREAD);
    assert_eq!(params.client_user_message_id(), Some("client-message-1"));
    assert_eq!(params.model(), Some("mock-model"));
    assert_eq!(params.effort(), Some("high"));
    assert_eq!(params.input().len(), 8);
    assert_eq!(params.input()[0].text_char_count(), 5);
    assert_eq!(params.unsupported_field(), None);
    assert_eq!(params.experimental_reason(), None);
    assert_eq!(
        serde_json::to_value(&params.input()[1]).unwrap(),
        json!({"type": "image", "url": "data:image/png;base64,AAAA", "detail": "low"})
    );
    assert_eq!(
        serde_json::to_value(&params.input()[2]).unwrap(),
        json!({"type": "image", "fileId": "file-1", "detail": null})
    );
    assert_eq!(
        serde_json::to_value(&params.input()[3]).unwrap(),
        json!({"type": "localImage", "detail": null, "path": "/tmp/shot.png"})
    );
}

#[test]
fn turn_steer_and_interrupt_params_read_codex_fields() {
    let steer = ClientRequest::try_from(JsonRpcRequest::new(1, "turn/steer").with_params(json!({
        "threadId": THREAD,
        "input": [{"type": "text", "text": "more"}],
        "expectedTurnId": "turn-1"
    })))
    .unwrap();
    let ClientRequest::TurnSteer { params, .. } = steer else {
        panic!("expected turn/steer");
    };
    assert_eq!(params.expected_turn_id(), "turn-1");
    assert_eq!(params.experimental_reason(), None);

    // `expectedTurnId` is required, as in Codex's schema.
    assert!(
        ClientRequest::try_from(JsonRpcRequest::new(1, "turn/steer").with_params(json!({
            "threadId": THREAD,
            "input": []
        })))
        .is_err()
    );

    let interrupt = ClientRequest::try_from(
        JsonRpcRequest::new(2, "turn/interrupt")
            .with_params(json!({"threadId": THREAD, "turnId": "turn-1"})),
    )
    .unwrap();
    let ClientRequest::TurnInterrupt { params, .. } = interrupt else {
        panic!("expected turn/interrupt");
    };
    assert_eq!((params.thread_id(), params.turn_id()), (THREAD, "turn-1"));
}

#[test]
fn turn_payloads_match_codex_schema() {
    // What Codex answers a `turn/start` with.
    assert_eq!(
        json!({"turn": serde_json::to_value(Turn::new("turn-1", TurnStatus::InProgress)).unwrap()}),
        json!({"turn": {
            "id": "turn-1",
            "items": [],
            "itemsView": "notLoaded",
            "status": "inProgress",
            "error": null,
            "startedAt": null,
            "completedAt": null,
            "durationMs": null
        }})
    );
    for (status, wire) in [
        (TurnStatus::Completed, "completed"),
        (TurnStatus::Interrupted, "interrupted"),
        (TurnStatus::Failed, "failed"),
        (TurnStatus::InProgress, "inProgress"),
    ] {
        assert_eq!(serde_json::to_value(status).unwrap(), wire);
    }
    assert_eq!(
        serde_json::to_value(ImageDetail::Original).unwrap(),
        "original"
    );
    // A turn written by a newer server reads back, its items as sent.
    let turn: Turn = serde_json::from_value(json!({
        "id": "turn-2",
        "items": [{"type": "agentMessage", "id": "m", "text": "hi"}],
        "itemsView": "summary",
        "status": "failed",
        "error": {"message": "boom", "codexErrorInfo": "other", "additionalDetails": null},
        "startedAt": 1,
        "completedAt": 2,
        "durationMs": 1000
    }))
    .unwrap();
    assert_eq!(turn.items().len(), 1);
    assert_eq!(turn.error().unwrap().message(), "boom");
    assert_eq!(
        turn.error().unwrap().codex_error_info(),
        Some(&json!("other"))
    );
}

#[test]
fn experimental_fields_name_codex_reasons() {
    let reason = |method: &str, params: Value| {
        ClientRequest::try_from(JsonRpcRequest::new(1, method).with_params(params))
            .unwrap()
            .experimental_reason()
    };
    let start = |extra: Value| {
        let mut params = json!({"threadId": THREAD, "input": []});
        params
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        reason("turn/start", params)
    };
    assert_eq!(
        start(json!({"permissions": "workspace"})),
        Some("turn/start.permissions")
    );
    assert_eq!(
        start(json!({"approvalPolicy": {"granular": {
            "sandbox_approval": true, "rules": true, "mcp_elicitations": true
        }}})),
        Some("askForApproval.granular")
    );
    assert_eq!(start(json!({"approvalPolicy": "never"})), None);
    assert_eq!(
        start(json!({"additionalContext": {}, "permissions": "x"})),
        Some("turn/start.additionalContext")
    );
    assert_eq!(
        start(json!({"multiAgentMode": "auto"})),
        Some("turn/start.multiAgentMode")
    );
    // `null` is not sent.
    assert_eq!(start(json!({"permissions": null})), None);
    assert_eq!(
        reason(
            "turn/steer",
            json!({"threadId": THREAD, "input": [], "expectedTurnId": "t", "additionalContext": {}})
        ),
        Some("turn/steer.additionalContext")
    );
    assert_eq!(
        reason("thread/settings/update", json!({"threadId": THREAD})),
        Some("thread/settings/update")
    );
    assert_eq!(
        reason("turn/interrupt", json!({"threadId": THREAD, "turnId": "t"})),
        None
    );
}

// ---------------------------------------------------------------------------------------------
// turn/start
// ---------------------------------------------------------------------------------------------

/// Codex's `turn_start_emits_notifications_and_accepts_model_override`.
#[tokio::test]
async fn turn_start_emits_notifications_and_accepts_model_override() {
    let server = Server::start();
    let mut connection = server.connect(1, false).await;
    let script = Arc::new(Script::default());
    server.load(&script, &[&connection]).await;
    script.push(Step::Respond(final_message("m-1", "Done")));

    let id = server
        .turn_start(
            &mut connection,
            &text_turn("hello").with_model("mock-model-override"),
        )
        .await;
    let turn = connection.response(id).await["turn"].clone();
    let turn_id = turn["id"].as_str().unwrap().to_owned();
    assert!(!turn_id.is_empty());
    assert_eq!(
        turn,
        json!({
            "id": turn_id,
            "items": [],
            "itemsView": "notLoaded",
            "status": "inProgress",
            "error": null,
            "startedAt": null,
            "completedAt": null,
            "durationMs": null
        })
    );

    let started = connection.notification("turn/started").await;
    assert_eq!(started["threadId"], THREAD);
    assert_eq!(started["turn"]["id"], turn_id.as_str());
    assert_eq!(started["turn"]["status"], "inProgress");
    assert!(started["turn"]["startedAt"].is_i64());
    assert_eq!(started["turn"]["completedAt"], Value::Null);

    let completed = connection.notification("turn/completed").await;
    assert_eq!(completed["threadId"], THREAD);
    assert_eq!(completed["turn"]["id"], turn_id.as_str());
    assert_eq!(completed["turn"]["status"], "completed");
    assert_eq!(completed["turn"]["error"], Value::Null);
    assert_eq!(completed["turn"]["startedAt"], started["turn"]["startedAt"]);
    assert!(completed["turn"]["completedAt"].is_i64());
    assert!(completed["turn"]["durationMs"].is_i64());

    let calls = script.calls();
    assert_eq!(calls[0].model.as_deref(), Some("mock-model-override"));
    assert_eq!(texts(&calls[0].input), vec!["hello"]);
}

#[tokio::test]
async fn reasoning_summary_preserves_resolved_lower_layer_fields() {
    let server = Server::start();
    let mut connection = server.connect(1, false).await;
    let script = Arc::new(Script::default());
    let provider = ProviderKey::new("test-provider");
    let resolver = ReasoningResolver {
        script: Arc::clone(&script),
        provider_defaults: ModelSettings::new().with_extra_body_value(
            provider.clone(),
            "reasoning",
            json!({"context": "provider-context", "mode": "provider-mode"}),
        ),
        model_defaults: ModelSettings::new(),
    };
    let agent = AgentSpec::builder()
        .id(AgentId::new("lead"))
        .name("Lead")
        .build()
        .unwrap();
    let thread = AgentThread::builder(
        SessionId::new(THREAD),
        AgentBinding::direct(agent),
        Arc::new(resolver),
    )
    .build();
    server.load_thread(thread, &[&connection]).await;
    script.push(Step::Respond(final_message("m-1", "Done")));

    let mut turn = serde_json::to_value(text_turn("hello")).unwrap();
    turn["summary"] = serde_json::to_value(ReasoningSummary::Concise).unwrap();
    let turn = serde_json::from_value(turn).unwrap();
    let id = server.turn_start(&mut connection, &turn).await;
    connection.response(id).await;
    connection.notification("turn/completed").await;

    let calls = script.calls();
    assert_eq!(
        calls[0].extra_body["reasoning"],
        json!({
            "context": "provider-context",
            "mode": "provider-mode",
            "summary": "concise"
        })
    );
}

/// Codex's `turn_start_with_empty_input_runs_model_request`.
#[tokio::test]
async fn turn_start_with_empty_input_runs_model_request() {
    let server = Server::start();
    let mut connection = server.connect(1, false).await;
    let script = Arc::new(Script::default());
    server.load(&script, &[&connection]).await;
    script.push(Step::Respond(final_message("m-1", "Done")));

    let id = server
        .turn_start(&mut connection, &TurnStartParams::new(THREAD, Vec::new()))
        .await;
    let turn_id = connection.response(id).await["turn"]["id"].clone();
    let started = connection.notification("turn/started").await;
    assert_eq!(started["turn"]["id"], turn_id);
    let completed = connection.notification("turn/completed").await;
    assert_eq!(completed["turn"]["status"], "completed");

    let calls = script.calls();
    assert_eq!(calls.len(), 1);
    assert!(
        calls[0].input.is_empty(),
        "empty turn/start should not synthesize an empty user message: {:?}",
        calls[0].input
    );
}

/// Codex's `turn_start_steers_active_turn_and_returns_active_turn_id`.
#[tokio::test]
async fn turn_start_steers_active_turn_and_returns_active_turn_id() {
    let server = Server::start();
    let mut connection = server.connect(1, false).await;
    let script = Arc::new(Script::default());
    server.load(&script, &[&connection]).await;
    let (release, wait) = gate();
    script.push(Step::After(wait, final_message("m-1", "first")));
    script.push(Step::Respond(final_message("m-2", "Done")));

    let id = server
        .turn_start(&mut connection, &text_turn("first"))
        .await;
    let active = connection.response(id).await["turn"]["id"].clone();
    connection.notification("turn/started").await;
    script.wait_for_calls(1).await;

    let id = server
        .turn_start(&mut connection, &text_turn("second"))
        .await;
    let steered = connection.response(id).await["turn"].clone();
    assert_eq!(steered["id"], active);
    assert_eq!(steered["status"], "inProgress");
    release.notify_one();

    let completed = connection.notification("turn/completed").await;
    assert_eq!(completed["turn"]["id"], active);
    let calls = script.calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(texts(&calls[1].input), vec!["first", "first", "second"]);
}

/// Codex's `turn_start_rejects_combined_oversized_text_input`.
#[tokio::test]
async fn turn_start_rejects_combined_oversized_text_input() {
    let server = Server::start();
    let mut connection = server.connect(1, false).await;
    let script = Arc::new(Script::default());
    server.load(&script, &[&connection]).await;

    let first = "x".repeat(MAX_USER_INPUT_TEXT_CHARS / 2);
    let second = "y".repeat(MAX_USER_INPUT_TEXT_CHARS / 2 + 1);
    let actual_chars = first.chars().count() + second.chars().count();
    let params = TurnStartParams::new(
        THREAD,
        vec![UserInput::text(first), UserInput::text(second)],
    );
    let id = server.turn_start(&mut connection, &params).await;
    let error = connection.error(id).await;
    assert_eq!(
        error,
        json!({
            "code": -32602,
            "message": format!(
                "Input exceeds the maximum length of {MAX_USER_INPUT_TEXT_CHARS} characters."
            ),
            "data": {
                "input_error_code": "input_too_large",
                "max_chars": MAX_USER_INPUT_TEXT_CHARS,
                "actual_chars": actual_chars,
            }
        })
    );
    assert!(script.calls().is_empty());
    assert!(script.steps.lock().unwrap().is_empty());
}

#[tokio::test]
async fn turn_start_accepts_text_at_the_limit() {
    let server = Server::start();
    let mut connection = server.connect(1, false).await;
    let script = Arc::new(Script::default());
    server.load(&script, &[&connection]).await;
    script.push(Step::Respond(final_message("m-1", "Done")));

    let params = TurnStartParams::new(
        THREAD,
        vec![UserInput::text("x".repeat(MAX_USER_INPUT_TEXT_CHARS))],
    );
    let id = server.turn_start(&mut connection, &params).await;
    assert_eq!(
        connection.response(id).await["turn"]["status"],
        "inProgress"
    );
    connection.notification("turn/completed").await;
}

#[tokio::test]
async fn turn_start_refuses_what_a_run_here_does_not_carry() {
    let server = Server::start();
    let mut connection = server.connect(1, true).await;
    let script = Arc::new(Script::default());
    server.load(&script, &[&connection]).await;

    let cases = [
        (
            json!({"threadId": "missing", "input": []}),
            invalid_request("thread not found: missing"),
        ),
        (
            json!({"threadId": THREAD, "input": [], "cwd": "/tmp"}),
            invalid_request("turn/start.cwd is not supported"),
        ),
        (
            json!({"threadId": THREAD, "input": [], "sandboxPolicy": {"type": "readOnly"}}),
            invalid_request("the thread's host has not configured a permission settings adapter"),
        ),
        (
            json!({"threadId": THREAD, "input": [], "permissions": "workspace"}),
            invalid_request("the thread's host has not configured a permission settings adapter"),
        ),
        (
            json!({"threadId": THREAD, "input": [], "effort": "minimal"}),
            invalid_request("turn/start.effort `minimal` is not supported"),
        ),
        (
            json!({"threadId": THREAD, "input": [{"type": "audio", "url": "data:audio/wav;base64,AA"}]}),
            invalid_request("audio input is not supported"),
        ),
        (
            json!({"threadId": THREAD, "input": [{"type": "mention", "name": "a", "path": "app://a"}]}),
            invalid_request("mention input is not supported"),
        ),
        (
            json!({"threadId": THREAD, "input": [
                {"type": "image", "url": "data:image/png;base64,AA", "detail": "original"}
            ]}),
            invalid_request("image detail `original` is not supported"),
        ),
        // Codex's `validate_user_input_image_urls`, which comes first.
        (
            json!({"threadId": "missing", "input": [
                {"type": "image", "url": "https://example.com/cat.png"}
            ]}),
            invalid_request("remote image URLs are not supported; use an inline data URL instead"),
        ),
    ];
    for (params, expected) in cases {
        let id = server
            .request(&mut connection, "turn/start", params.clone())
            .await;
        assert_eq!(connection.error(id).await, expected, "{params}");
    }
    assert!(script.calls().is_empty());
}

/// Codex's `turn_start_ignores_deprecated_multi_agent_mode` and
/// `turn_start_accepts_deprecated_personality_override_v2`.
#[tokio::test]
async fn turn_start_ignores_deprecated_fields() {
    let server = Server::start();
    let mut connection = server.connect(1, true).await;
    let script = Arc::new(Script::default());
    server.load(&script, &[&connection]).await;
    script.push(Step::Respond(final_message("m-1", "Done")));

    let id = server
        .request(
            &mut connection,
            "turn/start",
            json!({
                "threadId": THREAD,
                "input": [{"type": "text", "text": "hi"}],
                "multiAgentMode": "auto",
                "personality": "friendly"
            }),
        )
        .await;
    assert_eq!(
        connection.response(id).await["turn"]["status"],
        "inProgress"
    );
    assert_eq!(
        connection.notification("turn/completed").await["turn"]["status"],
        "completed"
    );
}

#[tokio::test]
async fn experimental_fields_need_the_experimental_api() {
    let server = Server::start();
    let mut connection = server.connect(1, false).await;
    let script = Arc::new(Script::default());
    server.load(&script, &[&connection]).await;

    let id = server
        .request(
            &mut connection,
            "turn/start",
            json!({"threadId": THREAD, "input": [], "permissions": "workspace"}),
        )
        .await;
    assert_eq!(
        connection.error(id).await,
        invalid_request("turn/start.permissions requires experimentalApi capability")
    );
    let id = server
        .request(
            &mut connection,
            "thread/settings/update",
            json!({"threadId": THREAD, "model": "m"}),
        )
        .await;
    assert_eq!(
        connection.error(id).await,
        invalid_request("thread/settings/update requires experimentalApi capability")
    );
}

#[tokio::test]
async fn a_failed_turn_reports_the_error_and_completes_failed() {
    let server = Server::start();
    let mut connection = server.connect(1, false).await;
    let script = Arc::new(Script::default());
    server.load(&script, &[&connection]).await;

    let id = server.turn_start(&mut connection, &text_turn("fail")).await;
    let turn_id = connection.response(id).await["turn"]["id"].clone();
    let error = connection.notification("error").await;
    assert_eq!(error["threadId"], THREAD);
    assert_eq!(error["turnId"], turn_id);
    assert_eq!(error["willRetry"], false);
    let message = error["error"]["message"].as_str().unwrap().to_owned();
    assert!(message.contains("ran out of responses"), "{message}");
    assert_eq!(
        error["error"],
        json!({
            "message": message,
            "codexErrorInfo": null,
            "additionalDetails": null,
            "misalignment": null
        })
    );

    let completed = connection.notification("turn/completed").await;
    assert_eq!(completed["turn"]["status"], "failed");
    assert_eq!(completed["turn"]["error"], error["error"]);
}

// ---------------------------------------------------------------------------------------------
// turn/steer
// ---------------------------------------------------------------------------------------------

/// Codex's `turn_steer_requires_active_turn`.
#[tokio::test]
async fn turn_steer_requires_active_turn() {
    let server = Server::start();
    let mut connection = server.connect(1, false).await;
    let script = Arc::new(Script::default());
    server.load(&script, &[&connection]).await;

    let params = TurnSteerParams::new(
        THREAD,
        vec![UserInput::text("steer")],
        "turn-does-not-exist",
    );
    let id = server
        .request(
            &mut connection,
            "turn/steer",
            serde_json::to_value(params).unwrap(),
        )
        .await;
    assert_eq!(
        connection.error(id).await,
        invalid_request("no active turn to steer")
    );
}

/// Codex's `turn_steer_returns_active_turn_id` and `turn_steer_rejects_oversized_text_input`,
/// with the other refusals of a steer.
#[tokio::test]
async fn turn_steer_returns_active_turn_id() {
    let server = Server::start();
    let mut connection = server.connect(1, false).await;
    let script = Arc::new(Script::default());
    server.load(&script, &[&connection]).await;
    let (release, wait) = gate();
    script.push(Step::After(wait, final_message("m-1", "first")));
    script.push(Step::Respond(final_message("m-2", "Done")));

    let id = server.turn_start(&mut connection, &text_turn("run")).await;
    let turn_id = connection.response(id).await["turn"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    connection.notification("turn/started").await;
    script.wait_for_calls(1).await;

    let steer = |input: Vec<UserInput>, expected: &str| {
        serde_json::to_value(TurnSteerParams::new(THREAD, input, expected)).unwrap()
    };

    let oversized = "x".repeat(MAX_USER_INPUT_TEXT_CHARS + 1);
    let id = server
        .request(
            &mut connection,
            "turn/steer",
            steer(vec![UserInput::text(oversized.clone())], &turn_id),
        )
        .await;
    let error = connection.error(id).await;
    assert_eq!(error["code"], -32602);
    assert_eq!(error["data"]["input_error_code"], "input_too_large");
    assert_eq!(error["data"]["actual_chars"], oversized.chars().count());

    let id = server
        .request(
            &mut connection,
            "turn/steer",
            steer(vec![UserInput::text("x")], ""),
        )
        .await;
    assert_eq!(
        connection.error(id).await,
        invalid_request("expectedTurnId must not be empty")
    );
    let id = server
        .request(
            &mut connection,
            "turn/steer",
            steer(vec![UserInput::text("x")], "wrong-turn"),
        )
        .await;
    assert_eq!(
        connection.error(id).await,
        invalid_request(&format!(
            "expected active turn id `wrong-turn` but found `{turn_id}`"
        ))
    );
    let id = server
        .request(&mut connection, "turn/steer", steer(Vec::new(), &turn_id))
        .await;
    assert_eq!(
        connection.error(id).await,
        invalid_request("input must not be empty")
    );

    let id = server
        .request(
            &mut connection,
            "turn/steer",
            steer(vec![UserInput::text("steered")], &turn_id),
        )
        .await;
    assert_eq!(connection.response(id).await, json!({"turnId": turn_id}));
    release.notify_one();
    connection.notification("turn/completed").await;
    assert_eq!(
        texts(&script.calls()[1].input),
        vec!["run", "first", "steered"]
    );
}

// ---------------------------------------------------------------------------------------------
// turn/interrupt
// ---------------------------------------------------------------------------------------------

/// Codex's `turn_interrupt_aborts_running_turn`.
#[tokio::test]
async fn turn_interrupt_aborts_running_turn() {
    let server = Server::start();
    let mut connection = server.connect(1, false).await;
    let script = Arc::new(Script::default());
    server.load(&script, &[&connection]).await;
    script.push(Step::Hang);

    let id = server
        .turn_start(&mut connection, &text_turn("run sleep"))
        .await;
    let turn_id = connection.response(id).await["turn"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let started = connection.notification("turn/started").await;
    assert_eq!(started["threadId"], THREAD);
    assert_eq!(started["turn"]["id"], turn_id.as_str());

    let id = server
        .request(
            &mut connection,
            "turn/interrupt",
            serde_json::to_value(TurnInterruptParams::new(THREAD, "wrong-turn")).unwrap(),
        )
        .await;
    assert_eq!(
        connection.error(id).await,
        invalid_request(&format!(
            "expected active turn id wrong-turn but found {turn_id}"
        ))
    );

    let id = server
        .request(
            &mut connection,
            "turn/interrupt",
            serde_json::to_value(TurnInterruptParams::new(THREAD, &turn_id)).unwrap(),
        )
        .await;
    // Answered once the turn has ended, ahead of the notification that it did.
    let answer = connection
        .until(|message| message["id"] == id || message["method"] == "turn/completed")
        .await;
    assert_eq!(answer, json!({"id": id, "result": {}}));
    let completed = connection.notification("turn/completed").await;
    assert_eq!(completed["threadId"], THREAD);
    assert_eq!(completed["turn"]["id"], turn_id.as_str());
    assert_eq!(completed["turn"]["status"], "interrupted");
    assert_eq!(completed["turn"]["error"], Value::Null);
}

/// Codex's `turn_interrupt_rejects_completed_turn`.
#[tokio::test]
async fn turn_interrupt_rejects_completed_turn() {
    let server = Server::start();
    let mut connection = server.connect(1, false).await;
    let script = Arc::new(Script::default());
    server.load(&script, &[&connection]).await;
    script.push(Step::Respond(final_message("m-1", "done")));

    let id = server
        .turn_start(&mut connection, &text_turn("say done"))
        .await;
    let turn_id = connection.response(id).await["turn"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let completed = connection.notification("turn/completed").await;
    assert_eq!(completed["turn"]["status"], "completed");

    let id = server
        .request(
            &mut connection,
            "turn/interrupt",
            serde_json::to_value(TurnInterruptParams::new(THREAD, &turn_id)).unwrap(),
        )
        .await;
    assert_eq!(
        connection.error(id).await,
        invalid_request("no active turn to interrupt")
    );
}

#[tokio::test]
async fn turn_interrupt_rejects_an_idle_thread() {
    let server = Server::start();
    let mut connection = server.connect(1, false).await;
    let script = Arc::new(Script::default());
    server.load(&script, &[&connection]).await;

    let id = server
        .request(
            &mut connection,
            "turn/interrupt",
            serde_json::to_value(TurnInterruptParams::new(THREAD, "turn-never-started")).unwrap(),
        )
        .await;
    assert_eq!(
        connection.error(id).await,
        invalid_request("no active turn to interrupt")
    );
}

#[tokio::test]
async fn a_startup_interrupt_is_answered_at_once() {
    let server = Server::start();
    let mut connection = server.connect(1, false).await;
    let script = Arc::new(Script::default());
    server.load(&script, &[&connection]).await;

    let id = server
        .request(
            &mut connection,
            "turn/interrupt",
            serde_json::to_value(TurnInterruptParams::new(THREAD, "")).unwrap(),
        )
        .await;
    assert_eq!(connection.response(id).await, json!({}));
}

/// Codex's `turn_interrupt_resolves_pending_command_approval_request`: the end of the turn
/// withdraws the thread's server requests still waiting.
#[tokio::test]
async fn turn_interrupt_resolves_pending_server_requests() {
    let (server, requester) = Server::start_with(|server| {
        ThreadRequester::new(server, &[ConnectionId::new(1)], SessionId::new(THREAD))
    });
    let mut connection = server.connect(1, false).await;
    let script = Arc::new(Script::default());
    server.load(&script, &[&connection]).await;
    script.push(Step::Hang);
    let id = server.turn_start(&mut connection, &text_turn("run")).await;
    let turn_id = connection.response(id).await["turn"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    connection.notification("turn/started").await;
    script.wait_for_calls(1).await;

    // The request a running turn raises; its producer is still to be connected.
    let (request_id, answer) = requester
        .send_request(ServerRequestPayload::CommandExecutionRequestApproval(
            CommandExecutionRequestApprovalParams::new(THREAD, &turn_id, "call-1", 0),
        ))
        .await;
    let request = connection
        .until(|message| message.get("id").is_some() && message.get("method").is_some())
        .await;
    assert_eq!(request["method"], "item/commandExecution/requestApproval");
    let waiting = tokio::spawn(async move { requester.wait_for_answer(request_id, answer).await });

    let id = server
        .request(
            &mut connection,
            "turn/interrupt",
            serde_json::to_value(TurnInterruptParams::new(THREAD, &turn_id)).unwrap(),
        )
        .await;
    connection.response(id).await;
    let answer = timeout(PATIENCE, waiting).await.unwrap().unwrap();
    assert_eq!(command_execution_approval_decision(answer), None);
    let resolved = connection.notification("serverRequest/resolved").await;
    assert_eq!(resolved, json!({"threadId": THREAD, "requestId": 0}));
    assert_eq!(
        connection.notification("turn/completed").await["turn"]["status"],
        "interrupted"
    );
}

// ---------------------------------------------------------------------------------------------
// thread/settings/update
// ---------------------------------------------------------------------------------------------

/// Codex's `thread_settings_update_emits_notification_and_updates_future_turns` and
/// `thread_settings_update_while_turn_is_active_emits_notification`, without the notification.
#[tokio::test]
async fn thread_settings_update_updates_future_turns() {
    let server = Server::start();
    let mut connection = server.connect(1, true).await;
    let script = Arc::new(Script::default());
    let thread = server.load(&script, &[&connection]).await;
    let (release, wait) = gate();
    script.push(Step::After(wait, final_message("m-1", "first")));
    script.push(Step::Respond(final_message("m-2", "second")));

    let id = server.turn_start(&mut connection, &text_turn("one")).await;
    connection.response(id).await;
    script.wait_for_calls(1).await;
    let update = ThreadSettingsUpdateParams::new(THREAD)
        .with_model("updated-model")
        .with_effort("xhigh");
    let id = server
        .request(
            &mut connection,
            "thread/settings/update",
            serde_json::to_value(update).unwrap(),
        )
        .await;
    assert_eq!(connection.response(id).await, json!({}));
    assert_eq!(thread.settings().model(), Some("updated-model"));
    release.notify_one();
    connection.notification("turn/completed").await;

    let id = server.turn_start(&mut connection, &text_turn("two")).await;
    connection.response(id).await;
    connection.notification("turn/completed").await;

    let calls = script.calls();
    assert_eq!(calls[0].model, None, "the running turn keeps its settings");
    assert_eq!(calls[1].model.as_deref(), Some("updated-model"));
    assert_eq!(calls[1].effort, Some(Effort::XHigh));
}

#[tokio::test]
async fn thread_settings_update_refuses_what_a_run_here_does_not_carry() {
    let server = Server::start();
    let mut connection = server.connect(1, true).await;
    let script = Arc::new(Script::default());
    server.load(&script, &[&connection]).await;

    let cases = [
        (
            json!({"threadId": "missing"}),
            invalid_request("thread not found: missing"),
        ),
        (
            json!({"threadId": THREAD, "approvalPolicy": "never"}),
            invalid_request("the thread's host has not configured a permission settings adapter"),
        ),
        (
            json!({"threadId": THREAD, "effort": "ultra"}),
            invalid_request("thread/settings/update.effort `ultra` is not supported"),
        ),
    ];
    for (params, expected) in cases {
        let id = server
            .request(&mut connection, "thread/settings/update", params.clone())
            .await;
        assert_eq!(connection.error(id).await, expected, "{params}");
    }
    let id = server
        .request(
            &mut connection,
            "thread/settings/update",
            json!({"threadId": THREAD, "personality": "pragmatic"}),
        )
        .await;
    assert_eq!(connection.response(id).await, json!({}));
}

#[test]
fn live_settings_payload_preserves_null_tier_and_rejects_unknown_fields() {
    use ra_protocol::server::protocol::TurnSettingsUpdateParams;
    let omitted: TurnSettingsUpdateParams =
        serde_json::from_value(json!({"threadId": THREAD, "turnId": "turn"})).unwrap();
    let cleared: TurnSettingsUpdateParams = serde_json::from_value(
        json!({"threadId": THREAD, "turnId": "turn", "serviceTier": null, "effort": null}),
    )
    .unwrap();
    assert_eq!(omitted.service_tier(), None);
    assert_eq!(cleared.service_tier(), Some(None));
    assert_eq!(cleared.effort(), None);
    assert!(
        serde_json::from_value::<TurnSettingsUpdateParams>(
            json!({"threadId": THREAD, "turnId": "turn", "unexpected": true})
        )
        .is_err()
    );
    assert_eq!(
        serde_json::to_value(cleared).unwrap()["serviceTier"],
        Value::Null
    );
}

#[tokio::test]
async fn turn_settings_update_accepts_live_target_and_reports_unavailable_target() {
    let server = Server::start();
    let mut connection = server.connect(1, true).await;
    let script = Arc::new(Script::default());
    let thread = server.load(&script, &[&connection]).await;
    script.push(Step::Hang);
    let id = server.turn_start(&mut connection, &text_turn("run")).await;
    let turn_id = connection.response(id).await["turn"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    script.wait_for_calls(1).await;
    let id = server
        .request(
            &mut connection,
            "turn/settings/update",
            json!({"threadId": THREAD, "turnId": turn_id,
        "model": "next-model", "effort": "high", "summary": "concise", "serviceTier": "priority"}),
        )
        .await;
    assert_eq!(connection.response(id).await, json!({"status": "applied"}));
    assert_eq!(thread.run_config().model(), None);
    assert_eq!(
        thread.turn_config(&turn_id).unwrap().model(),
        Some("next-model")
    );
    let id = server
        .request(
            &mut connection,
            "turn/settings/update",
            json!({"threadId": THREAD, "turnId": "wrong"}),
        )
        .await;
    assert_eq!(
        connection.response(id).await,
        json!({"status": "targetUnavailable"})
    );
    assert!(thread.interrupt());
    connection.notification("turn/completed").await;
    let id = server
        .request(
            &mut connection,
            "turn/settings/update",
            json!({"threadId": THREAD, "turnId": turn_id}),
        )
        .await;
    assert_eq!(
        connection.response(id).await,
        json!({"status": "targetUnavailable"})
    );
}

#[tokio::test]
async fn thread_settings_notify_all_experimental_subscribers() {
    let server = Server::start();
    let mut first = server.connect(1, true).await;
    let mut second = server.connect(2, true).await;
    let script = Arc::new(Script::default());
    server.load(&script, &[&first, &second]).await;
    let id = server
        .request(
            &mut first,
            "thread/settings/update",
            json!({"threadId": THREAD, "model": "updated", "effort": "high"}),
        )
        .await;
    assert_eq!(first.response(id).await, json!({}));
    let update = first.notification("thread/settings/updated").await;
    assert_eq!(update["threadId"], THREAD);
    assert_eq!(update["threadSettings"]["model"], "updated");
    assert_eq!(update["threadSettings"]["effort"], "high");
    assert_eq!(second.notification("thread/settings/updated").await, update);
}

#[tokio::test]
async fn coding_permission_updates_are_atomic_and_enforced_by_commands() {
    use ra_coding::{host::CodingHost, thread_settings::CodingThreadSettingsAdapter};
    use ra_core::state::RunId;
    use ra_core::{context::RunContext, item::CallId};
    use ra_protocol::server::protocol::SandboxPolicy;
    let dir = tempfile::tempdir().unwrap();
    let host = CodingHost::open(dir.path()).unwrap();
    let adapter = Arc::new(
        CodingThreadSettingsAdapter::new(
            host.workspace_root(),
            SandboxPolicy::ReadOnly {
                network_access: false,
            },
        )
        .unwrap(),
    );
    let server = Server::start();
    let mut connection = server.connect(1, true).await;
    let script = Arc::new(Script::default());
    let agent = AgentSpec::builder()
        .id(AgentId::new("lead"))
        .name("lead")
        .instructions("lead")
        .build()
        .unwrap();
    let thread = AgentThread::builder(
        SessionId::new(THREAD),
        AgentBinding::direct(agent.clone()),
        Arc::new(Resolver(script)),
    )
    .with_config(adapter.initialize(host.build_run_config()))
    .build();
    server
        .loader
        .load_with_adapter(thread.clone(), &[connection.id], Some(adapter))
        .await;
    let id = server
        .request(
            &mut connection,
            "thread/settings/update",
            json!({"threadId": THREAD, "approvalPolicy": "never"}),
        )
        .await;
    assert_eq!(connection.response(id).await, json!({}));
    assert_eq!(
        connection.notification("thread/settings/updated").await["threadSettings"]["approvalPolicy"],
        "never"
    );
    let tool = host.exec_command_tool().unwrap();
    let call_id = CallId::new("command");
    let arguments = json!({"cmd": "printf changed > forbidden.txt", "yield_time_ms": 1000});
    let config = thread.run_config();
    let run = RunContext::new(RunId::new("run"), &agent)
        .with_app_context(config.app_context().unwrap().clone());
    let context = ra_core::tool::ToolContext::new(&run, tool.as_ref(), &call_id, &arguments);
    assert!(!tool.needs_approval(&context).await.unwrap());
    let output = tool.call(context).await.unwrap();
    assert!(
        !dir.path().join("forbidden.txt").exists(),
        "read-only policy must reach the executor: {output:?}"
    );
    let before = thread.run_config().app_context().unwrap().clone();
    let id = server
        .request(
            &mut connection,
            "thread/settings/update",
            json!({"threadId": THREAD,
        "permissions": "unknown", "approvalPolicy": "on-request"}),
        )
        .await;
    assert!(
        connection.error(id).await["message"]
            .as_str()
            .unwrap()
            .contains("unknown permissions profile")
    );
    assert!(Arc::ptr_eq(
        &before,
        thread.run_config().app_context().unwrap()
    ));
    let id = server
        .request(
            &mut connection,
            "thread/settings/update",
            json!({"threadId": THREAD,
        "permissions": ":workspace", "sandboxPolicy": {"type": "readOnly"}}),
        )
        .await;
    assert!(
        connection.error(id).await["message"]
            .as_str()
            .unwrap()
            .contains("cannot be combined")
    );
    let id = server
        .request(
            &mut connection,
            "thread/settings/update",
            json!({"threadId": THREAD, "permissions": ":workspace"}),
        )
        .await;
    assert_eq!(connection.response(id).await, json!({}));
    let update = connection.notification("thread/settings/updated").await;
    assert_eq!(
        update["threadSettings"]["activePermissionProfile"]["id"],
        ":workspace"
    );
    let config = thread.run_config();
    let run = RunContext::new(RunId::new("run-2"), &agent)
        .with_app_context(config.app_context().unwrap().clone());
    tool.call(ra_core::tool::ToolContext::new(
        &run,
        tool.as_ref(),
        &call_id,
        &arguments,
    ))
    .await
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.path().join("forbidden.txt")).unwrap(),
        "changed"
    );
}

#[derive(Default)]
struct SettingsRecorder(Mutex<Vec<ra_core::session::rollout::RolloutItem>>);

#[async_trait]
impl ra_core::session::rollout::RolloutRecorder for SettingsRecorder {
    fn record(&self, item: ra_core::session::rollout::RolloutItem) {
        self.0.lock().unwrap().push(item);
    }
    async fn flush(&self) -> Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn settings_notification_has_a_persisted_snapshot_and_deduplicates_updates() {
    use ra_core::{event::HostEventBody, session::rollout::RolloutItem};
    let server = Server::start();
    let mut connection = server.connect(1, true).await;
    let script = Arc::new(Script::default());
    let recorder = Arc::new(SettingsRecorder::default());
    let agent = AgentSpec::builder()
        .id(AgentId::new("lead"))
        .name("lead")
        .build()
        .unwrap();
    let thread = AgentThread::builder(
        SessionId::new(THREAD),
        AgentBinding::direct(agent),
        Arc::new(Resolver(script.clone())),
    )
    .with_rollout_recorder(recorder.clone())
    .build();
    server.loader.load(thread, &[connection.id]).await;
    for index in 0..2 {
        let id = server
            .request(
                &mut connection,
                "thread/settings/update",
                json!({"threadId": THREAD,
            "model": "updated", "serviceTier": "priority", "summary": "concise"}),
            )
            .await;
        assert_eq!(connection.response(id).await, json!({}));
        if index == 0 {
            let update = connection.notification("thread/settings/updated").await;
            assert_eq!(update["threadSettings"]["serviceTier"], "priority");
            assert_eq!(update["threadSettings"]["summary"], "concise");
            assert!(recorder.0.lock().unwrap().iter().any(
                |item| matches!(item, RolloutItem::Event(event)
                if matches!(event.body(), HostEventBody::Unknown { family, data }
                    if family == "threadSettingsApplied" && data == &update["threadSettings"]))
            ));
        }
    }
    script.push(Step::Respond(final_message("final", "done")));
    let id = server.turn_start(&mut connection, &text_turn("run")).await;
    connection.response(id).await;
    connection.notification("turn/completed").await;
    assert!(
        !connection
            .passed
            .iter()
            .any(|message| message["method"] == "thread/settings/updated")
    );
    let id = server
        .request(
            &mut connection,
            "thread/settings/update",
            json!({"threadId": THREAD, "serviceTier": null}),
        )
        .await;
    connection.response(id).await;
    let update = connection.notification("thread/settings/updated").await;
    assert_eq!(update["threadSettings"]["serviceTier"], Value::Null);
    assert_eq!(update["threadSettings"]["summary"], "concise");
}

// ---------------------------------------------------------------------------------------------
// A turn's output schema: Codex's `output_schema.rs` suite and its steering rule
// ---------------------------------------------------------------------------------------------

/// Codex's schema in its `output_schema.rs` cases.
fn answer_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "answer": { "type": "string" }
        },
        "required": ["answer"],
        "additionalProperties": false
    })
}

/// A Responses endpoint that answers every request with one assistant message, as Codex's
/// `ev_response_created`, `ev_assistant_message` and `ev_completed` do.
async fn responses_endpoint() -> wiremock::MockServer {
    let message = json!({
        "id": "msg-1",
        "type": "message",
        "role": "assistant",
        "status": "completed",
        "content": [{"type": "output_text", "text": "Done"}]
    });
    let frames = [
        json!({
            "type": "response.created",
            "sequence_number": 0,
            "response": {"id": "resp-1", "status": "in_progress"}
        }),
        json!({
            "type": "response.output_item.done",
            "sequence_number": 1,
            "output_index": 0,
            "item": message
        }),
        json!({
            "type": "response.completed",
            "sequence_number": 2,
            "response": {
                "id": "resp-1",
                "object": "response",
                "status": "completed",
                "output": [message],
                "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}
            }
        }),
    ];
    let body: String = frames
        .iter()
        .map(|frame| format!("data: {frame}\n\n"))
        .collect();
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/v1/responses"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
        .mount(&server)
        .await;
    server
}

/// Resolves every model to the Responses adapter talking to `base_url`.
struct ResponsesResolver(String);

impl ModelResolver for ResponsesResolver {
    fn resolve_model(&self, model_name: Option<&str>) -> Result<ResolvedModel> {
        let model = ra_model::openai::responses::OpenAiResponsesModel::new(
            model_name.unwrap_or("mock-model"),
            ra_model::openai::auth::OpenAiAuth::new("test-secret")
                .with_base_url(format!("{}/v1/", self.0)),
        )?;
        Ok(ResolvedModel::new(
            ModelSelector::new(
                ProviderKey::new("openai"),
                Some(model_name.unwrap_or("mock-model").to_owned()),
                ApiProtocol::OpenAiResponses,
            ),
            Arc::new(model) as Arc<dyn Model>,
            ModelSettings::new(),
            ModelSettings::new(),
        ))
    }
}

fn responses_thread(endpoint: &wiremock::MockServer) -> AgentThread {
    let agent = AgentSpec::builder()
        .id(AgentId::new("lead"))
        .name("Lead")
        .instructions("lead")
        .build()
        .unwrap();
    AgentThread::builder(
        SessionId::new(THREAD),
        AgentBinding::direct(agent),
        Arc::new(ResponsesResolver(endpoint.uri())) as Arc<dyn ModelResolver>,
    )
    .build()
}

/// The bodies of the requests `endpoint` has received, in order.
async fn request_bodies(endpoint: &wiremock::MockServer) -> Vec<Value> {
    endpoint
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .map(|request| serde_json::from_slice(&request.body).unwrap())
        .collect()
}

#[test]
fn turn_start_params_carry_an_output_schema() {
    let params = text_turn("Hello").with_output_schema(answer_schema());
    let wire = serde_json::to_value(&params).unwrap();
    assert_eq!(wire["outputSchema"], answer_schema());
    let request = JsonRpcRequest::new(1, "turn/start").with_params(wire);
    let ClientRequest::TurnStart { params, .. } = ClientRequest::try_from(request).unwrap() else {
        panic!("expected turn/start");
    };
    assert_eq!(params.output_schema(), Some(&answer_schema()));

    // Codex's `Option<JsonValue>`: null is no schema.
    let request = JsonRpcRequest::new(2, "turn/start")
        .with_params(json!({"threadId": THREAD, "input": [], "outputSchema": null}));
    let ClientRequest::TurnStart { params, .. } = ClientRequest::try_from(request).unwrap() else {
        panic!("expected turn/start");
    };
    assert_eq!(params.output_schema(), None);
    assert_eq!(params.unsupported_field(), None);
}

/// Codex's `turn_start_accepts_output_schema_v2`.
#[tokio::test]
async fn turn_start_accepts_output_schema() {
    let endpoint = responses_endpoint().await;
    let server = Server::start();
    let mut connection = server.connect(1, false).await;
    server
        .load_thread(responses_thread(&endpoint), &[&connection])
        .await;

    let id = server
        .turn_start(
            &mut connection,
            &text_turn("Hello").with_output_schema(answer_schema()),
        )
        .await;
    assert_eq!(
        connection.response(id).await["turn"]["status"],
        "inProgress"
    );
    let completed = connection.notification("turn/completed").await;
    assert_eq!(completed["turn"]["status"], "completed", "{completed}");

    let bodies = request_bodies(&endpoint).await;
    assert_eq!(bodies.len(), 1);
    let text = bodies[0].get("text").expect("request missing text field");
    let format = text
        .get("format")
        .expect("request missing text.format field");
    assert_eq!(
        format,
        &json!({
            "name": "codex_output_schema",
            "type": "json_schema",
            "strict": true,
            "schema": answer_schema(),
        })
    );
}

/// Codex's `turn_start_output_schema_is_per_turn_v2`.
#[tokio::test]
async fn turn_start_output_schema_is_per_turn() {
    let endpoint = responses_endpoint().await;
    let server = Server::start();
    let mut connection = server.connect(1, false).await;
    server
        .load_thread(responses_thread(&endpoint), &[&connection])
        .await;

    let id = server
        .turn_start(
            &mut connection,
            &text_turn("Hello").with_output_schema(answer_schema()),
        )
        .await;
    connection.response(id).await;
    connection.notification("turn/completed").await;
    let bodies = request_bodies(&endpoint).await;
    assert_eq!(
        bodies[0].pointer("/text/format"),
        Some(&json!({
            "name": "codex_output_schema",
            "type": "json_schema",
            "strict": true,
            "schema": answer_schema(),
        }))
    );

    let id = server
        .turn_start(&mut connection, &text_turn("Hello again"))
        .await;
    connection.response(id).await;
    connection.notification("turn/completed").await;
    let bodies = request_bodies(&endpoint).await;
    assert_eq!(bodies.len(), 2);
    assert_eq!(bodies[1].pointer("/text/format"), None);
}

/// Codex's `start_or_steer_turn_requires_matching_active_output_schema`, through `turn/start`:
/// input that would steer a turn running with another schema is refused with Codex's answer, and
/// its settings are not applied; an equal schema, whatever the order of its keys, steers.
#[tokio::test]
async fn turn_start_steers_only_a_turn_with_the_same_output_schema() {
    let server = Server::start();
    let mut connection = server.connect(1, false).await;
    let script = Arc::new(Script::default());
    let thread = server.load(&script, &[&connection]).await;
    let active_schema: Value = serde_json::from_str(
        r#"{"type":"object","properties":{"answer":{"type":"string"},"count":{"type":"number"}},"required":["answer","count"]}"#,
    )
    .unwrap();
    let matching_schema_with_different_object_order: Value = serde_json::from_str(
        r#"{"required":["answer","count"],"properties":{"count":{"type":"number"},"answer":{"type":"string"}},"type":"object"}"#,
    )
    .unwrap();
    let different_schema: Value = serde_json::from_str(
        r#"{"type":"object","properties":{"answer":{"type":"number"}},"required":["answer"]}"#,
    )
    .unwrap();
    let (release, wait) = gate();
    script.push(Step::After(wait, ModelResponse::new(Vec::new())));
    script.push(Step::Respond(final_message("m-2", "{}")));

    let id = server
        .turn_start(
            &mut connection,
            &text_turn("start turn").with_output_schema(active_schema.clone()),
        )
        .await;
    let turn_id = connection.response(id).await["turn"]["id"].clone();
    script.wait_for_calls(1).await;

    let id = server
        .turn_start(
            &mut connection,
            &text_turn("rejected steer")
                .with_model("other-model")
                .with_output_schema(different_schema),
        )
        .await;
    assert_eq!(
        connection.error(id).await,
        json!({
            "code": -32603,
            "message": "failed to submit turn input: ActiveTurnOutputSchemaMismatch"
        })
    );
    assert_eq!(thread.settings().model(), None);

    let id = server
        .turn_start(
            &mut connection,
            &text_turn("accepted steer")
                .with_output_schema(matching_schema_with_different_object_order),
        )
        .await;
    assert_eq!(connection.response(id).await["turn"]["id"], turn_id);

    release.notify_one();
    connection.notification("turn/completed").await;
    let calls = script.calls();
    assert_eq!(calls.len(), 2);
    let second_request = texts(&calls[1].input);
    assert!(second_request.contains(&"accepted steer".to_owned()));
    assert!(!second_request.contains(&"rejected steer".to_owned()));
    let expected =
        OutputSchema::json_schema("codex_output_schema", active_schema).to_model_output_schema();
    for call in &calls {
        assert_eq!(call.output_schema, expected);
        assert_eq!(call.model, None);
    }
}

/// The coding adapter receives terminal events through the real listener, including cancellation
/// before a pending command enters the executor.
#[tokio::test]
async fn coding_command_records_are_released_when_the_turn_ends() {
    use ra_coding::{
        host::CodingHost,
        thread_settings::{CodingThreadSettings, CodingThreadSettingsAdapter},
    };
    use ra_core::{context::RunContext, item::CallId, state::RunId, tool::ToolContext};
    use ra_protocol::server::protocol::SandboxPolicy;

    for interrupt in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let host = CodingHost::open(dir.path()).unwrap();
        let adapter = Arc::new(
            CodingThreadSettingsAdapter::new(
                host.workspace_root(),
                SandboxPolicy::ReadOnly {
                    network_access: false,
                },
            )
            .unwrap(),
        );
        let server = Server::start();
        let mut connection = server.connect(1, true).await;
        let script = Arc::new(Script::default());
        let (release, wait) = gate();
        script.push(Step::After(wait, final_message("answer", "done")));
        let agent = AgentSpec::builder()
            .id(AgentId::new("lead"))
            .name("lead")
            .instructions("lead")
            .build()
            .unwrap();
        let thread = AgentThread::builder(
            SessionId::new(THREAD),
            AgentBinding::direct(agent.clone()),
            Arc::new(Resolver(Arc::clone(&script))),
        )
        .with_config(adapter.initialize(host.build_run_config()))
        .build();
        server
            .loader
            .load_with_adapter(thread.clone(), &[connection.id], Some(adapter))
            .await;
        let request = server.turn_start(&mut connection, &text_turn("work")).await;
        let turn_id = connection.response(request).await["turn"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        connection.notification("turn/started").await;
        script.wait_for_calls(1).await;

        let config = thread.run_config();
        let run_id = RunId::new(&turn_id);
        let run = RunContext::new(run_id.clone(), &agent)
            .with_app_context(config.app_context().unwrap().clone());
        let tool = host.exec_command_tool().unwrap();
        let call_id = CallId::new("pending");
        let arguments =
            json!({"cmd": "touch pending.txt", "sandbox_permissions": "require_escalated"});
        let context = ToolContext::new(&run, tool.as_ref(), &call_id, &arguments);
        assert!(tool.needs_approval(&context).await.unwrap());
        let settings = config
            .app_context()
            .unwrap()
            .downcast_ref::<CodingThreadSettings>()
            .unwrap();
        assert!(
            settings
                .pending_command_requirement(&run_id, &call_id)
                .is_some()
        );

        if interrupt {
            let request = server
                .request(
                    &mut connection,
                    "turn/interrupt",
                    serde_json::to_value(TurnInterruptParams::new(THREAD, &turn_id)).unwrap(),
                )
                .await;
            connection.response(request).await;
        } else {
            release.notify_one();
        }
        let completed = connection.notification("turn/completed").await;
        assert_eq!(
            completed["turn"]["status"],
            if interrupt {
                "interrupted"
            } else {
                "completed"
            }
        );
        assert!(
            settings
                .pending_command_requirement(&run_id, &call_id)
                .is_none()
        );
        assert!(!dir.path().join("pending.txt").exists());
    }
}
