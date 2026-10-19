//! Loading and reading threads, as Codex's app-server does it: `thread/start`, `thread/resume`,
//! `thread/fork`, `thread/read`, `thread/loaded/list`, `thread/started` and
//! `thread/status/changed` (`app-server-protocol/src/protocol/v2/{thread,thread_data}.rs`,
//! `app-server/src/request_processors/{thread_processor,thread_lifecycle}.rs`,
//! `app-server/src/thread_status.rs`).
//!
//! The server runs in this process with connections whose write queues the test holds. Threads are
//! prepared by a test factory whose agent runs on a scripted model, and are recorded into a thread
//! store the test can also write rollouts into, as Codex's cases write rollout files. The cases
//! port Codex's `thread_start`, `thread_resume`, `thread_fork`, `thread_read`,
//! `thread_loaded_list` and `thread_status` suites where they apply to what is ported; the rest
//! cover what this port adds or changes.
//! Persistent management cases also port archive, unarchive, delete, list, name and Git updates.

#[path = "threads/thread_list.rs"]
mod thread_list;
#[path = "threads/thread_read_state.rs"]
mod thread_read_state;

#[path = "threads/coding_tools.rs"]
mod coding_tools;

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use ra_coding::{
    CodingHost, thread_factory::CodingThreadFactory, thread_settings::CodingThreadSettingsAdapter,
};
use ra_core::{
    agent::{AgentId, AgentSpec, HandoffSpec},
    capability::{Capability, CapabilityFamily},
    error::{Error, Result},
    event::{EventTimestamp, HostEvent, HostEventBody},
    item::{
        CallId, ItemId, Message, ModelInputItem, ModelResponse, OutputPhase, RunItem, RunItemKind,
        ToolCall,
    },
    model::{
        ApiProtocol, Model, ModelRequest, ModelResolver, ModelSelector, ModelSettings, ProviderKey,
        ResolvedModel,
    },
    permission::{PermissionDecision, PermissionMode, PermissionRule},
    session::{
        SessionId,
        rollout::{
            RolloutItem, RolloutRecorder, RolloutRunEnd, RolloutRunEnded, RolloutRunStarted,
        },
    },
    state::{RunId, RunState},
    tool::{
        Tool, ToolApprovalPolicy, ToolAvailability, ToolContext, ToolOptions, ToolOrigin,
        ToolOutput, ToolSchema,
    },
};
use ra_protocol::{
    frame::{JsonRpcMessage, JsonRpcRequest},
    server::{
        AppServer, PreparedThread, ThreadFactory, ThreadFactoryRequest, ThreadOrigin,
        protocol::{
            ClientInfo, CommandExecutionRequestApprovalParams, InitializeCapabilities,
            InitializeParams, SandboxPolicy, ServerRequestPayload, Thread, ThreadForkParams,
            ThreadForkResponse, ThreadItem, ThreadLoadedListParams, ThreadLoadedListResponse,
            ThreadReadParams, ThreadReadResponse, ThreadResumeInitialTurnsPageParams,
            ThreadResumeParams, ThreadResumeResponse, ThreadStartParams, ThreadStartResponse,
            ThreadStatus, Turn, TurnItemsView, TurnStartParams, TurnStatus, UserInput,
        },
        test_api::ThreadRequester,
    },
    transport::{
        CHANNEL_CAPACITY, ConnectionId, ConnectionOrigin, QueuedOutgoingMessage, TransportEvent,
        test_api::connection_opened,
    },
};
use ra_runtime::{
    agent::{AgentBinding, AgentRegistry},
    permission::PermissionEngine,
    runner::RunConfig,
    thread::AgentThread,
};
use ra_session::{
    RolloutSessionMeta,
    rollout::RolloutThreadDirectory,
    store::{
        ArchiveThreadParams, CreateThreadParams, InMemoryThreadStore, ReadThreadParams,
        ThreadMetadataPatch, ThreadStore, UpdateThreadMetadataParams,
    },
};
use serde_json::{Value, json};
use tokio::{
    sync::{Notify, mpsc},
    task::JoinHandle,
    time::timeout,
};
use tokio_util::sync::CancellationToken;

/// How long a case waits for any one message.
const PATIENCE: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------------------------
// A scripted model and the factory that runs it
// ---------------------------------------------------------------------------------------------

enum Step {
    Respond(ModelResponse),
    After(Arc<Notify>, ModelResponse),
    Fail,
}

#[derive(Default)]
struct Script {
    steps: Mutex<VecDeque<Step>>,
    requests: Mutex<Vec<ModelRequest>>,
    called: Notify,
}

impl Script {
    fn push(&self, step: Step) {
        self.steps.lock().unwrap().push_back(step);
    }

    fn answer(&self, text: &str) {
        self.push(Step::Respond(final_message(text)));
    }

    async fn wait_for_calls(&self, count: usize) {
        timeout(PATIENCE, async {
            loop {
                let called = self.called.notified();
                if self.requests.lock().unwrap().len() >= count {
                    return;
                }
                called.await;
            }
        })
        .await
        .expect("the model is called");
    }

    fn last_request(&self) -> ModelRequest {
        self.requests.lock().unwrap().last().cloned().unwrap()
    }
}

/// Resolves any model but those named `unknown-*`, all on one provider.
struct Resolver(Arc<Script>);

impl ModelResolver for Resolver {
    fn resolve_model(&self, model_name: Option<&str>) -> Result<ResolvedModel> {
        let name = model_name.unwrap_or("mock-model");
        if name.starts_with("unknown") {
            return Err(Error::config(format!("unknown model {name}")));
        }
        Ok(ResolvedModel::new(
            ModelSelector::new(
                ProviderKey::new("test-provider"),
                Some(name.to_owned()),
                ApiProtocol::OpenAiResponses,
            ),
            Arc::new(ScriptedModel(Arc::clone(&self.0))) as Arc<dyn Model>,
            ModelSettings::new(),
            ModelSettings::new(),
        ))
    }
}

struct ScriptedModel(Arc<Script>);

#[async_trait]
impl Model for ScriptedModel {
    async fn get_response(&self, request: ModelRequest) -> Result<ModelResponse> {
        self.0.requests.lock().unwrap().push(request);
        self.0.called.notify_waiters();
        let step = self.0.steps.lock().unwrap().pop_front();
        match step {
            Some(Step::Respond(response)) => Ok(response),
            Some(Step::After(gate, response)) => {
                gate.notified().await;
                Ok(response)
            }
            Some(Step::Fail) => Err(Error::caller("the model failed")),
            None => Err(Error::caller("the model ran out of responses")),
        }
    }
}

fn final_message(text: &str) -> ModelResponse {
    ModelResponse::new(vec![RunItem::new(
        ItemId::new(format!("answer-{}", uuid_like())),
        RunItemKind::Message(Message::assistant(text, OutputPhase::Final)),
    )])
}

fn uuid_like() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

fn agent() -> Arc<AgentSpec> {
    AgentSpec::builder()
        .id(AgentId::new("lead"))
        .name("Lead")
        .instructions("lead")
        .build()
        .unwrap()
}

/// Prepares threads on the script, with the coding settings adapter if it has one, and remembers
/// what it was asked for.
struct Factory {
    script: Arc<Script>,
    adapter: Option<CodingThreadSettingsAdapter>,
    agent: Arc<AgentSpec>,
    config: RunConfig,
    requests: Mutex<Vec<ThreadFactoryRequest>>,
}

impl Factory {
    fn new(script: &Arc<Script>) -> Arc<Self> {
        Arc::new(Self::running(script, false, agent(), RunConfig::new()))
    }

    fn with_adapter(script: &Arc<Script>) -> Arc<Self> {
        Arc::new(Self::running(script, true, agent(), RunConfig::new()))
    }

    /// Prepares threads running `agent` with `config`, with the coding settings adapter if
    /// `adapter`.
    fn running(
        script: &Arc<Script>,
        adapter: bool,
        agent: Arc<AgentSpec>,
        config: RunConfig,
    ) -> Self {
        Self {
            script: Arc::clone(script),
            adapter: adapter.then(|| {
                CodingThreadSettingsAdapter::new(
                    "/work",
                    SandboxPolicy::WorkspaceWrite {
                        writable_roots: Vec::new(),
                        network_access: false,
                        exclude_tmpdir_env_var: false,
                        exclude_slash_tmp: false,
                    },
                )
                .unwrap()
            }),
            agent,
            config,
            requests: Mutex::default(),
        }
    }

    fn origins(&self) -> Vec<ThreadOrigin> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .map(|request| request.origin().clone())
            .collect()
    }
}

#[async_trait]
impl ThreadFactory for Factory {
    async fn prepare_thread(&self, request: &ThreadFactoryRequest) -> Result<PreparedThread> {
        self.requests.lock().unwrap().push(request.clone());
        let config = match &self.adapter {
            Some(adapter) => adapter.initialize(self.config.clone()),
            None => self.config.clone(),
        };
        let builder = AgentThread::builder(
            request.thread_id().clone(),
            AgentBinding::direct(Arc::clone(&self.agent)),
            Arc::new(Resolver(Arc::clone(&self.script))) as Arc<dyn ModelResolver>,
        )
        .with_config(config);
        let prepared = PreparedThread::new(builder);
        Ok(match &self.adapter {
            Some(adapter) => prepared.with_settings_adapter(Arc::new(adapter.clone())),
            None => prepared,
        })
    }
}

// ---------------------------------------------------------------------------------------------
// The server in this process
// ---------------------------------------------------------------------------------------------

struct Server {
    events: mpsc::Sender<TransportEvent>,
    store: Arc<dyn ThreadStore>,
    _task: JoinHandle<()>,
}

struct Options {
    store: Option<Arc<dyn ThreadStore>>,
    factory: Option<Arc<dyn ThreadFactory>>,
    unload_delay: Option<Duration>,
}

impl Server {
    /// A server on an in-memory store of its own, building threads with `factory`.
    fn start(factory: Arc<dyn ThreadFactory>) -> Self {
        build(
            Options {
                store: Some(Arc::new(InMemoryThreadStore::new())),
                factory: Some(factory),
                unload_delay: None,
            },
            None,
        )
        .0
    }

    fn run(server: AppServer, store: Arc<dyn ThreadStore>) -> Self {
        let (events, transport_event_rx) = mpsc::channel(CHANNEL_CAPACITY);
        let task = tokio::spawn(server.run(transport_event_rx));
        Self {
            events,
            store,
            _task: task,
        }
    }

    /// Opens and initializes a connection, with the experimental API.
    async fn connect(&self, id: u64) -> Connection {
        self.connect_with(
            id,
            InitializeCapabilities::new().with_experimental_api(true),
        )
        .await
    }

    async fn connect_with(&self, id: u64, capabilities: InitializeCapabilities) -> Connection {
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
            .with_capabilities(capabilities);
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

    async fn send(&self, connection: &Connection, message: impl Into<JsonRpcMessage>) {
        self.events
            .send(TransportEvent::IncomingMessage {
                connection_id: connection.id,
                message: message.into(),
            })
            .await
            .unwrap();
    }

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

    async fn call(&self, connection: &mut Connection, method: &str, params: Value) -> Value {
        let id = self.request(connection, method, params).await;
        connection.response(id).await
    }

    async fn call_error(&self, connection: &mut Connection, method: &str, params: Value) -> Value {
        let id = self.request(connection, method, params).await;
        connection.error(id).await
    }

    async fn start_thread(&self, connection: &mut Connection, params: ThreadStartParams) -> Thread {
        let result = self
            .call(
                connection,
                "thread/start",
                serde_json::to_value(params).unwrap(),
            )
            .await;
        serde_json::from_value::<ThreadStartResponse>(result)
            .unwrap()
            .thread()
            .clone()
    }

    async fn resume(
        &self,
        connection: &mut Connection,
        params: ThreadResumeParams,
    ) -> ThreadResumeResponse {
        let result = self
            .call(
                connection,
                "thread/resume",
                serde_json::to_value(params).unwrap(),
            )
            .await;
        serde_json::from_value(result).unwrap()
    }

    async fn fork(
        &self,
        connection: &mut Connection,
        params: ThreadForkParams,
    ) -> ThreadForkResponse {
        let result = self
            .call(
                connection,
                "thread/fork",
                serde_json::to_value(params).unwrap(),
            )
            .await;
        serde_json::from_value(result).unwrap()
    }

    async fn read(&self, connection: &mut Connection, params: ThreadReadParams) -> Thread {
        let result = self
            .call(
                connection,
                "thread/read",
                serde_json::to_value(params).unwrap(),
            )
            .await;
        serde_json::from_value::<ThreadReadResponse>(result)
            .unwrap()
            .thread()
            .clone()
    }

    /// Starts a turn on `text` and waits for it to complete.
    async fn run_turn(&self, connection: &mut Connection, thread_id: &str, text: &str) -> String {
        let turn_id = self.turn_start(connection, thread_id, text).await;
        connection.notification("turn/completed").await;
        turn_id
    }

    async fn turn_start(&self, connection: &mut Connection, thread_id: &str, text: &str) -> String {
        let params = TurnStartParams::new(thread_id, vec![UserInput::text(text)]);
        let result = self
            .call(
                connection,
                "turn/start",
                serde_json::to_value(params).unwrap(),
            )
            .await;
        result["turn"]["id"].as_str().unwrap().to_owned()
    }

    /// Writes a thread of `thread_id` into the store with a completed turn on `text`.
    async fn store_thread(&self, thread_id: &str, text: &str) -> Arc<dyn RolloutRecorder> {
        let recorder = self
            .store
            .create_thread_with(&CreateThreadParams::new(
                RolloutSessionMeta::new(SessionId::new(thread_id))
                    .with_model_provider("stored-provider")
                    .with_cwd("/stored")
                    .with_cli_version("0.0.0"),
            ))
            .await
            .unwrap();
        record(&recorder, completed_run("turn-1", text, "Done")).await;
        recorder
    }
}

/// Builds a server from `options` and runs it, keeping a requester for `requester_thread` made
/// before it runs, as requesters must be.
fn build(options: Options, requester_thread: Option<&str>) -> (Server, Option<ThreadRequester>) {
    build_with_session_source(options, requester_thread, None)
}

fn build_with_session_source(
    options: Options,
    requester_thread: Option<&str>,
    source: Option<ra_protocol::server::SessionSource>,
) -> (Server, Option<ThreadRequester>) {
    let store = options
        .store
        .clone()
        .unwrap_or_else(|| Arc::new(InMemoryThreadStore::new()));
    let mut server = AppServer::new(std::env::temp_dir().join("rusty-agent-threads-test")).unwrap();
    if let Some(source) = source {
        server = server.with_session_source(source);
    }
    if options.store.is_some() {
        server = server.with_thread_store(Arc::clone(&store));
    }
    if let Some(factory) = options.factory {
        server = server.with_thread_factory(factory);
    }
    if let Some(delay) = options.unload_delay {
        server = server.with_thread_unload_delay(delay);
    }
    let requester =
        requester_thread.map(|thread| ThreadRequester::new(&server, &[], SessionId::new(thread)));
    (Server::run(server, store), requester)
}

/// A connection, what the server writes to it, and what a case read past while waiting.
struct Connection {
    id: ConnectionId,
    written: mpsc::Receiver<QueuedOutgoingMessage>,
    passed: VecDeque<Value>,
    next_id: i64,
}

impl Connection {
    async fn next(&mut self) -> Value {
        if let Some(message) = self.passed.pop_front() {
            return message;
        }
        let queued = timeout(PATIENCE, self.written.recv())
            .await
            .expect("the server writes a message")
            .expect("the server keeps the write queue");
        serde_json::to_value(queued.message()).unwrap()
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

    /// The result of request `id`, or its error.
    async fn answer(&mut self, id: i64) -> std::result::Result<Value, Value> {
        let message = self
            .until(|message| message["id"] == id && message.get("method").is_none())
            .await;
        message
            .get("result")
            .cloned()
            .ok_or_else(|| message["error"].clone())
    }

    async fn notification(&mut self, method: &str) -> Value {
        self.until(|message| message["method"] == method && message.get("id").is_none())
            .await["params"]
            .clone()
    }

    /// Everything written to it so far that a case has not read, once the server has had a moment
    /// to write what it was going to.
    async fn drain(&mut self) -> Vec<Value> {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let mut messages: Vec<Value> = self.passed.drain(..).collect();
        while let Ok(queued) = self.written.try_recv() {
            messages.push(serde_json::to_value(queued.message()).unwrap());
        }
        messages
    }
}

fn invalid_request(message: &str) -> Value {
    json!({"code": -32600, "message": message})
}

fn method_not_found(message: &str) -> Value {
    json!({"code": -32601, "message": message})
}

fn statuses_of(messages: &[Value], thread_id: &str) -> Vec<ThreadStatus> {
    messages
        .iter()
        .filter(|message| {
            message["method"] == "thread/status/changed"
                && message["params"]["threadId"] == thread_id
        })
        .map(|message| serde_json::from_value(message["params"]["status"].clone()).unwrap())
        .collect()
}

fn user_texts(turn: &Turn) -> Vec<String> {
    turn.items()
        .iter()
        .filter_map(|item| match item {
            ThreadItem::UserMessage { content, .. } => match content.first() {
                Some(UserInput::Text { text, .. }) => Some(text.clone()),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Rollout records
// ---------------------------------------------------------------------------------------------

fn run_started(run_id: &str, text: &str) -> RolloutItem {
    RolloutItem::RunStarted(
        RolloutRunStarted::new(RunId::new(run_id), AgentId::new("lead"))
            .with_input(vec![ModelInputItem::Message(Message::user(text))]),
    )
}

fn run_ended(run_id: &str, end: RolloutRunEnd) -> RolloutItem {
    RolloutItem::RunEnded(RolloutRunEnded::new(RunId::new(run_id), end))
}

fn agent_record(id: &str, text: &str) -> RolloutItem {
    RolloutItem::Item(RunItem::new(
        ItemId::new(id),
        RunItemKind::Message(Message::assistant(text, OutputPhase::Final)),
    ))
}

fn completed_run(run_id: &str, text: &str, answer: &str) -> Vec<RolloutItem> {
    vec![
        run_started(run_id, text),
        agent_record(&format!("{run_id}-answer"), answer),
        run_ended(run_id, RolloutRunEnd::Completed),
    ]
}

async fn record(recorder: &Arc<dyn RolloutRecorder>, items: impl IntoIterator<Item = RolloutItem>) {
    for item in items {
        recorder.record(item);
    }
    recorder.flush().await.unwrap();
}

// ---------------------------------------------------------------------------------------------
// thread/start
// ---------------------------------------------------------------------------------------------

/// Ported from Codex's `thread_start_creates_thread_and_emits_started`: the thread is answered with
/// its id, an empty preview, its model and `idle`; `name` is written as `null`, `sessionId` sits on
/// the thread rather than the answer, and every connection is told with `thread/started` — without
/// a `thread/status/changed` first. The history mode is `legacy`, the only one a store here keeps.
#[tokio::test]
async fn thread_start_creates_thread_and_emits_started() {
    let script = Arc::new(Script::default());
    let factory = Factory::new(&script);
    let server = Server::start(factory.clone());
    let mut connection = server.connect(1).await;
    let mut other = server.connect(2).await;

    let id = server
        .request(
            &mut connection,
            "thread/start",
            serde_json::to_value(
                ThreadStartParams::new()
                    .with_model("gpt-5.2")
                    .with_thread_source("user"),
            )
            .unwrap(),
        )
        .await;
    let result = connection.response(id).await;
    let response: ThreadStartResponse = serde_json::from_value(result.clone()).unwrap();
    let thread = response.thread().clone();
    assert!(!thread.id().is_empty());
    assert_eq!(thread.session_id(), thread.id());
    assert!(thread.preview().is_empty());
    assert_eq!(thread.model(), Some("gpt-5.2"));
    assert_eq!(thread.model_provider(), "test-provider");
    assert!(thread.created_at() > 0);
    assert!(!thread.ephemeral());
    assert_eq!(thread.status(), &ThreadStatus::Idle);
    assert_eq!(thread.thread_source(), Some("user"));
    assert_eq!(thread.can_accept_direct_input(), Some(true));
    assert_eq!(result["thread"]["name"], Value::Null);
    assert_eq!(result["thread"]["historyMode"], "legacy");
    assert_eq!(result["thread"]["threadSource"], "user");
    assert_eq!(result.get("sessionId"), None);
    assert_eq!(result["model"], "gpt-5.2");
    assert_eq!(result["modelProvider"], "test-provider");
    assert_eq!(result["instructionSources"], json!([]));
    assert_eq!(factory.origins(), vec![ThreadOrigin::Start]);

    for connection in [&mut connection, &mut other] {
        let started = connection
            .until(|message| message["method"] == "thread/started")
            .await;
        let before: Vec<Value> = connection.passed.iter().cloned().collect();
        assert!(statuses_of(&before, thread.id()).is_empty());
        let started: Thread = serde_json::from_value(started["params"]["thread"].clone()).unwrap();
        assert_eq!(started, thread);
    }

    // It is loaded, and it runs turns.
    script.answer("Done");
    server.run_turn(&mut connection, thread.id(), "hello").await;
    assert_eq!(
        script.last_request().model_settings().effort(),
        None,
        "a thread started without an effort runs without one"
    );
    let stored = server
        .store
        .read_thread(&ReadThreadParams::new(SessionId::new(thread.id())))
        .await
        .unwrap();
    assert_eq!(stored.thread_source(), Some("user"));
    assert_eq!(stored.model_provider(), Some("test-provider"));
}

/// Ported from Codex's `thread_start_ephemeral_remains_pathless`: an ephemeral thread is marked so
/// and keeps no rollout, so its history cannot be listed.
#[tokio::test]
async fn thread_start_ephemeral_remains_pathless() {
    let script = Arc::new(Script::default());
    let server = Server::start(Factory::new(&script));
    let mut connection = server.connect(1).await;

    let thread = server
        .start_thread(
            &mut connection,
            ThreadStartParams::new().with_ephemeral(true),
        )
        .await;
    assert!(thread.ephemeral());
    assert_eq!(thread.path(), None);
    script.answer("Done");
    server.run_turn(&mut connection, thread.id(), "hello").await;
    assert!(
        server
            .store
            .read_thread(&ReadThreadParams::new(SessionId::new(thread.id())))
            .await
            .is_err()
    );
    assert_eq!(
        server
            .call_error(
                &mut connection,
                "thread/turns/list",
                json!({"threadId": thread.id()})
            )
            .await,
        invalid_request("ephemeral threads do not support thread/turns/list")
    );
    assert_eq!(
        server
            .call_error(
                &mut connection,
                "thread/read",
                json!({"threadId": thread.id(), "includeTurns": true})
            )
            .await,
        invalid_request("ephemeral threads do not support includeTurns")
    );
}

/// What `thread/start` refuses, in Codex's words where Codex refuses it: a paginated history a
/// store here does not keep, `permissions` with `sandbox`, an experimental member without the
/// experimental API, a member this framework does not have — including a deferred dynamic tool,
/// which needs tool search — a model the host does not resolve, and a thread no factory or store
/// is there to build or keep.
#[tokio::test]
async fn thread_start_refusals() {
    let script = Arc::new(Script::default());
    let factory = Factory::with_adapter(&script);
    let server = Server::start(factory.clone());
    let mut connection = server.connect(1).await;
    let mut stable = server.connect_with(2, InitializeCapabilities::new()).await;

    let cases = [
        (
            json!({"historyMode": "paginated"}),
            invalid_request(
                "paginated threads require thread/turns/list and thread/items/list support",
            ),
        ),
        (
            json!({"sandbox": "read-only", "permissions": ":workspace"}),
            invalid_request("`permissions` cannot be combined with `sandbox`"),
        ),
        (
            json!({"cwd": "/elsewhere"}),
            invalid_request("thread/start.cwd is not supported"),
        ),
        (
            json!({"permissions": ":nope"}),
            invalid_request("invalid thread settings override: unknown permissions profile: :nope"),
        ),
    ];
    for (params, error) in cases {
        assert_eq!(
            server
                .call_error(&mut connection, "thread/start", params)
                .await,
            error
        );
    }
    assert_eq!(
        server
            .call_error(
                &mut stable,
                "thread/start",
                json!({"permissions": ":workspace"})
            )
            .await,
        invalid_request("thread/start.permissions requires experimentalApi capability")
    );
    let error = server
        .call_error(
            &mut connection,
            "thread/start",
            json!({"model": "unknown-model"}),
        )
        .await;
    assert_eq!(error["code"], -32600, "{error}");
    // Personality and the multi-agent mode are deprecated and ignored, as Codex ignores them.
    server
        .start_thread(
            &mut connection,
            serde_json::from_value(json!({"personality": "friendly", "multiAgentMode": "x"}))
                .unwrap(),
        )
        .await;
    let loaded: ThreadLoadedListResponse = serde_json::from_value(
        server
            .call(&mut connection, "thread/loaded/list", json!({}))
            .await,
    )
    .unwrap();
    assert_eq!(loaded.data().len(), 1, "a refused start loads nothing");

    // Without a factory nothing is built; without a store a durable thread is not kept.
    let (server, _) = build(
        Options {
            store: Some(Arc::new(InMemoryThreadStore::new())),
            factory: None,
            unload_delay: None,
        },
        None,
    );
    let mut connection = server.connect(1).await;
    assert_eq!(
        server
            .call_error(&mut connection, "thread/start", json!({}))
            .await,
        method_not_found("thread/start is not supported yet")
    );
    let (server, _) = build(
        Options {
            store: None,
            factory: Some(Factory::new(&script)),
            unload_delay: None,
        },
        None,
    );
    let mut connection = server.connect(1).await;
    assert_eq!(
        server
            .call_error(&mut connection, "thread/start", json!({}))
            .await,
        method_not_found("thread/start is not supported yet")
    );
    let thread = server
        .start_thread(
            &mut connection,
            ThreadStartParams::new().with_ephemeral(true),
        )
        .await;
    assert!(thread.ephemeral());
}

/// The product settings `thread/start` carries go through the host's settings adapter and come
/// back in the answer under Codex's names, as the thread then runs with them.
#[tokio::test]
async fn thread_start_applies_product_settings() {
    let script = Arc::new(Script::default());
    let server = Server::start(Factory::with_adapter(&script));
    let mut connection = server.connect(1).await;

    let result = server
        .call(
            &mut connection,
            "thread/start",
            json!({"approvalPolicy": "never", "sandbox": "read-only", "serviceTier": "flex"}),
        )
        .await;
    assert_eq!(result["approvalPolicy"], "never");
    assert_eq!(
        result["sandbox"],
        json!({"type": "readOnly", "networkAccess": false})
    );
    assert_eq!(result["cwd"], "/work");
    assert_eq!(result["serviceTier"], "flex");
    assert_eq!(result["thread"]["cwd"], "/work");
    let thread_id = result["thread"]["id"].as_str().unwrap().to_owned();

    let result = server
        .call(
            &mut connection,
            "thread/start",
            json!({"permissions": ":danger-full-access"}),
        )
        .await;
    assert_eq!(result["sandbox"], json!({"type": "dangerFullAccess"}));
    assert_eq!(
        result["activePermissionProfile"]["id"],
        ":danger-full-access"
    );

    // Without an adapter, product settings are refused.
    let server = Server::start(Factory::new(&script));
    let mut connection = server.connect(1).await;
    assert_eq!(
        server
            .call_error(
                &mut connection,
                "thread/start",
                json!({"approvalPolicy": "never"})
            )
            .await,
        invalid_request("the thread's host has not configured a permission settings adapter")
    );
    let _ = thread_id;
}

// ---------------------------------------------------------------------------------------------
// thread/status/changed, thread/read, thread/loaded/list
// ---------------------------------------------------------------------------------------------

/// Ported from Codex's `thread_status_changed_emits_runtime_updates`: a turn makes the thread
/// active, and its end makes it idle again.
#[tokio::test]
async fn thread_status_changed_emits_runtime_updates() {
    let script = Arc::new(Script::default());
    let server = Server::start(Factory::new(&script));
    let mut connection = server.connect(1).await;
    let thread = server
        .start_thread(&mut connection, ThreadStartParams::new())
        .await;

    script.answer("done");
    server
        .run_turn(&mut connection, thread.id(), "collect status updates")
        .await;
    let messages = connection.drain().await;
    let mut statuses = statuses_of(&messages, thread.id());
    // The idle status may follow `turn/completed`; read it once it has come.
    if statuses.last() != Some(&ThreadStatus::Idle) {
        let params = connection.notification("thread/status/changed").await;
        statuses.push(serde_json::from_value(params["status"].clone()).unwrap());
    }
    assert_eq!(statuses, vec![ThreadStatus::active(), ThreadStatus::Idle]);
}

/// Ported from Codex's `thread_status_changed_can_be_opted_out`.
#[tokio::test]
async fn thread_status_changed_can_be_opted_out() {
    let script = Arc::new(Script::default());
    let server = Server::start(Factory::new(&script));
    let mut connection = server
        .connect_with(
            1,
            InitializeCapabilities::new()
                .with_experimental_api(true)
                .with_opt_out_notification_methods(["thread/status/changed"]),
        )
        .await;
    let thread = server
        .start_thread(&mut connection, ThreadStartParams::new())
        .await;
    script.answer("done");
    server
        .run_turn(&mut connection, thread.id(), "run once")
        .await;
    assert!(statuses_of(&connection.drain().await, thread.id()).is_empty());
}

/// Ported from Codex's `thread_read_reports_system_error_idle_flag_after_failed_turn`.
#[tokio::test]
async fn thread_read_reports_system_error_after_failed_turn() {
    let script = Arc::new(Script::default());
    let server = Server::start(Factory::new(&script));
    let mut connection = server.connect(1).await;
    let thread = server
        .start_thread(&mut connection, ThreadStartParams::new())
        .await;
    script.push(Step::Fail);
    server
        .turn_start(&mut connection, thread.id(), "fail this turn")
        .await;
    connection.notification("error").await;
    connection.notification("turn/completed").await;
    let read = server
        .read(&mut connection, ThreadReadParams::new(thread.id()))
        .await;
    assert_eq!(read.status(), &ThreadStatus::SystemError);

    // The next turn clears it.
    script.answer("done");
    server.run_turn(&mut connection, thread.id(), "again").await;
    let read = server
        .read(&mut connection, ThreadReadParams::new(thread.id()))
        .await;
    assert_eq!(read.status(), &ThreadStatus::Idle);
}

/// Ported from Codex's `thread_loaded_list_returns_loaded_thread_ids` and
/// `thread_loaded_list_paginates`.
#[tokio::test]
async fn thread_loaded_list_returns_and_pages_loaded_thread_ids() {
    let script = Arc::new(Script::default());
    let server = Server::start(Factory::new(&script));
    let mut connection = server.connect(1).await;
    let empty: ThreadLoadedListResponse = serde_json::from_value(
        server
            .call(&mut connection, "thread/loaded/list", json!({}))
            .await,
    )
    .unwrap();
    assert!(empty.data().is_empty());

    let first = server
        .start_thread(&mut connection, ThreadStartParams::new())
        .await;
    let second = server
        .start_thread(&mut connection, ThreadStartParams::new())
        .await;
    let mut expected = [first.id().to_owned(), second.id().to_owned()];
    expected.sort();

    let all: ThreadLoadedListResponse = serde_json::from_value(
        server
            .call(&mut connection, "thread/loaded/list", json!({}))
            .await,
    )
    .unwrap();
    assert_eq!(all.data(), expected);
    assert_eq!(all.next_cursor(), None);

    let page = |cursor: Option<&str>| {
        let mut params = ThreadLoadedListParams::new().with_limit(1);
        if let Some(cursor) = cursor {
            params = params.with_cursor(cursor);
        }
        serde_json::to_value(params).unwrap()
    };
    let first_page: ThreadLoadedListResponse = serde_json::from_value(
        server
            .call(&mut connection, "thread/loaded/list", page(None))
            .await,
    )
    .unwrap();
    assert_eq!(first_page.data(), &expected[..1]);
    assert_eq!(first_page.next_cursor(), Some(expected[0].as_str()));
    let second_page: ThreadLoadedListResponse = serde_json::from_value(
        server
            .call(
                &mut connection,
                "thread/loaded/list",
                page(first_page.next_cursor()),
            )
            .await,
    )
    .unwrap();
    assert_eq!(second_page.data(), &expected[1..]);
    assert_eq!(second_page.next_cursor(), None);
}

/// Ported from Codex's `thread_read_returns_summary_without_turns` and
/// `thread_read_can_include_turns`: a stored thread that is not loaded is described by what the
/// store holds, without a model or effort it never recorded.
#[tokio::test]
async fn thread_read_returns_stored_thread_with_or_without_turns() {
    let script = Arc::new(Script::default());
    let server = Server::start(Factory::new(&script));
    let mut connection = server.connect(1).await;
    server.store_thread("stored", "Saved user message").await;

    let result = server
        .call(
            &mut connection,
            "thread/read",
            json!({"threadId": "stored"}),
        )
        .await;
    assert_eq!(result["thread"]["model"], Value::Null);
    assert_eq!(result["thread"]["reasoningEffort"], Value::Null);
    let thread: Thread = serde_json::from_value(result["thread"].clone()).unwrap();
    assert_eq!(thread.id(), "stored");
    assert_eq!(thread.preview(), "Saved user message");
    assert_eq!(thread.model_provider(), "stored-provider");
    assert!(!thread.ephemeral());
    assert_eq!(thread.cwd(), Some(std::path::Path::new("/stored")));
    assert_eq!(thread.cli_version(), "0.0.0");
    assert_eq!(thread.source(), &json!("unknown"));
    assert!(thread.turns().is_empty());
    assert_eq!(thread.status(), &ThreadStatus::NotLoaded);
    assert_eq!(thread.can_accept_direct_input(), None);

    let thread = server
        .read(
            &mut connection,
            ThreadReadParams::new("stored").including_turns(),
        )
        .await;
    assert_eq!(thread.turns().len(), 1);
    let turn = &thread.turns()[0];
    assert_eq!(turn.status(), TurnStatus::Completed);
    assert_eq!(turn.items_view(), TurnItemsView::Full);
    assert_eq!(user_texts(turn), ["Saved user message"]);
    assert_eq!(thread.status(), &ThreadStatus::NotLoaded);

    assert_eq!(
        server
            .call_error(
                &mut connection,
                "thread/read",
                json!({"threadId": "missing"})
            )
            .await,
        invalid_request("thread not loaded: missing")
    );
}

/// Ported from Codex's `thread_read_loaded_thread_returns_precomputed_path_before_materialization`,
/// `thread_read_include_turns_rejects_unmaterialized_loaded_thread` and
/// `thread_turns_list_rejects_unmaterialized_loaded_thread`, on a store that writes a thread's
/// rollout only once its first turn has run: before then a loaded thread is described by the live
/// thread, and its turns cannot be read.
#[tokio::test]
async fn loaded_thread_reads_before_its_rollout_is_written() {
    let dir = tempfile::tempdir().unwrap();
    let script = Arc::new(Script::default());
    let (server, _) = build(
        Options {
            store: Some(Arc::new(RolloutThreadDirectory::new(dir.path()))),
            factory: Some(Factory::new(&script)),
            unload_delay: None,
        },
        None,
    );
    let mut connection = server.connect(1).await;
    let thread = server
        .start_thread(&mut connection, ThreadStartParams::new())
        .await;

    let read = server
        .read(&mut connection, ThreadReadParams::new(thread.id()))
        .await;
    assert_eq!(read.id(), thread.id());
    assert!(read.preview().is_empty());
    assert!(read.turns().is_empty());
    assert_eq!(read.status(), &ThreadStatus::Idle);
    let not_materialized = |method: &str| {
        invalid_request(&format!(
            "thread {} is not materialized yet; {method} is unavailable before first user message",
            thread.id()
        ))
    };
    assert_eq!(
        server
            .call_error(
                &mut connection,
                "thread/read",
                json!({"threadId": thread.id(), "includeTurns": true})
            )
            .await,
        not_materialized("includeTurns")
    );
    assert_eq!(
        server
            .call_error(
                &mut connection,
                "thread/turns/list",
                json!({"threadId": thread.id()})
            )
            .await,
        not_materialized("thread/turns/list")
    );

    // Once a turn has run, the store describes it.
    script.answer("Done");
    server.run_turn(&mut connection, thread.id(), "first").await;
    let read = server
        .read(
            &mut connection,
            ThreadReadParams::new(thread.id()).including_turns(),
        )
        .await;
    assert_eq!(read.preview(), "first");
    assert!(read.path().is_some());
    assert_eq!(read.model(), Some("mock-model"));
    assert_eq!(read.turns().len(), 1);
}

// ---------------------------------------------------------------------------------------------
// thread/resume
// ---------------------------------------------------------------------------------------------

/// Ported from Codex's `thread_resume_returns_rollout_history`: resuming a stored thread that is
/// not loaded loads it on its history — the next turn continues the conversation — answers with its
/// turns and `idle`, and tells every connection its status.
#[tokio::test]
async fn thread_resume_returns_rollout_history() {
    let script = Arc::new(Script::default());
    let factory = Factory::new(&script);
    let server = Server::start(factory.clone());
    let mut connection = server.connect(1).await;
    server.store_thread("stored", "Saved user message").await;

    let response = server
        .resume(&mut connection, ThreadResumeParams::new("stored"))
        .await;
    let thread = response.thread();
    assert_eq!(thread.id(), "stored");
    assert_eq!(thread.preview(), "Saved user message");
    assert_eq!(thread.model_provider(), "stored-provider");
    assert_eq!(thread.cwd(), Some(std::path::Path::new("/stored")));
    assert_eq!(thread.status(), &ThreadStatus::Idle);
    assert_eq!(thread.can_accept_direct_input(), Some(true));
    assert_eq!(thread.turns().len(), 1);
    assert_eq!(thread.turns()[0].status(), TurnStatus::Completed);
    assert_eq!(user_texts(&thread.turns()[0]), ["Saved user message"]);
    assert_eq!(response.settings()["model"], "mock-model");
    assert_eq!(factory.origins(), vec![ThreadOrigin::Resume]);
    let params = connection.notification("thread/status/changed").await;
    assert_eq!(
        params,
        json!({"threadId": "stored", "status": {"type": "idle"}})
    );

    script.answer("Again");
    server.run_turn(&mut connection, "stored", "continue").await;
    let input = script.last_request().input().to_vec();
    let texts: Vec<_> = input
        .iter()
        .filter_map(|item| match item {
            ModelInputItem::Message(message) => Some(message.text_content()),
            _ => None,
        })
        .collect();
    assert_eq!(texts, ["Saved user message", "Done", "continue"]);
}

/// Ported from Codex's `thread_resume_can_skip_turns_for_metadata_only_resume`, with Codex's
/// `initialTurnsPage` alongside.
#[tokio::test]
async fn thread_resume_can_skip_turns_and_include_a_page() {
    let script = Arc::new(Script::default());
    let server = Server::start(Factory::new(&script));
    let mut connection = server.connect(1).await;
    let recorder = server.store_thread("stored", "first").await;
    record(&recorder, completed_run("turn-2", "second", "Done")).await;
    drop(recorder);

    let response = server
        .resume(
            &mut connection,
            ThreadResumeParams::new("stored")
                .excluding_turns()
                .with_initial_turns_page(ThreadResumeInitialTurnsPageParams::new().with_limit(1)),
        )
        .await;
    assert!(response.thread().turns().is_empty());
    let page = response.initial_turns_page().unwrap();
    assert_eq!(page.data().len(), 1);
    assert_eq!(page.data()[0].id(), "turn-2");
    assert_eq!(page.data()[0].items_view(), TurnItemsView::Summary);
    assert!(page.next_cursor().is_some());
}

/// Ported from Codex's `thread_resume_rejects_unmaterialized_thread` and
/// `thread_resume_rejects_archived_session_by_id`; the archived thread is pointed at
/// `thread/unarchive` rather than Codex's command line.
#[tokio::test]
async fn thread_resume_rejects_missing_and_archived_threads() {
    let script = Arc::new(Script::default());
    let server = Server::start(Factory::new(&script));
    let mut connection = server.connect(1).await;
    assert_eq!(
        server
            .call_error(
                &mut connection,
                "thread/resume",
                json!({"threadId": "missing"})
            )
            .await,
        invalid_request("no rollout found for thread id missing")
    );
    // The in-memory store keeps no archive, as Codex's does not; the rollout directory does.
    let dir = tempfile::tempdir().unwrap();
    let (server, _) = build(
        Options {
            store: Some(Arc::new(RolloutThreadDirectory::new(dir.path()))),
            factory: Some(Factory::new(&script)),
            unload_delay: None,
        },
        None,
    );
    let mut connection = server.connect(1).await;
    let recorder = server
        .store_thread("archived", "Archived saved user message")
        .await;
    recorder.shutdown().await.unwrap();
    drop(recorder);
    server
        .store
        .archive_thread(&ArchiveThreadParams::new(SessionId::new("archived")))
        .await
        .unwrap();
    assert_eq!(
        server
            .call_error(
                &mut connection,
                "thread/resume",
                json!({"threadId": "archived"})
            )
            .await,
        invalid_request("session archived is archived. Unarchive it with thread/unarchive first.")
    );
    // Codex's `thread_read_can_return_archived_threads_by_id`.
    let read = server
        .read(&mut connection, ThreadReadParams::new("archived"))
        .await;
    assert_eq!(read.preview(), "Archived saved user message");
    // Members Codex reserves for its cloud are refused.
    assert_eq!(
        server
            .call_error(
                &mut connection,
                "thread/resume",
                json!({"threadId": "archived", "history": []})
            )
            .await,
        invalid_request("thread/resume.history is not supported")
    );
}

/// Ported from Codex's `thread_resume_and_read_interrupt_incomplete_rollout_turn_when_thread_is_idle`:
/// a turn the rollout left in progress was cut off, and reads as interrupted once the thread is
/// loaded and idle, as before.
#[tokio::test]
async fn thread_resume_and_read_interrupt_incomplete_rollout_turn_when_thread_is_idle() {
    let script = Arc::new(Script::default());
    let server = Server::start(Factory::new(&script));
    let mut connection = server.connect(1).await;
    let recorder = server.store_thread("stored", "Saved user message").await;
    record(
        &recorder,
        [
            run_started("incomplete-turn", "still"),
            agent_record("still-running", "Still running"),
        ],
    )
    .await;
    drop(recorder);

    for _ in 0..2 {
        let response = server
            .resume(&mut connection, ThreadResumeParams::new("stored"))
            .await;
        let thread = response.thread();
        assert_eq!(thread.status(), &ThreadStatus::Idle);
        assert_eq!(thread.turns().len(), 2);
        assert_eq!(thread.turns()[0].status(), TurnStatus::Completed);
        assert_eq!(thread.turns()[1].id(), "incomplete-turn");
        assert_eq!(thread.turns()[1].status(), TurnStatus::Interrupted);
    }
    let read = server
        .read(
            &mut connection,
            ThreadReadParams::new("stored").including_turns(),
        )
        .await;
    assert_eq!(read.status(), &ThreadStatus::Idle);
    assert_eq!(read.turns()[1].status(), TurnStatus::Interrupted);
}

/// Ported from Codex's `thread_resume_keeps_in_flight_turn_streaming`, with what the acceptance
/// criterion asks of it: a connection that resumes a thread while a turn runs gets the turn in
/// progress, with what it has made so far, and then hears the rest of it — nothing it was given is
/// sent again and nothing after is missed.
#[tokio::test]
async fn thread_resume_keeps_in_flight_turn_streaming() {
    let script = Arc::new(Script::default());
    let server = Server::start(Factory::new(&script));
    let mut primary = server.connect(1).await;
    let thread = server
        .start_thread(&mut primary, ThreadStartParams::new())
        .await;
    script.answer("seeded");
    server
        .run_turn(&mut primary, thread.id(), "seed history")
        .await;

    let gate = Arc::new(Notify::new());
    script.push(Step::After(Arc::clone(&gate), final_message("docs")));
    let turn_id = server
        .turn_start(&mut primary, thread.id(), "respond with docs")
        .await;
    script.wait_for_calls(2).await;

    let mut second = server.connect(2).await;
    let response = server
        .resume(&mut second, ThreadResumeParams::new(thread.id()))
        .await;
    let resumed = response.thread();
    assert_eq!(resumed.status(), &ThreadStatus::active());
    assert_eq!(resumed.turns().len(), 2);
    let running = &resumed.turns()[1];
    assert_eq!(running.id(), turn_id);
    assert_eq!(running.status(), TurnStatus::InProgress);
    assert_eq!(user_texts(running), ["respond with docs"]);

    gate.notify_one();
    let completed = second.notification("turn/completed").await;
    assert_eq!(completed["turn"]["id"], turn_id.as_str());
    let messages = second.drain().await;
    let user_messages_resent = messages.iter().any(|message| {
        message["method"] == "item/completed" && message["params"]["item"]["type"] == "userMessage"
    });
    assert!(!user_messages_resent, "{messages:?}");
    let answers: Vec<_> = second
        .passed
        .iter()
        .chain(&messages)
        .filter(|message| {
            message["method"] == "item/completed"
                && message["params"]["item"]["type"] == "agentMessage"
        })
        .collect();
    assert!(answers.len() <= 1);
}

/// Ported from Codex's `thread_resume_can_skip_turns_when_thread_is_running` and
/// `thread_resume_with_empty_path_uses_running_thread_id`: attaching to a loaded idle thread
/// answers without turns when asked, and an empty path is no path.
#[tokio::test]
async fn thread_resume_can_skip_turns_when_thread_is_loaded() {
    let script = Arc::new(Script::default());
    let factory = Factory::new(&script);
    let server = Server::start(factory.clone());
    let mut primary = server.connect(1).await;
    let thread = server
        .start_thread(&mut primary, ThreadStartParams::new())
        .await;
    script.answer("Done");
    server
        .run_turn(&mut primary, thread.id(), "seed history")
        .await;

    let result = server
        .call(
            &mut primary,
            "thread/resume",
            json!({"threadId": thread.id(), "excludeTurns": true, "path": ""}),
        )
        .await;
    let response: ThreadResumeResponse = serde_json::from_value(result).unwrap();
    assert_eq!(response.thread().id(), thread.id());
    assert_eq!(response.thread().status(), &ThreadStatus::Idle);
    assert!(response.thread().turns().is_empty());
    assert_eq!(
        factory.origins(),
        vec![ThreadOrigin::Start],
        "nothing is reloaded"
    );

    // A second connection attaching is subscribed: it hears the thread's next turn.
    let mut second = server.connect(2).await;
    let response = server
        .resume(&mut second, ThreadResumeParams::new(thread.id()))
        .await;
    assert_eq!(response.thread().turns().len(), 1);
    script.answer("Again");
    server.turn_start(&mut primary, thread.id(), "more").await;
    second.notification("turn/completed").await;
}

/// Adapted from Codex's `thread_resume_replays_pending_command_execution_request_approval`: a
/// connection attaching to a thread is sent its requests still waiting, after the answer. Nothing
/// in the server raises such a request yet, so the case raises it itself.
#[tokio::test]
async fn thread_resume_replays_pending_server_requests() {
    let script = Arc::new(Script::default());
    let store: Arc<dyn ThreadStore> = Arc::new(InMemoryThreadStore::new());
    let (server, requester) = build(
        Options {
            store: Some(Arc::clone(&store)),
            factory: Some(Factory::new(&script)),
            unload_delay: None,
        },
        Some("stored"),
    );
    let requester = requester.unwrap();
    server.store_thread("stored", "seed").await;
    let mut primary = server.connect(1).await;
    server
        .resume(&mut primary, ThreadResumeParams::new("stored"))
        .await;

    let (request_id, _answer) = requester
        .send_request(ServerRequestPayload::CommandExecutionRequestApproval(
            CommandExecutionRequestApprovalParams::new("stored", "turn-1", "call-1", 0)
                .with_command("cat file"),
        ))
        .await;
    let mut second = server.connect(2).await;
    let id = server
        .request(&mut second, "thread/resume", json!({"threadId": "stored"}))
        .await;
    let response = second.next().await;
    assert_eq!(response["id"], id);
    let replayed = second.next().await;
    assert_eq!(replayed["method"], "item/commandExecution/requestApproval");
    assert_eq!(replayed["id"], serde_json::to_value(&request_id).unwrap());
}

/// Ported from Codex's `thread_resume_restores_persisted_approval_policy`,
/// `thread_resume_approval_policy_override_wins_over_persisted_policy` and
/// `cold_resume_reresolves_persisted_active_permission_profile`: the settings a thread was started
/// with are recorded with its turn and restored when it is loaded again, unless the resume sets
/// them.
#[tokio::test]
async fn thread_resume_restores_persisted_policy_unless_overridden() {
    let script = Arc::new(Script::default());
    let store: Arc<dyn ThreadStore> = Arc::new(InMemoryThreadStore::new());
    let (server, _) = build(
        Options {
            store: Some(Arc::clone(&store)),
            factory: Some(Factory::with_adapter(&script)),
            unload_delay: Some(Duration::ZERO),
        },
        None,
    );
    let mut connection = server.connect(1).await;
    let result = server
        .call(
            &mut connection,
            "thread/start",
            json!({"approvalPolicy": "never", "permissions": ":read-only"}),
        )
        .await;
    let thread_id = result["thread"]["id"].as_str().unwrap().to_owned();
    script.answer("Done");
    server.run_turn(&mut connection, &thread_id, "first").await;
    unload(&server, &mut connection, &thread_id).await;

    let response = server
        .resume(&mut connection, ThreadResumeParams::new(&thread_id))
        .await;
    assert_eq!(response.settings()["approvalPolicy"], "never");
    assert_eq!(
        response.settings()["activePermissionProfile"]["id"],
        ":read-only"
    );
    assert_eq!(
        response.settings()["sandbox"],
        json!({"type": "readOnly", "networkAccess": false})
    );
    unload(&server, &mut connection, &thread_id).await;

    let result = server
        .call(
            &mut connection,
            "thread/resume",
            json!({"threadId": thread_id, "approvalPolicy": "on-request", "sandbox": "danger-full-access"}),
        )
        .await;
    assert_eq!(result["approvalPolicy"], "on-request");
    assert_eq!(result["sandbox"], json!({"type": "dangerFullAccess"}));
    assert_eq!(result["activePermissionProfile"], Value::Null);
}

/// Unsubscribes `connection` from the thread and waits for the thread to be unloaded, which every
/// connection hears as the thread no longer being loaded, then closed.
async fn unload(server: &Server, connection: &mut Connection, thread_id: &str) {
    server
        .call(
            connection,
            "thread/unsubscribe",
            json!({"threadId": thread_id}),
        )
        .await;
    let closed = connection.notification("thread/closed").await;
    assert_eq!(closed["threadId"], thread_id);
    let before: Vec<Value> = connection.passed.drain(..).collect();
    assert_eq!(
        statuses_of(&before, thread_id).last(),
        Some(&ThreadStatus::NotLoaded)
    );
}

/// Ported from Codex's reload of an idle loaded thread whose resume asks for other settings: with
/// nobody else subscribed it is loaded again with them; while someone is, they are ignored.
#[tokio::test]
async fn thread_resume_reloads_an_unwatched_idle_thread_for_new_settings() {
    let directory = tempfile::tempdir().unwrap();
    let script = Arc::new(Script::default());
    let factory = Factory::new(&script);
    let (server, _) = build(
        Options {
            store: Some(Arc::new(RolloutThreadDirectory::new(directory.path()))),
            factory: Some(factory.clone()),
            unload_delay: None,
        },
        None,
    );
    let mut connection = server.connect(1).await;
    let thread = server
        .start_thread(&mut connection, ThreadStartParams::new())
        .await;
    script.answer("Done");
    server.run_turn(&mut connection, thread.id(), "first").await;

    // Subscribed: the override is ignored.
    let response = server
        .resume(
            &mut connection,
            ThreadResumeParams::new(thread.id()).with_model("other-model"),
        )
        .await;
    assert_eq!(response.settings()["model"], "mock-model");
    assert_eq!(factory.origins(), vec![ThreadOrigin::Start]);

    server
        .call(
            &mut connection,
            "thread/unsubscribe",
            json!({"threadId": thread.id()}),
        )
        .await;
    let response = server
        .resume(
            &mut connection,
            ThreadResumeParams::new(thread.id()).with_model("other-model"),
        )
        .await;
    assert_eq!(response.settings()["model"], "other-model");
    assert_eq!(response.thread().turns().len(), 1);
    assert_eq!(
        factory.origins(),
        vec![ThreadOrigin::Start, ThreadOrigin::Resume]
    );
}

// ---------------------------------------------------------------------------------------------
// thread/fork
// ---------------------------------------------------------------------------------------------

/// Ported from Codex's `thread_fork_creates_new_thread_and_emits_started` and
/// `thread_read_returns_forked_from_id_for_forked_threads`: the fork is a new thread of its own id
/// on the source's history, which is left as it was, and every connection hears of it without its
/// turns.
#[tokio::test]
async fn thread_fork_creates_new_thread_and_emits_started() {
    let script = Arc::new(Script::default());
    let factory = Factory::new(&script);
    let server = Server::start(factory.clone());
    let mut connection = server.connect(1).await;
    server.store_thread("source", "Saved user message").await;
    let before = server
        .store
        .load_history(&ra_session::store::LoadThreadHistoryParams::new(
            SessionId::new("source"),
        ))
        .await
        .unwrap()
        .into_records();

    let id = server
        .request(
            &mut connection,
            "thread/fork",
            serde_json::to_value(ThreadForkParams::new("source").with_thread_source("user"))
                .unwrap(),
        )
        .await;
    let result = connection.response(id).await;
    assert_eq!(result["thread"]["name"], Value::Null);
    assert_eq!(result.get("sessionId"), None);
    let response: ThreadForkResponse = serde_json::from_value(result).unwrap();
    let thread = response.thread().clone();
    assert_ne!(thread.id(), "source");
    assert_eq!(thread.session_id(), thread.id());
    assert_eq!(thread.forked_from_id(), Some("source"));
    assert_eq!(thread.preview(), "Saved user message");
    assert_eq!(thread.status(), &ThreadStatus::Idle);
    assert_eq!(thread.thread_source(), Some("user"));
    assert_eq!(thread.turns().len(), 1);
    assert_eq!(user_texts(&thread.turns()[0]), ["Saved user message"]);
    assert_eq!(
        factory.origins(),
        vec![ThreadOrigin::Fork {
            source: SessionId::new("source")
        }]
    );
    let after = server
        .store
        .load_history(&ra_session::store::LoadThreadHistoryParams::new(
            SessionId::new("source"),
        ))
        .await
        .unwrap()
        .into_records();
    assert_eq!(before, after, "the source is left as it was");

    let started = connection
        .until(|message| message["method"] == "thread/started")
        .await;
    assert!(
        statuses_of(
            &connection.passed.iter().cloned().collect::<Vec<_>>(),
            thread.id()
        )
        .is_empty()
    );
    assert_eq!(started["params"]["thread"]["turns"], json!([]));
    let started: Thread = serde_json::from_value(started["params"]["thread"].clone()).unwrap();
    assert_eq!(started.id(), thread.id());

    let read = server
        .read(&mut connection, ThreadReadParams::new(thread.id()))
        .await;
    assert_eq!(read.forked_from_id(), Some("source"));
    assert_eq!(read.thread_source(), Some("user"));

    // The fork runs on its own copy of the history.
    script.answer("Forked");
    server.run_turn(&mut connection, thread.id(), "next").await;
    let texts: Vec<_> = script
        .last_request()
        .input()
        .iter()
        .filter_map(|item| match item {
            ModelInputItem::Message(message) => Some(message.text_content()),
            _ => None,
        })
        .collect();
    assert_eq!(texts, ["Saved user message", "Done", "next"]);
}

/// Ported from Codex's `thread_fork_at_last_turn_id_keeps_only_terminal_prefix`, with
/// `beforeTurnId` alongside: the fork keeps the turns up to and including the one named, or those
/// before it.
#[tokio::test]
async fn thread_fork_at_named_boundaries_keeps_only_the_prefix() {
    let script = Arc::new(Script::default());
    let server = Server::start(Factory::new(&script));
    let mut connection = server.connect(1).await;
    let thread = server
        .start_thread(&mut connection, ThreadStartParams::new())
        .await;
    let mut turn_ids = Vec::new();
    for text in ["first", "second", "third"] {
        script.answer("Done");
        turn_ids.push(server.run_turn(&mut connection, thread.id(), text).await);
    }

    let fork = server
        .fork(
            &mut connection,
            ThreadForkParams::new(thread.id()).through_turn(&turn_ids[1]),
        )
        .await;
    let ids: Vec<_> = fork.thread().turns().iter().map(Turn::id).collect();
    assert_eq!(ids, turn_ids[..2]);

    let fork = server
        .fork(
            &mut connection,
            ThreadForkParams::new(thread.id()).before_turn(&turn_ids[1]),
        )
        .await;
    let ids: Vec<_> = fork.thread().turns().iter().map(Turn::id).collect();
    assert_eq!(ids, turn_ids[..1]);
}

/// A turn is a run here, and a run continued after an approval pause starts again under its id:
/// forking through it keeps all of its segments, not just the first, where Codex's turns have one
/// start each.
#[tokio::test]
async fn thread_fork_through_a_turn_keeps_its_continued_segments() {
    let script = Arc::new(Script::default());
    let server = Server::start(Factory::new(&script));
    let mut connection = server.connect(1).await;
    let recorder = server
        .store
        .create_thread_with(&CreateThreadParams::new(RolloutSessionMeta::new(
            SessionId::new("source"),
        )))
        .await
        .unwrap();
    let mut records = vec![
        run_started("turn-1", "approve this"),
        run_ended("turn-1", RolloutRunEnd::Interrupted),
        RolloutItem::RunStarted(RolloutRunStarted::new(
            RunId::new("turn-1"),
            AgentId::new("lead"),
        )),
        agent_record("turn-1-answer", "Approved and done"),
        run_ended("turn-1", RolloutRunEnd::Completed),
    ];
    records.extend(completed_run("turn-2", "second", "Done"));
    record(&recorder, records).await;
    drop(recorder);

    let fork = server
        .fork(
            &mut connection,
            ThreadForkParams::new("source").through_turn("turn-1"),
        )
        .await;
    let turns = fork.thread().turns();
    assert_eq!(turns.len(), 1);
    assert_eq!(turns[0].id(), "turn-1");
    assert_eq!(turns[0].status(), TurnStatus::Completed);
    let answers: Vec<_> = turns[0]
        .items()
        .iter()
        .filter_map(|item| match item {
            ThreadItem::AgentMessage { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(answers, ["Approved and done"]);
}

/// Ported from Codex's `thread_fork_can_cut_before_unfinished_stored_turn`.
#[tokio::test]
async fn thread_fork_can_cut_before_unfinished_stored_turn() {
    let script = Arc::new(Script::default());
    let server = Server::start(Factory::new(&script));
    let mut connection = server.connect(1).await;
    let recorder = server.store_thread("source", "Saved user message").await;
    record(
        &recorder,
        [run_started("unfinished-turn", "Unfinished user message")],
    )
    .await;
    drop(recorder);

    let source = server
        .read(
            &mut connection,
            ThreadReadParams::new("source").including_turns(),
        )
        .await;
    assert_eq!(source.turns().len(), 2);
    assert_eq!(source.turns()[1].id(), "unfinished-turn");
    assert_eq!(source.turns()[1].status(), TurnStatus::Interrupted);

    let fork = server
        .fork(
            &mut connection,
            ThreadForkParams::new("source").before_turn("unfinished-turn"),
        )
        .await;
    assert_eq!(fork.thread().turns().len(), 1);
    assert_eq!(fork.thread().preview(), "Saved user message");

    // Forked as it stands, the unfinished turn is ended as interrupted in the copy.
    let fork = server
        .fork(&mut connection, ThreadForkParams::new("source"))
        .await;
    assert_eq!(fork.thread().turns().len(), 2);
    assert_eq!(fork.thread().turns()[1].status(), TurnStatus::Interrupted);
}

/// Ported from Codex's `thread_fork_rejects_incompatible_boundaries_and_ephemeral_goal_deferral`,
/// `thread_fork_rejects_unmaterialized_thread` and the boundary refusals of Codex's
/// `truncate_rollout_after_turn_id` and `truncate_rollout_before_turn_id`.
#[tokio::test]
async fn thread_fork_refusals() {
    let script = Arc::new(Script::default());
    let server = Server::start(Factory::new(&script));
    let mut connection = server.connect(1).await;
    let recorder = server.store_thread("source", "Saved user message").await;
    record(&recorder, [run_started("unfinished-turn", "Unfinished")]).await;
    drop(recorder);

    let cases = [
        (
            json!({"threadId": "source", "lastTurnId": "turn-1", "beforeTurnId": "turn-2"}),
            invalid_request("`beforeTurnId` cannot be combined with `lastTurnId`"),
        ),
        (
            json!({"threadId": "source", "deferGoalContinuation": true}),
            invalid_request("thread/fork.deferGoalContinuation is not supported"),
        ),
        (
            json!({"threadId": "missing"}),
            invalid_request("no rollout found for thread id missing"),
        ),
        (
            json!({"threadId": "source", "lastTurnId": "nope"}),
            invalid_request("lastTurnId 'nope' was not found in the source thread"),
        ),
        (
            json!({"threadId": "source", "lastTurnId": "unfinished-turn"}),
            invalid_request("lastTurnId 'unfinished-turn' identifies an in-progress turn"),
        ),
        (
            json!({"threadId": "source", "beforeTurnId": "nope"}),
            invalid_request("beforeTurnId 'nope' was not found in the source thread"),
        ),
    ];
    for (params, error) in cases {
        assert_eq!(
            server
                .call_error(&mut connection, "thread/fork", params)
                .await,
            error
        );
    }
}

/// Ported from Codex's `thread_fork_ephemeral_remains_pathless_and_omits_listing`: an ephemeral
/// fork keeps its copy in memory, with the source's preview and turns.
#[tokio::test]
async fn thread_fork_ephemeral_remains_pathless() {
    let script = Arc::new(Script::default());
    let server = Server::start(Factory::new(&script));
    let mut connection = server.connect(1).await;
    server.store_thread("source", "Saved user message").await;

    let fork = server
        .fork(&mut connection, ThreadForkParams::new("source").ephemeral())
        .await;
    let thread = fork.thread();
    assert!(thread.ephemeral());
    assert_eq!(thread.path(), None);
    assert_eq!(thread.preview(), "Saved user message");
    assert_eq!(thread.forked_from_id(), Some("source"));
    assert_eq!(thread.turns().len(), 1);
    assert!(
        server
            .store
            .read_thread(&ReadThreadParams::new(SessionId::new(thread.id())))
            .await
            .is_err()
    );
    script.answer("Done");
    server.run_turn(&mut connection, thread.id(), "next").await;
}

/// Ported from Codex's `thread_fork_inherits_explicit_source_name_from_session_index`.
#[tokio::test]
async fn thread_fork_inherits_the_source_name() {
    let script = Arc::new(Script::default());
    let server = Server::start(Factory::new(&script));
    let mut connection = server.connect(1).await;
    server.store_thread("source", "Saved user message").await;
    server
        .store
        .update_thread_metadata(&UpdateThreadMetadataParams::new(
            SessionId::new("source"),
            ThreadMetadataPatch::new().with_name(Some("Named source".to_owned())),
        ))
        .await
        .unwrap();

    let fork = server
        .fork(&mut connection, ThreadForkParams::new("source"))
        .await;
    assert_eq!(fork.thread().name(), Some("Named source"));
    let read = server
        .read(&mut connection, ThreadReadParams::new(fork.thread().id()))
        .await;
    assert_eq!(read.name(), Some("Named source"));
}

// ---------------------------------------------------------------------------------------------
// The coding product's factory
// ---------------------------------------------------------------------------------------------

/// The coding product starts its threads through the server: the coding agent in the host's
/// workspace, with the coding settings adapter's policy and working directory in the answer.
#[tokio::test]
async fn coding_thread_factory_starts_coding_threads() {
    let workspace = tempfile::tempdir().unwrap();
    let root = workspace.path().canonicalize().unwrap();
    let host = CodingHost::open(&root).unwrap();
    let script = Arc::new(Script::default());
    let settings = CodingThreadSettingsAdapter::new(
        &root,
        SandboxPolicy::ReadOnly {
            network_access: false,
        },
    )
    .unwrap();
    let factory = CodingThreadFactory::new(host, Arc::new(Resolver(Arc::clone(&script))), settings);
    let server = Server::start(Arc::new(factory));
    let mut connection = server.connect(1).await;

    let result = server
        .call(&mut connection, "thread/start", json!({}))
        .await;
    assert_eq!(result["cwd"], root.to_str().unwrap());
    assert_eq!(
        result["sandbox"],
        json!({"type": "readOnly", "networkAccess": false})
    );
    assert_eq!(result["approvalPolicy"], "on-request");
    let thread_id = result["thread"]["id"].as_str().unwrap().to_owned();
    script.answer("Done");
    server.run_turn(&mut connection, &thread_id, "hello").await;
    let request = script.last_request();
    assert!(
        request
            .tools()
            .iter()
            .any(|tool| tool.name() == "exec_command"),
        "the coding agent's tools are offered"
    );
}

// ---------------------------------------------------------------------------------------------
// The wire
// ---------------------------------------------------------------------------------------------

/// The statuses, sandbox modes and parameter readings as Codex's generated schema has them: a
/// tagged status, kebab-case sandbox modes, an empty path read as none, and experimental members
/// reported in Codex's declaration order.
#[test]
fn thread_payloads_have_codex_wire_shape() {
    assert_eq!(
        serde_json::to_value(ThreadStatus::NotLoaded).unwrap(),
        json!({"type": "notLoaded"})
    );
    assert_eq!(
        serde_json::to_value(ThreadStatus::SystemError).unwrap(),
        json!({"type": "systemError"})
    );
    assert_eq!(
        serde_json::to_value(ThreadStatus::active()).unwrap(),
        json!({"type": "active", "activeFlags": []})
    );
    let waiting: ThreadStatus =
        serde_json::from_value(json!({"type": "active", "activeFlags": ["waitingOnApproval"]}))
            .unwrap();
    assert_ne!(waiting, ThreadStatus::active());

    let resume: ThreadResumeParams =
        serde_json::from_value(json!({"threadId": "t", "path": ""})).unwrap();
    assert_eq!(resume.experimental_reason(), None);
    assert_eq!(resume.unsupported_field(), None);
    let resume: ThreadResumeParams =
        serde_json::from_value(json!({"threadId": "t", "path": "/x", "initialTurnsPage": {}}))
            .unwrap();
    assert_eq!(resume.experimental_reason(), Some("thread/resume.path"));
    assert_eq!(resume.unsupported_field(), Some("path"));

    let start: ThreadStartParams = serde_json::from_value(json!({
        "sandbox": "workspace-write",
        "approvalPolicy": {"granular": {"sandbox_approval": true, "rules": true, "mcp_elicitations": false}},
        "historyMode": "legacy",
    }))
    .unwrap();
    assert_eq!(start.experimental_reason(), Some("askForApproval.granular"));
    assert_eq!(start.unsupported_field(), None);
    let start: ThreadStartParams =
        serde_json::from_value(json!({"modelProvider": "openai", "config": {}})).unwrap();
    assert_eq!(start.experimental_reason(), None);
    assert_eq!(start.unsupported_field(), Some("modelProvider"));

    let fork: ThreadForkParams =
        serde_json::from_value(json!({"threadId": "t", "beforeTurnId": "x", "ephemeral": true}))
            .unwrap();
    assert_eq!(fork.experimental_reason(), Some("thread/fork.beforeTurnId"));
    assert_eq!(
        serde_json::to_value(ThreadReadParams::new("t").including_turns()).unwrap(),
        json!({"threadId": "t", "includeTurns": true})
    );
}

/// Codex restores the latest persisted policy independently of a fork's history boundary.
#[tokio::test]
async fn thread_fork_at_old_turn_inherits_latest_policy_and_honors_overrides() {
    let script = Arc::new(Script::default());
    let server = Server::start(Factory::with_adapter(&script));
    let mut connection = server.connect(1).await;
    let thread = server
        .start_thread(&mut connection, ThreadStartParams::new())
        .await;
    script.answer("one");
    let first = server.run_turn(&mut connection, thread.id(), "one").await;
    script.answer("two");
    let second = server.run_turn(&mut connection, thread.id(), "two").await;
    server
        .call(
            &mut connection,
            "thread/settings/update",
            json!({"threadId": thread.id(), "approvalPolicy": "never", "permissions": ":read-only"}),
        )
        .await;
    for boundary in [
        json!({"lastTurnId": first}),
        json!({"beforeTurnId": second}),
    ] {
        let mut params = boundary.as_object().unwrap().clone();
        params.insert("threadId".into(), json!(thread.id()));
        let result = server
            .call(
                &mut connection,
                "thread/fork",
                Value::Object(params.clone()),
            )
            .await;
        assert_eq!(result["approvalPolicy"], "never");
        assert_eq!(result["activePermissionProfile"]["id"], ":read-only");
        assert_eq!(result["approvalsReviewer"], "user");
        assert_eq!(result["thread"]["turns"].as_array().unwrap().len(), 1);
        params.insert("approvalPolicy".into(), json!("on-request"));
        params.insert("sandbox".into(), json!("danger-full-access"));
        let overridden = server
            .call(&mut connection, "thread/fork", Value::Object(params))
            .await;
        assert_eq!(overridden["approvalPolicy"], "on-request");
        assert_eq!(overridden["activePermissionProfile"], Value::Null);
    }
}

/// Archive materializes even a fresh thread, releases its live writer, replies, then broadcasts.
#[tokio::test]
async fn thread_management_archives_unarchives_and_deletes_persistent_threads() {
    let dir = tempfile::tempdir().unwrap();
    let script = Arc::new(Script::default());
    let (server, _) = build(
        Options {
            store: Some(Arc::new(RolloutThreadDirectory::new(dir.path()))),
            factory: Some(Factory::new(&script)),
            unload_delay: None,
        },
        None,
    );
    let mut connection = server.connect(1).await;
    let mut observer = server.connect(2).await;
    let thread = server
        .start_thread(&mut connection, ThreadStartParams::new())
        .await;
    let id = thread.id();
    assert_eq!(
        server
            .call(&mut connection, "thread/archive", json!({"threadId": id}))
            .await,
        json!({})
    );
    assert_eq!(
        observer.notification("thread/archived").await["threadId"],
        id
    );
    assert_eq!(
        server
            .call(&mut connection, "thread/loaded/list", json!({}))
            .await["data"],
        json!([])
    );
    let archived = server
        .call(&mut connection, "thread/list", json!({"archived": true}))
        .await;
    assert_eq!(archived["data"][0]["id"], id);
    assert_eq!(archived["data"][0]["status"], json!({"type": "notLoaded"}));
    let error = server
        .call_error(&mut connection, "thread/resume", json!({"threadId": id}))
        .await;
    assert!(error["message"].as_str().unwrap().contains("archived"));
    let unarchived = server
        .call(&mut connection, "thread/unarchive", json!({"threadId": id}))
        .await;
    assert_eq!(unarchived["thread"]["id"], id);
    assert_eq!(
        observer.notification("thread/unarchived").await["threadId"],
        id
    );
    server
        .resume(&mut connection, ThreadResumeParams::new(id))
        .await;
    script.answer("answer");
    server.run_turn(&mut connection, id, "hello").await;
    assert_eq!(
        server
            .call(&mut connection, "thread/delete", json!({"threadId": id}))
            .await,
        json!({})
    );
    assert_eq!(
        observer.notification("thread/deleted").await["threadId"],
        id
    );
    assert!(
        server.call(&mut connection, "thread/list", json!({})).await["data"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let error = server
        .call_error(&mut connection, "thread/delete", json!({"threadId": id}))
        .await;
    assert_eq!(error["code"], -32600);
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .contains("thread not found")
    );
}

/// Explicit metadata mutations work before the first turn, preserve omitted fields and sanitize
/// credentials before any response or persistent record. Canonical markers survive reopening.
#[tokio::test]
async fn thread_metadata_updates_git_and_name_for_live_cold_and_archived_threads() {
    let dir = tempfile::tempdir().unwrap();
    let script = Arc::new(Script::default());
    let state = ra_session::store::local::try_init(dir.path())
        .await
        .unwrap();
    let store: Arc<dyn ThreadStore> =
        Arc::new(RolloutThreadDirectory::new(dir.path()).with_state_db(state));
    let (server, _) = build(
        Options {
            store: Some(Arc::clone(&store)),
            factory: Some(Factory::new(&script)),
            unload_delay: None,
        },
        None,
    );
    let mut connection = server.connect(1).await;
    let mut observer = server.connect(2).await;
    let thread = server
        .start_thread(&mut connection, ThreadStartParams::new())
        .await;
    let id = thread.id();
    assert_eq!(
        server
            .call(
                &mut connection,
                "thread/name/set",
                json!({"threadId": id, "name": "  New name  "})
            )
            .await,
        json!({})
    );
    assert_eq!(
        observer.notification("thread/name/updated").await,
        json!({"threadId": id, "threadName": "New name"})
    );
    let result = server.call(&mut connection, "thread/metadata/update", json!({"threadId": id,
        "gitInfo": {"sha": " abc ", "branch": " main ", "originUrl": "https://user:synthetic-secret@example.invalid/repo.git"}
    })).await;
    let expected =
        json!({"sha": "abc", "branch": "main", "originUrl": "https://example.invalid/repo.git"});
    assert_eq!(result["thread"]["gitInfo"], expected);
    assert_eq!(result["thread"]["name"], "New name");
    assert_eq!(result["thread"]["status"], json!({"type": "idle"}));
    script.answer("answer");
    server.run_turn(&mut connection, id, "first message").await;
    let listed = server
        .call(
            &mut connection,
            "thread/list",
            json!({"useStateDbOnly": true}),
        )
        .await;
    assert_eq!(listed["data"][0]["gitInfo"], expected);
    server
        .call(&mut connection, "thread/archive", json!({"threadId": id}))
        .await;
    let result = server
        .call(
            &mut connection,
            "thread/metadata/update",
            json!({"threadId": id, "gitInfo": {"branch": "feature"}}),
        )
        .await;
    assert_eq!(
        result["thread"]["gitInfo"],
        json!({"sha": "abc", "branch": "feature", "originUrl": "https://example.invalid/repo.git"})
    );
    let path = std::path::PathBuf::from(result["thread"]["path"].as_str().unwrap());
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(!text.contains("synthetic-secret"));
    assert!(text.contains("https://example.invalid/repo.git"));
    // Reopening without the database exercises canonical SessionMeta replay directly.
    let reopened = RolloutThreadDirectory::new(dir.path());
    let read = reopened
        .read_thread(&ReadThreadParams::new(SessionId::new(id)).including_archived())
        .await
        .unwrap();
    assert_eq!(read.git_info().unwrap().branch.as_deref(), Some("feature"));
    let cleared = server
        .call(
            &mut connection,
            "thread/metadata/update",
            json!({"threadId": id,
                "gitInfo": {"sha": null, "branch": null, "originUrl": null}
            }),
        )
        .await;
    assert_eq!(cleared["thread"]["gitInfo"], Value::Null);
    let read = server
        .read(&mut connection, ThreadReadParams::new(id))
        .await;
    assert_eq!(serde_json::to_value(read).unwrap()["gitInfo"], Value::Null);
    assert_eq!(
        server
            .call(&mut connection, "thread/list", json!({"archived": true}))
            .await["data"][0]["gitInfo"],
        Value::Null
    );
}

/// The local listing ports sorting, provider/cwd/title filters, both cursors and limits.
#[tokio::test]
async fn thread_list_pages_persisted_threads_and_filters_without_loading_them() {
    let dir = tempfile::tempdir().unwrap();
    let script = Arc::new(Script::default());
    let store: Arc<dyn ThreadStore> = Arc::new(RolloutThreadDirectory::new(dir.path()));
    for (id, created, provider, cwd, name) in [
        ("a", 1000, "one", "/work/a", "Alpha"),
        ("b", 1000, "two", "/work/b", "Beta"),
        ("c", 2000, "one", "/work/a", "Gamma"),
    ] {
        let recorder = store
            .create_thread_with(&CreateThreadParams::new(
                RolloutSessionMeta::new(SessionId::new(id))
                    .with_created_at(EventTimestamp::from_millis(created))
                    .with_model_provider(provider)
                    .with_cwd(cwd),
            ))
            .await
            .unwrap();
        record(&recorder, completed_run(id, name, "Done")).await;
        recorder.shutdown().await.unwrap();
        store
            .update_thread_metadata(&UpdateThreadMetadataParams::new(
                SessionId::new(id),
                ThreadMetadataPatch::new().with_name(Some(name.into())),
            ))
            .await
            .unwrap();
    }
    let (server, _) = build(
        Options {
            store: Some(store),
            factory: Some(Factory::new(&script)),
            unload_delay: None,
        },
        None,
    );
    let mut connection = server.connect(1).await;
    let first = server
        .call(&mut connection, "thread/list", json!({"limit": 2}))
        .await;
    assert_eq!(
        first["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["c", "b"]
    );
    assert_eq!(first["data"][0]["turns"], json!([]));
    let second = server
        .call(
            &mut connection,
            "thread/list",
            json!({"limit": 2, "cursor": first["nextCursor"]}),
        )
        .await;
    assert_eq!(second["data"][0]["id"], "a");
    assert_eq!(second["nextCursor"], Value::Null);
    let reverse = server
        .call(
            &mut connection,
            "thread/list",
            json!({"limit": 2, "sortDirection": "asc", "cursor": second["backwardsCursor"]}),
        )
        .await;
    assert_eq!(
        reverse["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["b", "c"]
    );
    for (params, expected) in [
        (json!({"modelProviders": ["one"]}), vec!["c", "a"]),
        (json!({"cwd": ["/work/a"]}), vec!["c", "a"]),
        (json!({"searchTerm": "Beta"}), vec!["b"]),
        (json!({"cwd": []}), vec![]),
        (json!({"modelProviders": []}), vec!["c", "b", "a"]),
        (json!({"sortDirection": "asc", "limit": 0}), vec!["a"]),
    ] {
        let result = server.call(&mut connection, "thread/list", params).await;
        assert_eq!(
            result["data"]
                .as_array()
                .unwrap()
                .iter()
                .map(|t| t["id"].as_str().unwrap())
                .collect::<Vec<_>>(),
            expected
        );
    }
    assert_eq!(
        server
            .call(&mut connection, "thread/loaded/list", json!({}))
            .await["data"],
        json!([])
    );
}

/// Validation ports empty names, metadata patches and hosted-only originator filters; ephemeral
/// threads remain unavailable to persistent metadata and deletion operations.
#[tokio::test]
async fn thread_management_rejects_invalid_mutations_and_gates_experimental_fields() {
    let script = Arc::new(Script::default());
    let server = Server::start(Factory::new(&script));
    let mut connection = server.connect(1).await;
    let ephemeral = server
        .start_thread(
            &mut connection,
            ThreadStartParams::new().with_ephemeral(true),
        )
        .await;
    for (method, params, message) in [
        (
            "thread/name/set",
            json!({"threadId": "none", "name": "  "}),
            "thread name must not be empty",
        ),
        (
            "thread/metadata/update",
            json!({"threadId": "none"}),
            "thread metadata update must include at least one field",
        ),
        (
            "thread/metadata/update",
            json!({"threadId": "none", "gitInfo": {}}),
            "gitInfo must include at least one field",
        ),
        (
            "thread/metadata/update",
            json!({"threadId": "none", "gitInfo": {"branch": " "}}),
            "gitInfo.branch must not be empty",
        ),
    ] {
        assert_eq!(
            server.call_error(&mut connection, method, params).await,
            invalid_request(message)
        );
    }
    let error = server
        .call_error(
            &mut connection,
            "thread/delete",
            json!({"threadId": ephemeral.id()}),
        )
        .await;
    assert_eq!(error["code"], -32600);
    let error = server
        .call_error(
            &mut connection,
            "thread/metadata/update",
            json!({"threadId": ephemeral.id(), "gitInfo": {"branch": "main"}}),
        )
        .await;
    assert!(error["message"].as_str().unwrap().contains("ephemeral"));
    let error = server
        .call_error(
            &mut connection,
            "thread/list",
            json!({"originators": ["client"]}),
        )
        .await;
    assert_eq!(error["code"], -32602);
    let error = server
        .call_error(&mut connection, "thread/list", json!({"cursor": "invalid"}))
        .await;
    assert_eq!(error["code"], -32600);
    let mut stable = server.connect_with(2, InitializeCapabilities::new()).await;
    let error = server
        .call_error(
            &mut stable,
            "thread/metadata/update",
            json!({"threadId": "none", "projectId": "project"}),
        )
        .await;
    assert_eq!(error["code"], -32600);
    assert!(error["message"].as_str().unwrap().contains("experimental"));
}

/// Archive stops a live turn and includes previewless spawned descendants. Deletion walks the
/// entire subtree even when descendants have already been archived, as Codex's lifecycle does.
#[tokio::test]
async fn thread_archive_interrupts_live_turn_and_delete_removes_spawned_subtree() {
    use ra_core::{agent::control::AgentPath, session::rollout::RolloutThreadSpawn};
    let dir = tempfile::tempdir().unwrap();
    let script = Arc::new(Script::default());
    let store: Arc<dyn ThreadStore> = Arc::new(RolloutThreadDirectory::new(dir.path()));
    let (server, _) = build(
        Options {
            store: Some(Arc::clone(&store)),
            factory: Some(Factory::new(&script)),
            unload_delay: None,
        },
        None,
    );
    let mut connection = server.connect(1).await;
    let mut observer = server.connect(2).await;
    let root = server
        .start_thread(&mut connection, ThreadStartParams::new())
        .await;
    for (id, parent, depth) in [("child", root.id(), 1), ("grandchild", "child", 2)] {
        let recorder = store
            .create_thread_with(&CreateThreadParams::new(
                RolloutSessionMeta::new(SessionId::new(id)).with_thread_spawn(
                    RolloutThreadSpawn::new(
                        SessionId::new(root.id()),
                        SessionId::new(parent),
                        depth,
                        AgentPath::root().join(id).unwrap(),
                    ),
                ),
            ))
            .await
            .unwrap();
        recorder
            .persist(ra_core::session::rollout::PersistContext::Standard)
            .await
            .unwrap();
        recorder.shutdown().await.unwrap();
    }
    script.push(Step::After(Arc::new(Notify::new()), final_message("late")));
    server.turn_start(&mut connection, root.id(), "wait").await;
    script.wait_for_calls(1).await;
    server
        .call(
            &mut connection,
            "thread/archive",
            json!({"threadId": root.id()}),
        )
        .await;
    let mut archived = Vec::new();
    for _ in 0..3 {
        archived.push(
            observer.notification("thread/archived").await["threadId"]
                .as_str()
                .unwrap()
                .to_owned(),
        );
    }
    assert_eq!(archived, [root.id(), "grandchild", "child"]);
    let read = server
        .read(
            &mut connection,
            ThreadReadParams::new(root.id()).including_turns(),
        )
        .await;
    assert_eq!(read.turns()[0].status(), TurnStatus::Interrupted);
    server
        .call(
            &mut connection,
            "thread/unarchive",
            json!({"threadId": root.id()}),
        )
        .await;
    server
        .call(
            &mut connection,
            "thread/delete",
            json!({"threadId": root.id()}),
        )
        .await;
    let mut deleted = Vec::new();
    for _ in 0..3 {
        deleted.push(
            observer.notification("thread/deleted").await["threadId"]
                .as_str()
                .unwrap()
                .to_owned(),
        );
    }
    assert_eq!(deleted, ["grandchild", "child", root.id()]);
    assert!(
        server
            .call(&mut connection, "thread/list", json!({"archived": true}))
            .await["data"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

/// Source and spawn relation filters retain every matching thread across storage pages.
#[tokio::test]
async fn thread_list_source_and_relation_filters_do_not_skip_page_matches() {
    use ra_core::{agent::control::AgentPath, session::rollout::RolloutThreadSpawn};
    let directory = tempfile::tempdir().unwrap();
    let store: Arc<dyn ThreadStore> = Arc::new(RolloutThreadDirectory::new(directory.path()));
    for (id, created, parent) in [
        ("root", 100, None),
        ("first", 200, Some("root")),
        ("other", 300, None),
        ("second", 400, Some("root")),
        ("third", 500, Some("second")),
    ] {
        let mut meta = RolloutSessionMeta::new(SessionId::new(id))
            .with_created_at(EventTimestamp::from_millis(created));
        if let Some(parent) = parent {
            meta = meta.with_thread_spawn(RolloutThreadSpawn::new(
                SessionId::new("root"),
                SessionId::new(parent),
                if parent == "root" { 1 } else { 2 },
                AgentPath::root().join(id).unwrap(),
            ));
        }
        let recorder = store
            .create_thread_with(&CreateThreadParams::new(meta))
            .await
            .unwrap();
        record(&recorder, completed_run(id, id, "Done")).await;
        recorder.shutdown().await.unwrap();
    }
    let (server, _) = build(
        Options {
            store: Some(store),
            factory: None,
            unload_delay: None,
        },
        None,
    );
    let mut connection = server.connect(1).await;
    let first = server
        .call(
            &mut connection,
            "thread/list",
            json!({"limit": 2, "sourceKinds": ["subAgent"]}),
        )
        .await;
    assert_eq!(first["data"][0]["id"], "third");
    assert_eq!(first["data"][1]["id"], "second");
    let second = server
        .call(
            &mut connection,
            "thread/list",
            json!({"limit": 2, "sourceKinds": ["subAgent"], "cursor": first["nextCursor"]}),
        )
        .await;
    assert_eq!(second["data"].as_array().unwrap().len(), 1);
    assert_eq!(second["data"][0]["id"], "first");
    for (params, expected) in [
        (json!({"parentThreadId": "root"}), vec!["second", "first"]),
        (
            json!({"ancestorThreadId": "root"}),
            vec!["third", "second", "first"],
        ),
        (json!({"sourceKinds": ["unknown"]}), vec!["other", "root"]),
    ] {
        let result = server.call(&mut connection, "thread/list", params).await;
        assert_eq!(
            result["data"]
                .as_array()
                .unwrap()
                .iter()
                .map(|thread| thread["id"].as_str().unwrap())
                .collect::<Vec<_>>(),
            expected
        );
    }
}

/// An independently owned writer is a request conflict and must keep its rollout untouched.
#[tokio::test]
async fn thread_management_reports_external_writer_conflicts_without_mutation() {
    let directory = tempfile::tempdir().unwrap();
    let external = RolloutThreadDirectory::new(directory.path());
    let recorder = external
        .create_thread_with(&CreateThreadParams::new(RolloutSessionMeta::new(
            SessionId::new("external"),
        )))
        .await
        .unwrap();
    record(&recorder, completed_run("turn", "Hello", "Done")).await;
    let store = Arc::new(RolloutThreadDirectory::new(directory.path()));
    let (server, _) = build(
        Options {
            store: Some(store),
            factory: None,
            unload_delay: None,
        },
        None,
    );
    let mut connection = server.connect(1).await;
    for (method, params) in [
        (
            "thread/metadata/update",
            json!({"threadId": "external", "gitInfo": {"branch": "new"}}),
        ),
        ("thread/archive", json!({"threadId": "external"})),
        ("thread/delete", json!({"threadId": "external"})),
    ] {
        let error = server.call_error(&mut connection, method, params).await;
        assert_eq!(error["code"], -32600, "{error}");
    }
    let persisted = external
        .read_thread(&ReadThreadParams::new(SessionId::new("external")))
        .await
        .unwrap();
    assert!(persisted.archived_at().is_none());
    assert!(persisted.git_info().is_none());
    recorder.shutdown().await.unwrap();
}

// ---------------------------------------------------------------------------------------------
// Approvals the server cannot ask for
// ---------------------------------------------------------------------------------------------

/// The automatic reviewer is not ported, so every method that sets the reviewer refuses it, under
/// either of its names, and the thread keeps its client reviewing.
#[tokio::test]
async fn approvals_reviewer_auto_review_is_refused_wherever_it_is_set() {
    let script = Arc::new(Script::default());
    let (server, _) = build(
        Options {
            store: Some(Arc::new(InMemoryThreadStore::new())),
            factory: Some(Factory::with_adapter(&script)),
            unload_delay: Some(Duration::ZERO),
        },
        None,
    );
    let mut connection = server.connect(1).await;
    for name in ["auto_review", "guardian_subagent"] {
        let error = server
            .call_error(
                &mut connection,
                "thread/start",
                json!({"approvalsReviewer": name}),
            )
            .await;
        assert_eq!(
            error["message"],
            "thread/start.approvalsReviewer `auto_review` is not supported"
        );
    }

    let thread = server
        .start_thread(&mut connection, ThreadStartParams::new())
        .await;
    script.answer("Done");
    let turn_id = server.run_turn(&mut connection, thread.id(), "first").await;
    for (method, params) in [
        (
            "thread/settings/update",
            json!({"threadId": thread.id(), "approvalsReviewer": "auto_review"}),
        ),
        (
            "turn/start",
            json!({"threadId": thread.id(), "input": [{"type": "text", "text": "again"}],
                "approvalsReviewer": "auto_review"}),
        ),
        (
            "turn/settings/update",
            json!({"threadId": thread.id(), "turnId": turn_id, "approvalsReviewer": "auto_review"}),
        ),
        (
            "thread/resume",
            json!({"threadId": thread.id(), "approvalsReviewer": "auto_review"}),
        ),
        (
            "thread/fork",
            json!({"threadId": thread.id(), "approvalsReviewer": "auto_review"}),
        ),
    ] {
        let error = server.call_error(&mut connection, method, params).await;
        assert_eq!(
            error["message"],
            format!("{method}.approvalsReviewer `auto_review` is not supported")
        );
    }
    assert_eq!(script.requests.lock().unwrap().len(), 1, "no turn started");

    unload(&server, &mut connection, thread.id()).await;
    let error = server
        .call_error(
            &mut connection,
            "thread/resume",
            json!({"threadId": thread.id(), "approvalsReviewer": "auto_review"}),
        )
        .await;
    assert_eq!(
        error["message"],
        "thread/resume.approvalsReviewer `auto_review` is not supported"
    );
    let resumed = server
        .resume(&mut connection, ThreadResumeParams::new(thread.id()))
        .await;
    assert_eq!(resumed.settings()["approvalsReviewer"], "user");
}

/// A thread recorded under `auto_review` before the reviewer was refused is loaded again with its
/// client reviewing, its other recorded settings restored, rather than refused.
#[tokio::test]
async fn thread_resume_falls_back_from_a_recorded_auto_review() {
    let script = Arc::new(Script::default());
    let server = Server::start(Factory::with_adapter(&script));
    let recorder = server.store_thread("recorded", "hello").await;
    let allocator = RunState::start(RunId::new("settings")).restore_event_seq_allocator(None);
    record(
        &recorder,
        [RolloutItem::Event(
            HostEvent::allocate(
                &allocator,
                AgentId::new("lead"),
                HostEventBody::Unknown {
                    family: "threadSettingsApplied".into(),
                    data: json!({"approvalPolicy": "never", "approvalsReviewer": "auto_review"}),
                },
            )
            .unwrap(),
        )],
    )
    .await;
    let mut connection = server.connect(1).await;

    let resumed = server
        .resume(&mut connection, ThreadResumeParams::new("recorded"))
        .await;
    assert_eq!(resumed.settings()["approvalsReviewer"], "user");
    assert_eq!(resumed.settings()["approvalPolicy"], "never");
}

/// A tool that only declares how it is approved and whether it is enabled.
struct Declared {
    origin: ToolOrigin,
    schema: ToolSchema,
    options: ToolOptions,
}

#[async_trait]
impl Tool for Declared {
    fn origin(&self) -> &ToolOrigin {
        &self.origin
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    fn options(&self) -> ToolOptions {
        self.options.clone()
    }

    async fn call(&self, _context: ToolContext<'_>) -> Result<ToolOutput> {
        Ok(ToolOutput::text("ran"))
    }
}

fn declared(name: &str, approval: ToolApprovalPolicy) -> Arc<dyn Tool> {
    declared_with(name, ToolOptions::new().with_approval(approval))
}

fn declared_with(name: &str, options: ToolOptions) -> Arc<dyn Tool> {
    Arc::new(Declared {
        origin: ToolOrigin::new(name).unwrap(),
        schema: ToolSchema::new(name, json!({"type": "object", "properties": {}, "required": [], "additionalProperties": false})).unwrap(),
        options,
    })
}

fn agent_with(id: &str, tools: Vec<Arc<dyn Tool>>, handoffs: Vec<HandoffSpec>) -> Arc<AgentSpec> {
    AgentSpec::builder()
        .id(AgentId::new(id))
        .name(id)
        .instructions(id)
        .tools(tools)
        .handoffs(handoffs)
        .build()
        .unwrap()
}

/// Contributes one tool to the agents of the runs it is installed on.
struct Contributes(Arc<dyn Tool>);

#[async_trait]
impl Capability for Contributes {
    fn kind(&self) -> CapabilityFamily {
        CapabilityFamily::new("approval_probe").unwrap()
    }

    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        vec![Arc::clone(&self.0)]
    }
}

/// What `thread/start` answers with `factory`: the thread's id, or the error's message.
async fn start_with(factory: Factory) -> std::result::Result<String, String> {
    let factory = Arc::new(factory);
    let server = Server::start(factory.clone());
    let mut connection = server.connect(1).await;
    let id = server
        .request(&mut connection, "thread/start", json!({}))
        .await;
    let answer = connection.answer(id).await;
    assert_eq!(factory.origins(), vec![ThreadOrigin::Start]);
    match answer {
        Ok(result) => Ok(result["thread"]["id"].as_str().unwrap().to_owned()),
        Err(error) => {
            let message = error["message"].as_str().unwrap();
            assert!(message.starts_with("error creating thread: "), "{message}");
            Err(message.to_owned())
        }
    }
}

/// A thread one of whose tools needs approval for every call is refused when its settings adapter
/// asks the client about none of those calls, or it has none: the coding adapter asks about
/// commands and patches, and not about input to a terminal without its stdin review. Tools that
/// decide per call, need no approval or are never enabled are not checked; tools reached through
/// handoffs are checked. Capability tools are checked when their run-bound calls ask for approval.
#[tokio::test]
async fn thread_start_refuses_tools_whose_approvals_nobody_is_asked_for() {
    let script = Arc::new(Script::default());
    let refusal = |names: &str| {
        format!(
            "every call of tool {names} needs approval, but the thread's host asks the client \
             about none of them"
        )
    };
    let always = |name: &str| declared(name, ToolApprovalPolicy::Always);
    let solo = |tools| agent_with("lead", tools, Vec::new());

    for adapter in [false, true] {
        let error = start_with(Factory::running(
            &script,
            adapter,
            solo(vec![always("guarded")]),
            RunConfig::new(),
        ))
        .await
        .unwrap_err();
        assert!(error.contains(&refusal("`guarded`")), "{error}");
    }

    start_with(Factory::running(
        &script,
        true,
        solo(vec![always("exec_command"), always("apply_patch")]),
        RunConfig::new(),
    ))
    .await
    .unwrap();
    let error = start_with(Factory::running(
        &script,
        true,
        solo(vec![
            always("write_stdin"),
            always("apply_patch"),
            always("audit"),
        ]),
        RunConfig::new(),
    ))
    .await
    .unwrap_err();
    assert!(
        error.contains(&refusal("`audit`, `write_stdin`")),
        "{error}"
    );

    start_with(Factory::running(
        &script,
        false,
        solo(vec![
            declared("checked", ToolApprovalPolicy::Dynamic),
            declared("free", ToolApprovalPolicy::Never),
            declared_with(
                "off",
                ToolOptions::new()
                    .with_approval(ToolApprovalPolicy::Always)
                    .with_availability(ToolAvailability::Disabled),
            ),
        ]),
        RunConfig::new(),
    ))
    .await
    .unwrap();

    let delegate = agent_with("delegate", vec![always("delegated")], Vec::new());
    let lead = agent_with(
        "lead",
        Vec::new(),
        vec![HandoffSpec::new(
            AgentId::new("delegate"),
            ToolSchema::new("transfer_to_delegate", json!({"type": "object", "properties": {}, "required": [], "additionalProperties": false}))
                .unwrap(),
        )],
    );
    let registry = AgentRegistry::builder()
        .register(Arc::clone(&lead))
        .register(delegate)
        .build()
        .unwrap();
    let error = start_with(Factory::running(
        &script,
        false,
        lead,
        RunConfig::new().with_agent_registry(registry),
    ))
    .await
    .unwrap_err();
    assert!(error.contains(&refusal("`delegated`")), "{error}");
}

/// Resuming and forking load a thread as starting one does, and refuse it alike, leaving nothing
/// loaded.
#[tokio::test]
async fn thread_resume_and_fork_refuse_tools_whose_approvals_nobody_is_asked_for() {
    let script = Arc::new(Script::default());
    let factory = Arc::new(Factory::running(
        &script,
        false,
        agent_with(
            "lead",
            vec![declared("guarded", ToolApprovalPolicy::Always)],
            Vec::new(),
        ),
        RunConfig::new(),
    ));
    let server = Server::start(factory);
    server.store_thread("stored", "hello").await;
    let mut connection = server.connect(1).await;
    for (method, prefix) in [
        ("thread/resume", "error resuming thread"),
        ("thread/fork", "error forking thread"),
    ] {
        let error = server
            .call_error(&mut connection, method, json!({"threadId": "stored"}))
            .await;
        let message = error["message"].as_str().unwrap();
        assert!(
            message.starts_with(prefix) && message.contains("`guarded`"),
            "{message}"
        );
    }
    let loaded = server
        .call(&mut connection, "thread/loaded/list", json!({}))
        .await;
    let loaded: ThreadLoadedListResponse = serde_json::from_value(loaded).unwrap();
    assert!(loaded.data().is_empty(), "{loaded:?}");
}

/// Reject a reviewer before either returning a loaded thread or shutting an unwatched one down
/// for reload; the unsupported request must not change subscription or factory state.
#[tokio::test]
async fn unsupported_reviewer_does_not_reload_a_loaded_thread() {
    let script = Arc::new(Script::default());
    let factory = Factory::with_adapter(&script);
    let (server, _) = build(
        Options {
            store: Some(Arc::new(InMemoryThreadStore::new())),
            factory: Some(factory.clone()),
            unload_delay: Some(Duration::from_secs(60)),
        },
        None,
    );
    let mut connection = server.connect(1).await;
    let thread = server
        .start_thread(&mut connection, ThreadStartParams::new())
        .await;
    for subscribed in [true, false] {
        if !subscribed {
            server
                .call(
                    &mut connection,
                    "thread/unsubscribe",
                    json!({"threadId": thread.id()}),
                )
                .await;
        }
        for reviewer in ["auto_review", "guardian_subagent"] {
            let error = server
                .call_error(
                    &mut connection,
                    "thread/resume",
                    json!({
                        "threadId": thread.id(), "approvalsReviewer": reviewer
                    }),
                )
                .await;
            assert_eq!(
                error["message"],
                "thread/resume.approvalsReviewer `auto_review` is not supported"
            );
        }
        let loaded = server
            .call(&mut connection, "thread/loaded/list", json!({}))
            .await;
        assert!(
            loaded["data"]
                .as_array()
                .unwrap()
                .iter()
                .any(|id| id == thread.id())
        );
        assert_eq!(factory.origins(), vec![ThreadOrigin::Start]);
    }
    let resumed = server
        .resume(&mut connection, ThreadResumeParams::new(thread.id()))
        .await;
    assert_eq!(resumed.settings()["approvalsReviewer"], "user");
    assert_eq!(factory.origins(), vec![ThreadOrigin::Start]);
}

fn tool_response(name: &str) -> ModelResponse {
    ModelResponse::new(vec![RunItem::new(
        ItemId::new("tool-item"),
        RunItemKind::ToolCall(ToolCall::new(CallId::new("call"), name, json!({}))),
    )])
}

fn last_tool_output(script: &Script) -> Value {
    script
        .last_request()
        .input()
        .iter()
        .find_map(|item| match item {
            ModelInputItem::ToolCallOutput(output) if output.call_id().as_str() == "call" => {
                Some(output.output().clone())
            }
            _ => None,
        })
        .expect("the model receives the tool result")
}

/// A fixed allow runs the tool without a request; a fixed denial keeps the thread usable while
/// refusing the call. Neither requires an approval adapter despite the tool's Always fallback.
#[tokio::test]
async fn static_approval_checks_preserve_effective_permission_decisions() {
    for (permission, runs) in [
        (
            PermissionEngine::new(PermissionMode::BypassPermissions),
            true,
        ),
        (
            PermissionEngine::default()
                .with_rules([
                    PermissionRule::new(PermissionDecision::Allow).with_tool_name("guarded")
                ]),
            true,
        ),
        (
            PermissionEngine::default().with_rules([
                PermissionRule::new(PermissionDecision::Deny).with_tool_name("guarded")
            ]),
            false,
        ),
        (PermissionEngine::new(PermissionMode::DontAsk), false),
    ] {
        let script = Arc::new(Script::default());
        let factory = Arc::new(Factory::running(
            &script,
            false,
            agent_with(
                "lead",
                vec![declared("guarded", ToolApprovalPolicy::Always)],
                vec![],
            ),
            RunConfig::new().with_permission_engine(permission),
        ));
        let server = Server::start(factory);
        let mut connection = server.connect(1).await;
        let thread = server
            .start_thread(&mut connection, ThreadStartParams::new())
            .await;
        script.push(Step::Respond(tool_response("guarded")));
        script.answer("done");
        server
            .run_turn(&mut connection, thread.id(), "call it")
            .await;
        assert_eq!(last_tool_output(&script).to_string().contains("ran"), runs);
        assert!(
            !connection
                .passed
                .iter()
                .any(|message| message.get("id").is_some() && message.get("method").is_some()),
            "no approval request is sent"
        );
    }
}

/// Requested and recorded settings must apply before the static check on every loading path.
#[tokio::test]
async fn approval_checks_use_start_resume_and_fork_settings() {
    let script = Arc::new(Script::default());
    let factory = Arc::new(Factory::running(
        &script,
        true,
        agent_with(
            "lead",
            vec![declared("guarded", ToolApprovalPolicy::Always)],
            vec![],
        ),
        RunConfig::new(),
    ));
    let (server, _) = build(
        Options {
            store: Some(Arc::new(InMemoryThreadStore::new())),
            factory: Some(factory),
            unload_delay: Some(Duration::ZERO),
        },
        None,
    );
    let mut connection = server.connect(1).await;
    let started = server
        .call(
            &mut connection,
            "thread/start",
            json!({"approvalPolicy": "never"}),
        )
        .await;
    let id = started["thread"]["id"].as_str().unwrap();
    assert_eq!(started["approvalPolicy"], "never");
    script.answer("done");
    server.run_turn(&mut connection, id, "save settings").await;
    let forked = server
        .call(&mut connection, "thread/fork", json!({"threadId": id}))
        .await;
    assert_eq!(forked["approvalPolicy"], "never");
    unload(&server, &mut connection, id).await;
    let resumed = server
        .resume(&mut connection, ThreadResumeParams::new(id))
        .await;
    assert_eq!(resumed.settings()["approvalPolicy"], "never");
}

struct BoundTools {
    binds: Arc<Mutex<usize>>,
    unbound_reads: Arc<Mutex<usize>>,
}

#[async_trait]
impl Capability for BoundTools {
    fn kind(&self) -> CapabilityFamily {
        CapabilityFamily::new("approval_probe").unwrap()
    }

    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        *self.unbound_reads.lock().unwrap() += 1;
        vec![declared("not_bound", ToolApprovalPolicy::Always)]
    }

    fn bind(&self, _context: &ra_core::context::RunContext) -> Result<Option<Arc<dyn Capability>>> {
        *self.binds.lock().unwrap() += 1;
        Ok(Some(Arc::new(Contributes(declared(
            "bound",
            ToolApprovalPolicy::Never,
        )))))
    }
}

/// Only the bound capability contributes tools, and binding happens once when its run begins.
#[tokio::test]
async fn loading_preserves_capability_binding_and_uses_the_bound_tools() {
    let script = Arc::new(Script::default());
    let binds = Arc::new(Mutex::new(0));
    let unbound_reads = Arc::new(Mutex::new(0));
    let factory = Arc::new(Factory::running(
        &script,
        false,
        agent(),
        RunConfig::new().with_capability(Arc::new(BoundTools {
            binds: Arc::clone(&binds),
            unbound_reads: Arc::clone(&unbound_reads),
        })),
    ));
    let server = Server::start(factory);
    let mut connection = server.connect(1).await;
    let thread = server
        .start_thread(&mut connection, ThreadStartParams::new())
        .await;
    assert_eq!(*binds.lock().unwrap(), 0);
    assert_eq!(*unbound_reads.lock().unwrap(), 0);
    script.push(Step::Respond(tool_response("bound")));
    script.answer("done");
    server
        .run_turn(&mut connection, thread.id(), "call bound tool")
        .await;
    assert!(last_tool_output(&script).to_string().contains("ran"));
    assert_eq!(*binds.lock().unwrap(), 1);
    assert_eq!(*unbound_reads.lock().unwrap(), 0);
}

/// A capability's unsupported approval is refused through the normal path after run binding.
#[tokio::test]
async fn unsupported_capability_approvals_are_refused_when_called() {
    let script = Arc::new(Script::default());
    let factory = Arc::new(Factory::running(
        &script,
        false,
        agent(),
        RunConfig::new().with_capability(Arc::new(Contributes(declared(
            "contributed",
            ToolApprovalPolicy::Always,
        )))),
    ));
    let server = Server::start(factory);
    let mut connection = server.connect(1).await;
    let thread = server
        .start_thread(&mut connection, ThreadStartParams::new())
        .await;
    script.push(Step::Respond(tool_response("contributed")));
    script.answer("done");
    server
        .run_turn(&mut connection, thread.id(), "call contributed tool")
        .await;
    let output = last_tool_output(&script);
    assert_eq!(output, json!("approval request failed"));
    assert!(
        !connection
            .passed
            .iter()
            .any(|message| message.get("id").is_some() && message.get("method").is_some()),
        "no unsupported request is sent"
    );
}

// ---------------------------------------------------------------------------------------------
// Dynamic tools
// ---------------------------------------------------------------------------------------------

/// Ported from Codex's `thread_start_rejects_invalid_dynamic_tool_inputs` and
/// `thread_start_rejects_hidden_dynamic_tools_without_namespace`: tools in mixed shapes, an empty
/// namespace, a repeated namespace and a deferred tool outside a namespace are refused with
/// Codex's texts before anything is built; tools in the legacy shape are read and checked like
/// canonical ones, and an empty list starts a thread with none.
#[tokio::test]
async fn thread_start_checks_dynamic_tools() {
    let script = Arc::new(Script::default());
    let factory = Factory::new(&script);
    let server = Server::start(factory.clone());
    let mut connection = server.connect(1).await;
    let schema = json!({"type": "object", "properties": {}});
    let function = |name: &str| json!({"type": "function", "name": name, "description": name, "inputSchema": schema});
    let namespace = |name: &str, tools: Vec<Value>| json!({"type": "namespace", "name": name, "description": name, "tools": tools});

    for (dynamic_tools, expected) in [
        (
            json!([
                function("canonical_tool"),
                {
                    "namespace": "legacy_app",
                    "name": "legacy_tool",
                    "description": "Legacy tool",
                    "inputSchema": schema
                }
            ]),
            "Invalid request: dynamic tools must use either canonical or legacy format consistently",
        ),
        (
            json!([namespace(
                "canonical_namespace",
                vec![json!({
                    "type": "function",
                    "name": "legacy_visibility_tool",
                    "description": "Uses a legacy visibility field",
                    "inputSchema": schema,
                    "exposeToContext": false
                })]
            )]),
            "Invalid request: dynamic tools must use either canonical or legacy format consistently",
        ),
        (
            json!([namespace("empty_namespace", vec![])]),
            "dynamic tool namespace must contain at least one tool: empty_namespace",
        ),
        (
            json!([
                namespace("duplicate_namespace", vec![function("first_tool")]),
                namespace("duplicate_namespace", vec![function("second_tool")])
            ]),
            "duplicate dynamic tool namespace: duplicate_namespace",
        ),
        (
            json!([{
                "type": "function",
                "name": "hidden_tool",
                "description": "Hidden dynamic tool",
                "inputSchema": schema,
                "deferLoading": true
            }]),
            "deferred dynamic tool must include a namespace: hidden_tool",
        ),
        (
            json!([{"name": "hidden_tool", "description": "", "inputSchema": schema,
                "exposeToContext": false}]),
            "deferred dynamic tool must include a namespace: hidden_tool",
        ),
        (
            json!([{"name": "lookup.ticket", "description": "", "inputSchema": schema}]),
            "dynamic tool name must match ^[a-zA-Z0-9_-]+$ to match Responses API: lookup.ticket",
        ),
    ] {
        assert_eq!(
            server
                .call_error(
                    &mut connection,
                    "thread/start",
                    json!({"dynamicTools": dynamic_tools}),
                )
                .await,
            invalid_request(expected)
        );
    }
    assert!(factory.origins().is_empty(), "no thread is prepared");

    let started = server
        .call(&mut connection, "thread/start", json!({"dynamicTools": []}))
        .await;
    assert!(started["thread"]["id"].is_string(), "{started}");
    assert_eq!(factory.origins(), vec![ThreadOrigin::Start]);
}
