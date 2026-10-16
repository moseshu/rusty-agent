//! A thread's items, live and read back, as Codex's app-server has them
//! (`app-server-protocol/src/protocol/v2/{item,thread}.rs`, `protocol/thread_history.rs`,
//! `app-server/src/bespoke_event_handling.rs`,
//! `app-server/src/request_processors/thread_processor.rs`).
//!
//! The wire cases pin each payload against the shape of Codex's generated schema. The server cases
//! run the server in this process with connections whose write queues the test holds, and threads
//! loaded through the crate's `test-api` door whose turns run on a scripted model and are recorded
//! into an in-memory thread store, which `thread/turns/list` reads back. They port Codex's
//! `turn_start` cases on item notifications and its `thread_read` cases on `thread/turns/list` and
//! `thread/items/list`; a history read case writes the thread's rollout records itself, as Codex's
//! cases write rollout lines.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use ra_core::{
    agent::{AgentId, AgentSpec},
    error::{Error, Result},
    item::{
        CallId, Compaction, ContentBlock, ItemId, Message, MessageRole, ModelInputItem,
        ModelResponse, OutputPhase, Reasoning, RunItem, RunItemKind, ToolCall, ToolCallOutput,
    },
    model::{
        ApiProtocol, Model, ModelRequest, ModelResolver, ModelSelector, ModelSettings, ProviderKey,
        ResolvedModel,
    },
    session::{
        InterruptedTurnHistoryMarker, SessionId,
        rollout::{
            RolloutItem, RolloutRecorder, RolloutRunEnd, RolloutRunEnded, RolloutRunStarted,
        },
    },
    state::RunId,
    tool::{
        Tool, ToolApprovalPolicy, ToolContext, ToolOptions, ToolOrigin, ToolOutput, ToolSchema,
    },
};
use ra_protocol::{
    frame::{JsonRpcMessage, JsonRpcNotification, JsonRpcRequest},
    server::{
        AppServer, ThreadToolItems,
        protocol::{
            ClientInfo, CommandExecutionItem, CommandExecutionStatus, FileUpdateChange,
            InitializeCapabilities, InitializeParams, ItemCompletedNotification,
            ItemStartedNotification, MessagePhase, PatchApplyStatus, PatchChangeKind,
            SortDirection, ThreadItem, ThreadItemsListAnchor, ThreadItemsListCursor,
            ThreadItemsListParams, ThreadItemsListResponse, ThreadTurnsListParams,
            ThreadTurnsListResponse, Turn, TurnInterruptParams, TurnItemsView, TurnStartParams,
            TurnStatus, UserInput,
        },
        test_api::ThreadLoader,
    },
    transport::{
        CHANNEL_CAPACITY, ConnectionId, ConnectionOrigin, QueuedOutgoingMessage, TransportEvent,
        test_api::connection_opened,
    },
};
use ra_runtime::{
    agent::AgentBinding,
    runner::RunConfig,
    thread::{AgentThread, turn_input_item_id},
};
use ra_session::{
    RolloutSessionMeta,
    store::{CreateThreadParams, InMemoryThreadStore, ThreadStore},
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

const THREAD: &str = "thread-1";

// ---------------------------------------------------------------------------------------------
// A scripted model and a tool
// ---------------------------------------------------------------------------------------------

enum Step {
    Respond(ModelResponse),
    After(Arc<Notify>, ModelResponse),
    Fail,
    FailAfter(Arc<Notify>),
    Hang,
}

#[derive(Default)]
struct Script {
    steps: Mutex<VecDeque<Step>>,
    calls: Mutex<usize>,
    called: Notify,
}

impl Script {
    fn push(&self, step: Step) {
        self.steps.lock().unwrap().push_back(step);
    }

    async fn wait_for_calls(&self, count: usize) {
        timeout(PATIENCE, async {
            loop {
                let called = self.called.notified();
                if *self.calls.lock().unwrap() >= count {
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
            Arc::new(ScriptedModel(Arc::clone(&self.0))) as Arc<dyn Model>,
            ModelSettings::new(),
            ModelSettings::new(),
        ))
    }
}

struct ScriptedModel(Arc<Script>);

#[async_trait]
impl Model for ScriptedModel {
    async fn get_response(&self, _request: ModelRequest) -> Result<ModelResponse> {
        *self.0.calls.lock().unwrap() += 1;
        self.0.called.notify_waiters();
        let step = self.0.steps.lock().unwrap().pop_front();
        match step {
            Some(Step::Respond(response)) => Ok(response),
            Some(Step::After(gate, response)) => {
                gate.notified().await;
                Ok(response)
            }
            Some(Step::FailAfter(gate)) => {
                gate.notified().await;
                Err(Error::caller("the model failed"))
            }
            Some(Step::Fail) => Err(Error::caller("the model failed")),
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

fn tool_call(id: &str, call_id: &str) -> ModelResponse {
    ModelResponse::new(vec![RunItem::new(
        ItemId::new(id),
        RunItemKind::ToolCall(ToolCall::new(CallId::new(call_id), "quick_look", json!({}))),
    )])
}

/// A tool that answers at once.
struct QuickTool {
    origin: ToolOrigin,
    schema: ToolSchema,
}

#[async_trait]
impl Tool for QuickTool {
    fn origin(&self) -> &ToolOrigin {
        &self.origin
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    fn options(&self) -> ToolOptions {
        ToolOptions::new().with_approval(ToolApprovalPolicy::Never)
    }

    async fn call(&self, _context: ToolContext<'_>) -> Result<ToolOutput> {
        Ok(ToolOutput::text("looked"))
    }
}

/// Shows `quick_look` as the command `look`, as a coding host shows its command tool.
#[derive(Debug)]
struct ShowLooks;

impl ThreadToolItems for ShowLooks {
    fn started(&self, call: &ToolCall) -> Option<ThreadItem> {
        (call.name() == "quick_look").then(|| {
            CommandExecutionItem::new(
                call.call_id().as_str(),
                "look",
                "/work",
                CommandExecutionStatus::InProgress,
            )
            .into()
        })
    }

    fn completed(&self, call: &ToolCall, output: &ToolCallOutput) -> Option<ThreadItem> {
        let text = ToolOutput::from_stored(output.output())
            .ok()
            .flatten()
            .and_then(|output| output.as_text().map(str::to_owned))
            .unwrap_or_default();
        (call.name() == "quick_look").then(|| {
            CommandExecutionItem::new(
                call.call_id().as_str(),
                "look",
                "/work",
                CommandExecutionStatus::Completed,
            )
            .with_aggregated_output(text)
            .with_exit_code(0)
            .into()
        })
    }
}

fn agent() -> Arc<AgentSpec> {
    let tool = QuickTool {
        origin: ToolOrigin::new("quick_look").unwrap(),
        schema: ToolSchema::new(
            "quick_look",
            json!({"type": "object", "properties": {}, "required": [], "additionalProperties": false}),
        )
        .unwrap(),
    };
    AgentSpec::builder()
        .id(AgentId::new("lead"))
        .name("Lead")
        .instructions("lead")
        .tools(vec![Arc::new(tool) as Arc<dyn Tool>])
        .build()
        .unwrap()
}

fn agent_thread(script: &Arc<Script>, recorder: Option<Arc<dyn RolloutRecorder>>) -> AgentThread {
    let builder = AgentThread::builder(
        SessionId::new(THREAD),
        AgentBinding::direct(agent()),
        Arc::new(Resolver(Arc::clone(script))) as Arc<dyn ModelResolver>,
    )
    .with_config(RunConfig::new());
    match recorder {
        Some(recorder) => builder.with_rollout_recorder(recorder),
        None => builder,
    }
    .build()
}

// ---------------------------------------------------------------------------------------------
// The server in this process
// ---------------------------------------------------------------------------------------------

struct Server {
    events: mpsc::Sender<TransportEvent>,
    loader: ThreadLoader,
    store: Arc<InMemoryThreadStore>,
    _task: JoinHandle<()>,
}

impl Server {
    /// A server reading history from a store of its own, showing tool calls with `tool_items`.
    fn start(tool_items: Option<Arc<dyn ThreadToolItems>>) -> Self {
        let store = Arc::new(InMemoryThreadStore::new());
        let mut server = AppServer::new(std::env::temp_dir().join("rusty-agent-items-test"))
            .unwrap()
            .with_thread_store(Arc::clone(&store) as Arc<dyn ThreadStore>);
        if let Some(tool_items) = tool_items {
            server = server.with_thread_tool_items(tool_items);
        }
        Self::run(server, store)
    }

    /// A server that reads no history.
    fn start_without_store() -> Self {
        let server = AppServer::new(std::env::temp_dir().join("rusty-agent-items-test")).unwrap();
        Self::run(server, Arc::new(InMemoryThreadStore::new()))
    }

    fn run(server: AppServer, store: Arc<InMemoryThreadStore>) -> Self {
        let loader = ThreadLoader::new(&server);
        let (events, transport_event_rx) = mpsc::channel(CHANNEL_CAPACITY);
        let task = tokio::spawn(server.run(transport_event_rx));
        Self {
            events,
            loader,
            store,
            _task: task,
        }
    }

    /// Creates the thread in the store and returns what records into it.
    async fn create_thread(&self) -> Arc<dyn RolloutRecorder> {
        self.store
            .create_thread_with(&CreateThreadParams::new(RolloutSessionMeta::new(
                SessionId::new(THREAD),
            )))
            .await
            .unwrap()
    }

    /// Opens and initializes a connection.
    async fn connect(&self, id: u64) -> Connection {
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
            .with_capabilities(InitializeCapabilities::new().with_experimental_api(true));
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

    /// Loads a thread on `script` recording into the store, with `connections` subscribed.
    async fn load(&self, script: &Arc<Script>, connections: &[&Connection]) -> AgentThread {
        let recorder = self.create_thread().await;
        self.load_thread(agent_thread(script, Some(recorder)), connections)
            .await
    }

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

    /// Starts a turn on `text` and returns its id.
    async fn turn_start(&self, connection: &mut Connection, text: &str) -> String {
        let params = TurnStartParams::new(THREAD, vec![UserInput::text(text)]);
        let id = self
            .request(
                connection,
                "turn/start",
                serde_json::to_value(params).unwrap(),
            )
            .await;
        connection.response(id).await["turn"]["id"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    async fn turns_list(
        &self,
        connection: &mut Connection,
        params: ThreadTurnsListParams,
    ) -> ThreadTurnsListResponse {
        let id = self
            .request(
                connection,
                "thread/turns/list",
                serde_json::to_value(params).unwrap(),
            )
            .await;
        serde_json::from_value(connection.response(id).await).unwrap()
    }

    async fn turns_list_error(
        &self,
        connection: &mut Connection,
        params: ThreadTurnsListParams,
    ) -> Value {
        let id = self
            .request(
                connection,
                "thread/turns/list",
                serde_json::to_value(params).unwrap(),
            )
            .await;
        connection.error(id).await
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

    async fn notification(&mut self, method: &str) -> Value {
        self.until(|message| message["method"] == method && message.get("id").is_none())
            .await["params"]
            .clone()
    }

    /// The notifications of one turn, from its start to its end, in order: their methods, and the
    /// items of its item notifications.
    async fn turn_notifications(&mut self) -> Vec<(String, Value)> {
        self.notification("turn/started").await;
        let mut seen = Vec::new();
        loop {
            let message = self
                .until(|message| {
                    message.get("id").is_none()
                        && matches!(
                            message["method"].as_str(),
                            Some("item/started" | "item/completed" | "turn/completed")
                        )
                })
                .await;
            let method = message["method"].as_str().unwrap().to_owned();
            if method == "turn/completed" {
                seen.push((method, message["params"]["turn"].clone()));
                return seen;
            }
            seen.push((method, message["params"]["item"].clone()));
        }
    }
}

/// The items `item/completed` reported, in order.
fn completed_items(notifications: &[(String, Value)]) -> Vec<ThreadItem> {
    notifications
        .iter()
        .filter(|(method, _)| method == "item/completed")
        .map(|(_, item)| serde_json::from_value(item.clone()).unwrap())
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

fn agent_texts(turn: &Turn) -> Vec<String> {
    turn.items()
        .iter()
        .filter_map(|item| match item {
            ThreadItem::AgentMessage { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

fn invalid_request(message: &str) -> Value {
    json!({"code": -32600, "message": message})
}

// ---------------------------------------------------------------------------------------------
// Rollout records, as a thread's turns leave them
// ---------------------------------------------------------------------------------------------

fn run_started(run_id: &str, text: Option<&str>) -> RolloutItem {
    let started = RolloutRunStarted::new(RunId::new(run_id), AgentId::new("lead"));
    RolloutItem::RunStarted(match text {
        Some(text) => started.with_input(vec![ModelInputItem::Message(Message::user(text))]),
        None => started,
    })
}

fn run_ended(run_id: &str, end: RolloutRunEnd) -> RolloutItem {
    RolloutItem::RunEnded(RolloutRunEnded::new(RunId::new(run_id), end))
}

fn agent_record(id: &str, text: &str, phase: OutputPhase) -> RolloutItem {
    RolloutItem::Item(RunItem::new(
        ItemId::new(id),
        RunItemKind::Message(Message::assistant(text, phase)),
    ))
}

/// Records a completed run of `text` answered with `answer`.
fn completed_run(run_id: &str, text: &str, answer: &str) -> Vec<RolloutItem> {
    vec![
        run_started(run_id, Some(text)),
        agent_record(&format!("{run_id}-answer"), answer, OutputPhase::Final),
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
// Wire shapes
// ---------------------------------------------------------------------------------------------

/// The items a run here produces, in the shape of Codex's `ThreadItem`.
#[test]
fn thread_items_match_codexs_wire_shape() {
    let cases = [
        (
            ThreadItem::user_message("u1", vec![UserInput::text("hi")]),
            json!({"type": "userMessage", "id": "u1", "clientId": null,
                   "content": [{"type": "text", "text": "hi", "text_elements": []}]}),
        ),
        (
            ThreadItem::agent_message("a1", "done", Some(MessagePhase::FinalAnswer)),
            json!({"type": "agentMessage", "id": "a1", "text": "done", "phase": "final_answer",
                   "memoryCitation": null, "delivery": null, "questions": null}),
        ),
        (
            ThreadItem::reasoning("r1", vec!["sum".into()], vec![]),
            json!({"type": "reasoning", "id": "r1", "summary": ["sum"], "content": []}),
        ),
        (
            CommandExecutionItem::new("call-1", "ls", "/work", CommandExecutionStatus::Completed)
                .with_aggregated_output("a\n")
                .with_exit_code(0)
                .with_process_id("7")
                .into(),
            json!({"type": "commandExecution", "id": "call-1", "pluginId": null,
                   "scriptPath": null, "command": "ls", "cwd": "/work", "processId": "7",
                   "source": "agent", "status": "completed", "commandActions": [],
                   "aggregatedOutput": "a\n", "exitCode": 0, "durationMs": null}),
        ),
        (
            ThreadItem::file_change(
                "call-2",
                vec![
                    FileUpdateChange::new("a.txt", PatchChangeKind::Add, "x\n"),
                    FileUpdateChange::new(
                        "b.txt",
                        PatchChangeKind::update(Some("c.txt".into())),
                        "@@\n-a\n+b\n",
                    ),
                ],
                PatchApplyStatus::InProgress,
            ),
            json!({"type": "fileChange", "id": "call-2", "status": "inProgress", "changes": [
                {"path": "a.txt", "kind": {"type": "add"}, "diff": "x\n"},
                {"path": "b.txt", "kind": {"type": "update", "move_path": "c.txt"},
                 "diff": "@@\n-a\n+b\n"},
            ]}),
        ),
        (
            ThreadItem::context_compaction("c1"),
            json!({"type": "contextCompaction", "id": "c1"}),
        ),
    ];
    for (item, wire) in cases {
        assert_eq!(serde_json::to_value(&item).unwrap(), wire);
        assert_eq!(serde_json::from_value::<ThreadItem>(wire).unwrap(), item);
    }
}

/// Codex's `ItemStartedNotification` and `ItemCompletedNotification`, read as Codex writes them.
#[test]
fn item_notifications_match_codexs_wire_shape() {
    let item = json!({"type": "contextCompaction", "id": "c1"});
    let started = json!({"item": item, "threadId": "t", "turnId": "u", "startedAtMs": 5});
    let read: ItemStartedNotification = serde_json::from_value(started.clone()).unwrap();
    assert_eq!(read.item(), &ThreadItem::context_compaction("c1"));
    assert_eq!(
        (read.thread_id(), read.turn_id(), read.started_at_ms()),
        ("t", "u", 5)
    );
    assert_eq!(serde_json::to_value(read).unwrap(), started);

    let completed = json!({"item": item, "threadId": "t", "turnId": "u", "completedAtMs": 6});
    let read: ItemCompletedNotification = serde_json::from_value(completed.clone()).unwrap();
    assert_eq!(read.completed_at_ms(), 6);
    assert_eq!(serde_json::to_value(read).unwrap(), completed);
}

/// Codex's `ThreadTurnsListParams`, `ThreadItemsListParams` and their answers.
#[test]
fn history_reads_match_codexs_wire_shape() {
    let params: ThreadTurnsListParams = serde_json::from_value(json!({
        "threadId": "t", "cursor": "c", "limit": 2, "sortDirection": "asc", "itemsView": "full",
    }))
    .unwrap();
    assert_eq!(
        params,
        ThreadTurnsListParams::new("t")
            .with_cursor("c")
            .with_limit(2)
            .with_sort_direction(SortDirection::Asc)
            .with_items_view(TurnItemsView::Full)
    );
    let params: ThreadTurnsListParams = serde_json::from_value(json!({"threadId": "t"})).unwrap();
    assert_eq!(params, ThreadTurnsListParams::new("t"));

    // Codex's cursor is a string or an item anchor.
    let params: ThreadItemsListParams = serde_json::from_value(json!({
        "threadId": "t", "turnId": "u", "cursor": {"type": "item", "itemId": "i"},
        "limit": 1, "sortDirection": "desc",
    }))
    .unwrap();
    assert_eq!(
        params,
        ThreadItemsListParams::new("t")
            .with_turn_id("u")
            .with_cursor(ThreadItemsListCursor::Anchor(ThreadItemsListAnchor::item(
                "i"
            )))
            .with_limit(1)
            .with_sort_direction(SortDirection::Desc)
    );
    let params: ThreadItemsListParams =
        serde_json::from_value(json!({"threadId": "t", "cursor": "opaque"})).unwrap();
    assert_eq!(
        params.cursor(),
        Some(&ThreadItemsListCursor::Opaque("opaque".into()))
    );

    let response = json!({"data": [], "nextCursor": null, "backwardsCursor": null});
    let read: ThreadTurnsListResponse = serde_json::from_value(response.clone()).unwrap();
    assert_eq!(serde_json::to_value(read).unwrap(), response);
    let response = json!({"data": [{"turnId": "u", "item": {"type": "contextCompaction", "id": "c"},
                                    "startedAtMs": null, "completedAtMs": 3}],
                          "nextCursor": "n", "backwardsCursor": null});
    let read: ThreadItemsListResponse = serde_json::from_value(response.clone()).unwrap();
    assert_eq!(read.data()[0].completed_at_ms(), Some(3));
    assert_eq!(serde_json::to_value(read).unwrap(), response);
}

/// Codex's `verify_server_notification_serialization`: a server notification says when it was
/// sent beside its method and parameters; a notification without the stamp writes none.
#[test]
fn notifications_carry_when_they_were_emitted() {
    let notification = JsonRpcNotification::new("thread/closed")
        .with_params(json!({"threadId": "t"}))
        .with_emitted_at_ms(1_234);
    let wire =
        json!({"method": "thread/closed", "params": {"threadId": "t"}, "emittedAtMs": 1_234});
    assert_eq!(serde_json::to_value(&notification).unwrap(), wire);
    let read: JsonRpcMessage = serde_json::from_value(wire).unwrap();
    assert_eq!(read, JsonRpcMessage::from(notification));
    assert_eq!(
        serde_json::to_value(JsonRpcNotification::new("initialized")).unwrap(),
        json!({"method": "initialized"})
    );
}

// ---------------------------------------------------------------------------------------------
// Items as a turn runs
// ---------------------------------------------------------------------------------------------

/// Codex's `turn_start_emits_user_message_item_with_text_elements` and the answer `turn/completed`
/// carries: the input starts and completes as a user message right after the turn starts, the
/// answer as an agent message, and the completed turn carries the answer as its summary.
#[tokio::test]
async fn a_turn_reports_its_input_and_answer_as_items() {
    let server = Server::start(None);
    let mut connection = server.connect(1).await;
    let script = Arc::new(Script::default());
    server.load(&script, &[&connection]).await;
    script.push(Step::Respond(final_message("answer-1", "Done")));

    let turn_id = server.turn_start(&mut connection, "Hello").await;
    let notifications = connection.turn_notifications().await;
    let methods: Vec<_> = notifications
        .iter()
        .map(|(method, item)| format!("{method} {}", item["type"].as_str().unwrap_or("turn")))
        .collect();
    assert_eq!(
        methods,
        [
            "item/started userMessage",
            "item/completed userMessage",
            "item/started agentMessage",
            "item/completed agentMessage",
            "turn/completed turn",
        ]
    );
    let input_id = turn_input_item_id(&turn_id, 0);
    assert_eq!(
        completed_items(&notifications),
        [
            ThreadItem::user_message(input_id.as_str(), vec![UserInput::text("Hello")]),
            ThreadItem::agent_message("answer-1", "Done", Some(MessagePhase::FinalAnswer)),
        ]
    );
    let completed: Turn = serde_json::from_value(notifications[4].1.clone()).unwrap();
    assert_eq!(completed.status(), TurnStatus::Completed);
    assert_eq!(completed.items_view(), TurnItemsView::Summary);
    assert_eq!(agent_texts(&completed), ["Done"]);
}

/// The item notifications say which thread and turn they belong to and when, and are stamped
/// with when they were sent.
#[tokio::test]
async fn item_notifications_name_their_turn_and_time() {
    let server = Server::start(None);
    let mut connection = server.connect(1).await;
    let script = Arc::new(Script::default());
    server.load(&script, &[&connection]).await;
    script.push(Step::Respond(final_message("answer-1", "Done")));

    let turn_id = server.turn_start(&mut connection, "Hello").await;
    let started = connection
        .until(|message| message["method"] == "item/started")
        .await;
    assert_eq!(started["params"]["threadId"], THREAD);
    assert_eq!(started["params"]["turnId"], turn_id.as_str());
    assert!(started["params"]["startedAtMs"].as_i64().unwrap() > 0);
    assert!(started["emittedAtMs"].as_i64().unwrap() > 0);
    let completed = connection
        .until(|message| message["method"] == "item/completed")
        .await;
    assert!(completed["params"]["completedAtMs"].as_i64().unwrap() > 0);
    connection.notification("turn/completed").await;
}

/// A tool call shows the item the host gives it, started when the call is made and completed
/// with its output, as a Codex tool announces its own items; without the host's say, it shows
/// none, as a Codex function tool shows none.
#[tokio::test]
async fn tool_calls_show_the_items_the_host_gives_them() {
    for shown in [true, false] {
        let tool_items = shown.then(|| Arc::new(ShowLooks) as Arc<dyn ThreadToolItems>);
        let server = Server::start(tool_items);
        let mut connection = server.connect(1).await;
        let script = Arc::new(Script::default());
        server.load(&script, &[&connection]).await;
        script.push(Step::Respond(tool_call("call-item", "call-1")));
        script.push(Step::Respond(final_message("answer-1", "Done")));

        server.turn_start(&mut connection, "look").await;
        let notifications = connection.turn_notifications().await;
        let commands: Vec<_> = notifications
            .iter()
            .filter(|(_, item)| item["type"] == "commandExecution")
            .map(|(method, item)| (method.as_str(), item["status"].as_str().unwrap()))
            .collect();
        if shown {
            assert_eq!(
                commands,
                [
                    ("item/started", "inProgress"),
                    ("item/completed", "completed")
                ]
            );
            let completed = notifications
                .iter()
                .find(|(method, item)| {
                    method == "item/completed" && item["type"] == "commandExecution"
                })
                .unwrap();
            assert_eq!(completed.1["id"], "call-1");
            assert_eq!(completed.1["aggregatedOutput"], "looked");
        } else {
            assert!(commands.is_empty(), "{notifications:?}");
        }
    }
}

/// Codex's `turn/completed` carries the answer only of a turn that completed: a failed turn and
/// an interrupted one carry no items.
#[tokio::test]
async fn only_a_completed_turn_carries_its_answer() {
    let server = Server::start(None);
    let mut connection = server.connect(1).await;
    let script = Arc::new(Script::default());
    server.load(&script, &[&connection]).await;

    script.push(Step::Fail);
    server.turn_start(&mut connection, "fail").await;
    let failed: Turn =
        serde_json::from_value(connection.notification("turn/completed").await["turn"].clone())
            .unwrap();
    assert_eq!(failed.status(), TurnStatus::Failed);
    assert!(failed.items().is_empty());
    assert_eq!(failed.items_view(), TurnItemsView::NotLoaded);

    script.push(Step::Hang);
    let turn_id = server.turn_start(&mut connection, "hang").await;
    script.wait_for_calls(2).await;
    server
        .request(
            &mut connection,
            "turn/interrupt",
            serde_json::to_value(TurnInterruptParams::new(THREAD, &turn_id)).unwrap(),
        )
        .await;
    let interrupted: Turn =
        serde_json::from_value(connection.notification("turn/completed").await["turn"].clone())
            .unwrap();
    assert_eq!(interrupted.status(), TurnStatus::Interrupted);
    assert!(interrupted.items().is_empty());
}

/// Input steered into a running turn is a user message of that turn once the run takes it in.
#[tokio::test]
async fn steered_input_is_a_user_message_of_the_running_turn() {
    let server = Server::start(None);
    let mut connection = server.connect(1).await;
    let script = Arc::new(Script::default());
    server.load(&script, &[&connection]).await;
    let gate = Arc::new(Notify::new());
    script.push(Step::After(
        Arc::clone(&gate),
        tool_call("call-item", "call-1"),
    ));
    script.push(Step::Respond(final_message("answer-1", "Done")));

    let turn_id = server.turn_start(&mut connection, "first").await;
    script.wait_for_calls(1).await;
    let id = server
        .request(
            &mut connection,
            "turn/steer",
            json!({"threadId": THREAD, "expectedTurnId": turn_id,
                   "input": [{"type": "text", "text": "also this"}]}),
        )
        .await;
    connection.response(id).await;
    gate.notify_one();

    let notifications = connection.turn_notifications().await;
    let users: Vec<_> = completed_items(&notifications)
        .into_iter()
        .filter_map(|item| match item {
            ThreadItem::UserMessage { content, .. } => Some(content),
            _ => None,
        })
        .collect();
    assert_eq!(
        users,
        [
            vec![UserInput::text("first")],
            vec![UserInput::text("also this")]
        ]
    );
}

// ---------------------------------------------------------------------------------------------
// Reading history back
// ---------------------------------------------------------------------------------------------

/// Codex's `thread_turns_list_can_page_backward_and_forward`: pages run newest first by default,
/// the next cursor continues past the page, and the backwards cursor reverses from the page's
/// first turn — including it again — and sees the turns added since.
#[tokio::test]
async fn thread_turns_list_can_page_backward_and_forward() {
    let server = Server::start(None);
    let mut connection = server.connect(1).await;
    let recorder = server.create_thread().await;
    let mut records = completed_run("run-1", "first", "one");
    records.extend(completed_run("run-2", "second", "two"));
    records.extend(completed_run("run-3", "third", "three"));
    record(&recorder, records).await;

    let page = server
        .turns_list(
            &mut connection,
            ThreadTurnsListParams::new(THREAD)
                .with_limit(2)
                .with_sort_direction(SortDirection::Desc),
        )
        .await;
    let texts: Vec<_> = page.data().iter().flat_map(user_texts).collect();
    assert_eq!(texts, ["third", "second"]);
    assert!(
        page.data()
            .iter()
            .all(|turn| turn.items_view() == TurnItemsView::Summary)
    );
    let next_cursor = page.next_cursor().expect("older turns remain").to_owned();
    let backwards_cursor = page
        .backwards_cursor()
        .expect("the page has a first turn")
        .to_owned();

    let page = server
        .turns_list(
            &mut connection,
            ThreadTurnsListParams::new(THREAD)
                .with_cursor(next_cursor)
                .with_limit(10)
                .with_sort_direction(SortDirection::Desc),
        )
        .await;
    let texts: Vec<_> = page.data().iter().flat_map(user_texts).collect();
    assert_eq!(texts, ["first"]);
    assert_eq!(page.next_cursor(), None);

    record(&recorder, completed_run("run-4", "fourth", "four")).await;
    let page = server
        .turns_list(
            &mut connection,
            ThreadTurnsListParams::new(THREAD)
                .with_cursor(backwards_cursor)
                .with_limit(10)
                .with_sort_direction(SortDirection::Asc),
        )
        .await;
    let texts: Vec<_> = page.data().iter().flat_map(user_texts).collect();
    assert_eq!(texts, ["third", "fourth"]);
}

/// Codex's `thread_turns_list_supports_requested_items_view`: all of a turn's items, a summary of
/// its first user message and last agent message, or none — with the turn otherwise the same.
#[tokio::test]
async fn thread_turns_list_supports_requested_items_view() {
    let server = Server::start(None);
    let mut connection = server.connect(1).await;
    let recorder = server.create_thread().await;
    record(
        &recorder,
        [
            run_started("run-1", Some("first")),
            agent_record("draft", "draft", OutputPhase::Commentary),
            agent_record("final", "final", OutputPhase::Final),
            run_ended("run-1", RolloutRunEnd::Completed),
        ],
    )
    .await;

    let read = async |connection: &mut Connection, view| {
        let page = server
            .turns_list(
                connection,
                ThreadTurnsListParams::new(THREAD)
                    .with_limit(10)
                    .with_sort_direction(SortDirection::Asc)
                    .with_items_view(view),
            )
            .await;
        assert_eq!(page.data().len(), 1);
        page.data()[0].clone()
    };
    let full = read(&mut connection, TurnItemsView::Full).await;
    assert_eq!(full.items_view(), TurnItemsView::Full);
    assert_eq!(agent_texts(&full), ["draft", "final"]);

    let summary = read(&mut connection, TurnItemsView::Summary).await;
    assert_eq!(summary.items_view(), TurnItemsView::Summary);
    assert_eq!(user_texts(&summary), ["first"]);
    assert_eq!(agent_texts(&summary), ["final"]);

    let not_loaded = read(&mut connection, TurnItemsView::NotLoaded).await;
    assert_eq!(not_loaded.items_view(), TurnItemsView::NotLoaded);
    assert!(not_loaded.items().is_empty());
    assert_eq!(not_loaded.id(), full.id());
    assert_eq!(not_loaded.status(), full.status());
    assert_eq!(not_loaded.started_at(), full.started_at());
    assert_eq!(not_loaded.completed_at(), full.completed_at());
    assert_eq!(not_loaded.duration_ms(), full.duration_ms());
    assert_eq!(full.status(), TurnStatus::Completed);
    assert!(full.started_at().is_some() && full.duration_ms().is_some());
}

/// Codex's `thread_items_list_returns_unsupported`: items are listed from Codex's paginated
/// history, which a legacy thread does not have; an item anchor needs its turn first.
#[tokio::test]
async fn thread_items_list_returns_unsupported() {
    let server = Server::start(None);
    let mut connection = server.connect(1).await;

    let id = server
        .request(
            &mut connection,
            "thread/items/list",
            serde_json::to_value(ThreadItemsListParams::new(
                "00000000-0000-4000-8000-000000000123",
            ))
            .unwrap(),
        )
        .await;
    assert_eq!(
        connection.error(id).await,
        json!({"code": -32601, "message": "thread/items/list is not supported yet"})
    );

    let id = server
        .request(
            &mut connection,
            "thread/items/list",
            serde_json::to_value(ThreadItemsListParams::new(THREAD).with_cursor(
                ThreadItemsListCursor::Anchor(ThreadItemsListAnchor::item("i")),
            ))
            .unwrap(),
        )
        .await;
    assert_eq!(
        connection.error(id).await,
        json!({"code": -32602, "message": "turnId is required when cursor is an item anchor"})
    );
}

/// What a client is sent as a thread's turns run is what it reads back once they have run: the
/// same turns, under the same ids, with the same items in the same order — nothing lost, nothing
/// twice.
#[tokio::test]
async fn turns_read_back_are_the_turns_sent_live() {
    let server = Server::start(Some(Arc::new(ShowLooks)));
    let mut connection = server.connect(1).await;
    let script = Arc::new(Script::default());
    server.load(&script, &[&connection]).await;
    script.push(Step::Respond(tool_call("call-item", "call-1")));
    script.push(Step::Respond(final_message("answer-1", "Looked")));
    script.push(Step::Respond(final_message("answer-2", "Again")));

    let mut live = Vec::new();
    for text in ["look", "again"] {
        let turn_id = server.turn_start(&mut connection, text).await;
        let notifications = connection.turn_notifications().await;
        live.push((turn_id, completed_items(&notifications)));
    }

    let page = server
        .turns_list(
            &mut connection,
            ThreadTurnsListParams::new(THREAD)
                .with_sort_direction(SortDirection::Asc)
                .with_items_view(TurnItemsView::Full),
        )
        .await;
    let read: Vec<_> = page
        .data()
        .iter()
        .map(|turn| (turn.id().to_owned(), turn.items().to_vec()))
        .collect();
    assert_eq!(read, live);
    assert!(
        page.data()
            .iter()
            .all(|turn| turn.status() == TurnStatus::Completed)
    );
    assert!(
        live[0]
            .1
            .iter()
            .any(|item| matches!(item, ThreadItem::CommandExecution { .. }))
    );
}

/// The turn running is read with what it has so far, laid over the rollout, as Codex lays its
/// listener's view of the running turn over the stored turns; once it is interrupted it reads as
/// interrupted.
#[tokio::test]
async fn a_running_turn_is_read_with_what_it_has_so_far() {
    let server = Server::start(None);
    let mut connection = server.connect(1).await;
    let script = Arc::new(Script::default());
    server.load(&script, &[&connection]).await;
    script.push(Step::Hang);

    let turn_id = server.turn_start(&mut connection, "wait").await;
    connection
        .until(|message| message["method"] == "item/completed")
        .await;
    script.wait_for_calls(1).await;
    let page = server
        .turns_list(
            &mut connection,
            ThreadTurnsListParams::new(THREAD).with_items_view(TurnItemsView::Full),
        )
        .await;
    assert_eq!(page.data().len(), 1);
    let running = &page.data()[0];
    assert_eq!(running.id(), turn_id);
    assert_eq!(running.status(), TurnStatus::InProgress);
    assert_eq!(user_texts(running), ["wait"]);

    server
        .request(
            &mut connection,
            "turn/interrupt",
            serde_json::to_value(TurnInterruptParams::new(THREAD, &turn_id)).unwrap(),
        )
        .await;
    connection.notification("turn/completed").await;
    let page = server
        .turns_list(
            &mut connection,
            ThreadTurnsListParams::new(THREAD).with_items_view(TurnItemsView::Full),
        )
        .await;
    assert_eq!(page.data().len(), 1);
    assert_eq!(page.data()[0].status(), TurnStatus::Interrupted);
    // The marker the interrupt leaves for the model is not the user's.
    assert_eq!(user_texts(&page.data()[0]), ["wait"]);
}

/// Codex's overlay of the running turn: the rollout may not hold it yet — here the thread records
/// nowhere the server reads — and the turn its listener follows is laid over the stored turns, in
/// progress, with what it has so far.
#[tokio::test]
async fn a_running_turn_not_yet_stored_is_laid_over_the_stored_turns() {
    let server = Server::start(None);
    let mut connection = server.connect(1).await;
    let recorder = server.create_thread().await;
    record(&recorder, completed_run("run-1", "stored", "one")).await;
    let script = Arc::new(Script::default());
    server
        .load_thread(agent_thread(&script, None), &[&connection])
        .await;
    script.push(Step::Hang);

    let turn_id = server.turn_start(&mut connection, "live").await;
    connection
        .until(|message| message["method"] == "item/completed")
        .await;
    let page = server
        .turns_list(
            &mut connection,
            ThreadTurnsListParams::new(THREAD)
                .with_sort_direction(SortDirection::Asc)
                .with_items_view(TurnItemsView::Full),
        )
        .await;
    let turns: Vec<_> = page
        .data()
        .iter()
        .map(|turn| (turn.id().to_owned(), turn.status(), user_texts(turn)))
        .collect();
    assert_eq!(
        turns,
        [
            (
                "run-1".to_owned(),
                TurnStatus::Completed,
                vec!["stored".to_owned()]
            ),
            (turn_id, TurnStatus::InProgress, vec!["live".to_owned()]),
        ]
    );
}

/// How each run ended is how its turn reads: completed, failed with its error, interrupted when
/// cancelled — and, when its thread is not running it, interrupted when it never recorded an end,
/// as Codex reports a turn cut off. A run paused for approval keeps its turn, and the segment that
/// continues it continues the same turn.
#[tokio::test]
async fn turns_read_back_end_as_their_runs_ended() {
    let server = Server::start(None);
    let mut connection = server.connect(1).await;
    let recorder = server.create_thread().await;
    record(
        &recorder,
        [
            run_started("paused", Some("approve")),
            RolloutItem::Item(RunItem::new(
                ItemId::new("ask"),
                RunItemKind::Message(Message::assistant("asking", OutputPhase::Commentary)),
            )),
            run_ended("paused", RolloutRunEnd::Interrupted),
            run_started("paused", None),
            agent_record("approved", "approved", OutputPhase::Final),
            run_ended("paused", RolloutRunEnd::Completed),
            run_started("failed", Some("fail")),
            RolloutItem::RunEnded(
                RolloutRunEnded::new(RunId::new("failed"), RolloutRunEnd::Failed)
                    .with_error("the model failed"),
            ),
            run_started("cancelled", Some("cancel")),
            run_ended("cancelled", RolloutRunEnd::Cancelled),
            run_started("cut-off", Some("cut")),
        ],
    )
    .await;

    let page = server
        .turns_list(
            &mut connection,
            ThreadTurnsListParams::new(THREAD)
                .with_sort_direction(SortDirection::Asc)
                .with_items_view(TurnItemsView::Full),
        )
        .await;
    let turns: Vec<_> = page
        .data()
        .iter()
        .map(|turn| (turn.id(), turn.status()))
        .collect();
    assert_eq!(
        turns,
        [
            ("paused", TurnStatus::Completed),
            ("failed", TurnStatus::Failed),
            ("cancelled", TurnStatus::Interrupted),
            ("cut-off", TurnStatus::Interrupted),
        ]
    );
    assert_eq!(agent_texts(&page.data()[0]), ["asking", "approved"]);
    assert_eq!(user_texts(&page.data()[0]), ["approve"]);
    assert_eq!(
        page.data()[1].error().map(|error| error.message()),
        Some("the model failed")
    );
    assert_eq!(page.data()[3].completed_at(), None);
}

/// A record recorded again under its id replaces its item in place; what Codex keeps out of a
/// turn's items stays out — system messages, and the marker an interrupt leaves — and a record
/// outside any run opens a turn of its own, named by its place in the rollout as Codex names one.
#[tokio::test]
async fn turns_read_back_keep_one_item_per_record() {
    let server = Server::start(None);
    let mut connection = server.connect(1).await;
    let recorder = server.create_thread().await;
    let marker = InterruptedTurnHistoryMarker::ContextualUser.item().unwrap();
    record(
        &recorder,
        [
            RolloutItem::Item(RunItem::new(
                ItemId::new("forked"),
                RunItemKind::Message(Message::user("from the fork")),
            )),
            run_started("run-1", Some("go")),
            RolloutItem::Item(RunItem::new(
                ItemId::new("system"),
                RunItemKind::Message(Message::system("context")),
            )),
            agent_record("answer", "early", OutputPhase::Commentary),
            RolloutItem::Item(RunItem::new(
                ItemId::new("thinking"),
                RunItemKind::Reasoning(Reasoning::new().with_summary(vec!["why".into()])),
            )),
            agent_record("answer", "settled", OutputPhase::Final),
            RolloutItem::Item(RunItem::new(
                ItemId::new("compacted"),
                RunItemKind::Compaction(Compaction::new("summary", Vec::new())),
            )),
            RolloutItem::Item(marker),
            run_ended("run-1", RolloutRunEnd::Cancelled),
        ],
    )
    .await;

    let page = server
        .turns_list(
            &mut connection,
            ThreadTurnsListParams::new(THREAD)
                .with_sort_direction(SortDirection::Asc)
                .with_items_view(TurnItemsView::Full),
        )
        .await;
    assert_eq!(page.data().len(), 2);
    // The rollout opens with the thread's metadata, so the record is its second.
    assert_eq!(page.data()[0].id(), "rollout-1");
    assert_eq!(user_texts(&page.data()[0]), ["from the fork"]);
    assert_eq!(
        page.data()[1].items(),
        [
            ThreadItem::user_message(
                turn_input_item_id("run-1", 0).as_str(),
                vec![UserInput::text("go")]
            ),
            ThreadItem::agent_message("answer", "settled", Some(MessagePhase::FinalAnswer)),
            ThreadItem::reasoning("thinking", vec!["why".into()], Vec::new()),
            ThreadItem::context_compaction("compacted"),
        ]
    );
}

/// What a client gets when there is no history to read, or a cursor that no longer points into
/// it, in Codex's words.
#[tokio::test]
async fn history_reads_are_refused_as_codex_refuses_them() {
    let server = Server::start(None);
    let mut connection = server.connect(1).await;
    assert_eq!(
        server
            .turns_list_error(&mut connection, ThreadTurnsListParams::new("missing"))
            .await,
        invalid_request("thread not loaded: missing")
    );

    let recorder = server.create_thread().await;
    record(&recorder, completed_run("run-1", "first", "one")).await;
    assert_eq!(
        server
            .turns_list_error(
                &mut connection,
                ThreadTurnsListParams::new(THREAD).with_cursor("junk")
            )
            .await,
        invalid_request("invalid cursor: junk")
    );
    let gone = json!({"turnId": "gone", "includeAnchor": true}).to_string();
    assert_eq!(
        server
            .turns_list_error(
                &mut connection,
                ThreadTurnsListParams::new(THREAD).with_cursor(gone)
            )
            .await,
        invalid_request("invalid cursor: anchor turn is no longer present")
    );

    // A loaded thread whose history the server cannot read is one that keeps none.
    let server = Server::start_without_store();
    let mut connection = server.connect(1).await;
    let script = Arc::new(Script::default());
    server
        .load_thread(agent_thread(&script, None), &[&connection])
        .await;
    assert_eq!(
        server
            .turns_list_error(&mut connection, ThreadTurnsListParams::new(THREAD))
            .await,
        invalid_request("ephemeral threads do not support thread/turns/list")
    );
}

/// The user message carries the input as the model received it, images included.
#[tokio::test]
async fn image_input_reads_back_as_the_input_it_was_given_as() {
    let server = Server::start(None);
    let mut connection = server.connect(1).await;
    let recorder = server.create_thread().await;
    let content = vec![
        ContentBlock::text("see"),
        ContentBlock::image(ra_core::item::ImageSource::url("https://example.com/a.png")),
        ContentBlock::image_path("/tmp/b.png"),
        ContentBlock::image_base64("image/png", "AAAA"),
    ];
    let started =
        RolloutRunStarted::new(RunId::new("run-1"), AgentId::new("lead")).with_input(vec![
            ModelInputItem::Message(Message::new(MessageRole::User, content)),
        ]);
    record(
        &recorder,
        [
            RolloutItem::RunStarted(started),
            run_ended("run-1", RolloutRunEnd::Completed),
        ],
    )
    .await;

    let page = server
        .turns_list(
            &mut connection,
            ThreadTurnsListParams::new(THREAD).with_items_view(TurnItemsView::Full),
        )
        .await;
    let ThreadItem::UserMessage { content, .. } = &page.data()[0].items()[0] else {
        panic!("expected the user's message");
    };
    assert_eq!(
        serde_json::to_value(content).unwrap(),
        json!([
            {"type": "text", "text": "see", "text_elements": []},
            {"type": "image", "url": "https://example.com/a.png", "detail": null},
            {"type": "localImage", "path": "/tmp/b.png", "detail": null},
            {"type": "image", "url": "data:image/png;base64,AAAA", "detail": null},
        ])
    );
}

// ---------------------------------------------------------------------------------------------
// The coding host's items
// ---------------------------------------------------------------------------------------------

/// The coding host shows the commands `exec_command` runs as command executions, read from the
/// tool's real results: how each exited, what it printed, and — for one still running when its
/// call returns — the session it keeps running in. Read back, they are the same.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_coding_hosts_commands_show_as_command_executions() {
    let workspace = tempfile::tempdir().unwrap();
    let host = ra_coding::CodingHost::open(workspace.path()).unwrap();
    let process_manager = Arc::clone(host.process_manager());
    let exec_command: Arc<dyn Tool> = Arc::new(
        ra_tools::exec_command::ExecCommandTool::rooted(host.workspace_root())
            .unwrap()
            .with_manager(Arc::clone(&process_manager))
            .with_policy_resolvers(
                {
                    let environment = process_manager.environment().clone();
                    Arc::new(move |_context: &ToolContext<'_>| Ok(environment.clone()))
                },
                Arc::new(|_context: &ToolContext<'_>| Ok(false)),
            ),
    );
    let server = Server::start(Some(Arc::new(host.thread_tool_items())));
    let mut connection = server.connect(1).await;
    let script = Arc::new(Script::default());
    let command = |id: &str, cmd: &str, yield_time_ms: u64| {
        ModelResponse::new(vec![RunItem::new(
            ItemId::new(id),
            RunItemKind::ToolCall(ToolCall::new(
                CallId::new(id),
                "exec_command",
                json!({"cmd": cmd, "workdir": null, "shell": null, "tty": null, "login": null,
                       "yield_time_ms": yield_time_ms, "timeout_ms": null,
                       "sandbox_permissions": null, "justification": null, "prefix_rule": null}),
            )),
        )])
    };
    script.push(Step::Respond(command("call-echo", "echo hi", 5_000)));
    script.push(Step::Respond(command("call-fail", "exit 3", 5_000)));
    script.push(Step::Respond(command(
        "call-oops",
        "echo oops; exit 2",
        5_000,
    )));
    let background_command =
        "while [ ! -f release-command ]; do sleep 0.01; done; printf late; exit 7";
    script.push(Step::Respond(command(
        "call-sleep",
        background_command,
        200,
    )));
    script.push(Step::Respond(final_message("answer-1", "Done")));
    let recorder = server.create_thread().await;
    let tool_agent = AgentSpec::builder()
        .id(AgentId::new("lead"))
        .name("Lead")
        .instructions("lead")
        .tools(vec![exec_command])
        .build()
        .unwrap();
    let thread = AgentThread::builder(
        SessionId::new(THREAD),
        AgentBinding::direct(tool_agent),
        Arc::new(Resolver(Arc::clone(&script))) as Arc<dyn ModelResolver>,
    )
    .with_rollout_recorder(recorder)
    .build();
    server.load_thread(thread, &[&connection]).await;

    let first_turn = server.turn_start(&mut connection, "run them").await;
    let notifications = connection.turn_notifications().await;
    let started: Vec<_> = notifications
        .iter()
        .filter(|(method, item)| method == "item/started" && item["type"] == "commandExecution")
        .map(|(_, item)| (item["command"].clone(), item["status"].clone()))
        .collect();
    assert_eq!(
        started,
        [
            (json!("echo hi"), json!("inProgress")),
            (json!("exit 3"), json!("inProgress")),
            (json!("echo oops; exit 2"), json!("inProgress")),
            (json!(background_command), json!("inProgress")),
        ]
    );
    let completed: Vec<_> = completed_items(&notifications)
        .into_iter()
        .filter(|item| matches!(item, ThreadItem::CommandExecution { .. }))
        .collect();
    let root = host.workspace_root().to_string_lossy().into_owned();
    let summary: Vec<_> = completed
        .iter()
        .map(|item| match item {
            ThreadItem::CommandExecution {
                id,
                cwd,
                status,
                exit_code,
                aggregated_output,
                process_id,
                ..
            } => {
                assert_eq!(cwd, &root);
                (
                    id.as_str(),
                    *status,
                    *exit_code,
                    aggregated_output.clone().unwrap_or_default(),
                    process_id.is_some(),
                )
            }
            _ => unreachable!(),
        })
        .collect();
    assert_eq!(
        summary,
        [
            (
                "call-echo",
                CommandExecutionStatus::Completed,
                Some(0),
                "hi\n".to_owned(),
                true
            ),
            (
                "call-fail",
                CommandExecutionStatus::Failed,
                Some(3),
                String::new(),
                true
            ),
            (
                "call-oops",
                CommandExecutionStatus::Failed,
                Some(2),
                "oops\n".to_owned(),
                true
            ),
        ]
    );

    let page = server
        .turns_list(
            &mut connection,
            ThreadTurnsListParams::new(THREAD).with_items_view(TurnItemsView::Full),
        )
        .await;
    let read: Vec<_> = page.data()[0]
        .items()
        .iter()
        .filter(|item| matches!(item, ThreadItem::CommandExecution { .. }))
        .cloned()
        .collect();
    assert_eq!(&read[..3], completed);
    assert!(matches!(
        &read[3],
        ThreadItem::CommandExecution {
            status: CommandExecutionStatus::InProgress,
            ..
        }
    ));

    // An exit after another turn must update the command in its original turn.
    script.push(Step::Respond(final_message("answer-2", "Next")));
    let second_turn = server.turn_start(&mut connection, "next").await;
    connection.turn_notifications().await;
    // While the command still runs, its turn stays where it was among the turns read back.
    let page = server
        .turns_list(
            &mut connection,
            ThreadTurnsListParams::new(THREAD).with_sort_direction(SortDirection::Asc),
        )
        .await;
    let ids: Vec<_> = page.data().iter().map(Turn::id).collect();
    assert_eq!(ids, [first_turn.as_str(), second_turn.as_str()]);
    std::fs::write(workspace.path().join("release-command"), "go").unwrap();
    let delta = connection
        .notification("item/commandExecution/outputDelta")
        .await;
    assert_eq!(delta["turnId"], first_turn);
    assert_eq!(delta["itemId"], "call-echo");
    assert_eq!(delta["delta"], "hi\n");
    let late_delta = connection
        .until(|v| {
            v["method"] == "item/commandExecution/outputDelta"
                && v["params"]["itemId"] == "call-sleep"
        })
        .await;
    assert_eq!(late_delta["params"]["turnId"], first_turn);
    assert_eq!(late_delta["params"]["delta"], "late");
    let exit = connection
        .until(|v| v["method"] == "item/completed" && v["params"]["item"]["id"] == "call-sleep")
        .await;
    assert_eq!(exit["params"]["turnId"], first_turn);
    assert_eq!(exit["params"]["item"]["status"], "failed");
    assert_eq!(exit["params"]["item"]["exitCode"], 7);
    assert_eq!(exit["params"]["item"]["aggregatedOutput"], "late");
    assert!(exit["params"]["item"]["durationMs"].as_i64().unwrap() >= 200);
    let page = server
        .turns_list(
            &mut connection,
            ThreadTurnsListParams::new(THREAD)
                .with_items_view(TurnItemsView::Full)
                .with_sort_direction(SortDirection::Asc),
        )
        .await;
    assert_eq!(page.data().len(), 2);
    let replayed = page.data()[0]
        .items()
        .iter()
        .find(|item| item.id() == "call-sleep")
        .unwrap();
    assert_eq!(
        serde_json::to_value(replayed).unwrap(),
        exit["params"]["item"]
    );
    assert!(
        !page.data()[1]
            .items()
            .iter()
            .any(|item| item.id() == "call-sleep")
    );
    process_manager.terminate_background_terminals().await;
}

fn stored(output: ToolOutput) -> Value {
    serde_json::to_value(output).unwrap()
}

/// The coding host shows a patch `apply_patch` applies as the files it changes: a created file
/// with its content, a changed one with the patch's hunks — moved, when the patch moves it — and a
/// deleted one; in progress while it applies, then completed or failed.
#[test]
fn the_coding_hosts_patches_show_as_file_changes() {
    let items = ra_coding::thread_items::CodingThreadToolItems::new("/work");
    let patch = "*** Begin Patch\n\
                 *** Add File: notes.txt\n\
                 +first\n\
                 +second\n\
                 *** Update File: src/lib.rs\n\
                 *** Move to: src/main.rs\n\
                 @@ fn main\n\
                 -old\n\
                 +new\n\
                 *** Delete File: gone.txt\n\
                 *** End Patch";
    let call = ToolCall::new(
        CallId::new("call-patch"),
        "apply_patch",
        json!({"patch": patch}),
    );
    let changes = json!([
        {"path": "notes.txt", "kind": {"type": "add"}, "diff": "first\nsecond\n"},
        {"path": "src/lib.rs", "kind": {"type": "update", "move_path": "src/main.rs"},
         "diff": "@@ fn main\n-old\n+new\n"},
        {"path": "gone.txt", "kind": {"type": "delete"}, "diff": ""},
    ]);
    assert_eq!(
        serde_json::to_value(items.started(&call).unwrap()).unwrap(),
        json!({"type": "fileChange", "id": "call-patch", "changes": changes, "status": "inProgress"})
    );
    let applied = ToolCallOutput::new(CallId::new("call-patch"), stored(ToolOutput::text("ok")));
    assert_eq!(
        serde_json::to_value(items.completed(&call, &applied).unwrap()).unwrap()["status"],
        "completed"
    );
    let failed = applied.clone().with_error(true);
    assert_eq!(
        serde_json::to_value(items.completed(&call, &failed).unwrap()).unwrap()["status"],
        "failed"
    );

    // A patch given as a custom tool's raw input reads the same; one that cannot be read shows
    // nothing, and neither does a tool the host does not show.
    let custom = ToolCall::custom(CallId::new("call-patch"), "apply_patch", patch);
    assert_eq!(items.started(&custom), items.started(&call));
    let unreadable = ToolCall::new(
        CallId::new("c"),
        "apply_patch",
        json!({"patch": "nonsense"}),
    );
    assert_eq!(items.started(&unreadable), None);
    let other = ToolCall::new(CallId::new("c"), "read_file", json!({"path": "a"}));
    assert_eq!(items.started(&other), None);
    assert_eq!(items.completed(&other, &applied), None);
}

/// A command runs in the directory its call names, under the workspace root; a call that failed
/// is a failed command.
#[test]
fn the_coding_hosts_commands_run_where_their_calls_say() {
    let items = ra_coding::thread_items::CodingThreadToolItems::new("/work");
    let call = ToolCall::new(
        CallId::new("call-1"),
        "exec_command",
        json!({"cmd": "make", "workdir": "sub"}),
    );
    assert_eq!(
        serde_json::to_value(items.started(&call).unwrap()).unwrap(),
        json!({"type": "commandExecution", "id": "call-1", "pluginId": null, "scriptPath": null,
               "command": "make", "cwd": "/work/sub", "processId": null, "source": "agent",
               "status": "inProgress",
               "commandActions": [{"type": "unknown", "command": "make"}],
               "aggregatedOutput": null, "exitCode": null, "durationMs": null})
    );
    let refused = ToolCallOutput::new(
        CallId::new("call-1"),
        stored(ToolOutput::text("The command was not approved.")),
    )
    .with_error(true);
    let completed = serde_json::to_value(items.completed(&call, &refused).unwrap()).unwrap();
    assert_eq!(completed["status"], "failed");
    assert_eq!(
        completed["aggregatedOutput"],
        "The command was not approved."
    );
}

#[tokio::test]
async fn spawned_runs_keep_their_turn_ids_and_input() {
    let server = Server::start(None);
    let mut connection = server.connect(1).await;
    let recorder = server.create_thread().await;
    record(
        &recorder,
        [
            RolloutItem::RunStarted(
                RolloutRunStarted::new(RunId::new("child-run"), AgentId::new("child"))
                    .with_parent_run_id(RunId::new("parent-run"))
                    .with_input(vec![ModelInputItem::Message(Message::user("child task"))]),
            ),
            agent_record("child-answer", "done", OutputPhase::Final),
            run_ended("child-run", RolloutRunEnd::Completed),
        ],
    )
    .await;
    let page = server
        .turns_list(
            &mut connection,
            ThreadTurnsListParams::new(THREAD).with_items_view(TurnItemsView::Full),
        )
        .await;
    assert_eq!(page.data()[0].id(), "child-run");
    assert_eq!(user_texts(&page.data()[0]), ["child task"]);
    assert!(page.data()[0].started_at().is_some());
}

#[tokio::test]
async fn failed_turn_keeps_leftover_input_in_same_turn() {
    let server = Server::start(None);
    let mut connection = server.connect(1).await;
    let script = Arc::new(Script::default());
    server.load(&script, &[&connection]).await;
    let gate = Arc::new(Notify::new());
    script.push(Step::FailAfter(Arc::clone(&gate)));
    let turn_id = server.turn_start(&mut connection, "first").await;
    script.wait_for_calls(1).await;
    let id = server.request(&mut connection, "turn/steer", json!({"threadId": THREAD, "expectedTurnId": turn_id, "input": [{"type":"text", "text":"also this"}]})).await;
    connection.response(id).await;
    gate.notify_one();
    connection.turn_notifications().await;
    let page = server
        .turns_list(
            &mut connection,
            ThreadTurnsListParams::new(THREAD).with_items_view(TurnItemsView::Full),
        )
        .await;
    assert_eq!(page.data().len(), 1);
    assert_eq!(page.data()[0].id(), turn_id);
    assert_eq!(page.data()[0].status(), TurnStatus::Failed);
    assert_eq!(user_texts(&page.data()[0]), ["first", "also this"]);
}

struct WaitingTool {
    origin: ToolOrigin,
    schema: ToolSchema,
    entered: Arc<Notify>,
    release: Arc<Notify>,
}

#[async_trait]
impl Tool for WaitingTool {
    fn origin(&self) -> &ToolOrigin {
        &self.origin
    }
    fn schema(&self) -> &ToolSchema {
        &self.schema
    }
    fn options(&self) -> ToolOptions {
        ToolOptions::new().with_approval(ToolApprovalPolicy::Never)
    }
    async fn call(&self, _context: ToolContext<'_>) -> Result<ToolOutput> {
        self.entered.notify_one();
        self.release.notified().await;
        Ok(ToolOutput::text("looked"))
    }
}

#[tokio::test]
async fn tool_start_is_visible_before_the_tool_returns() {
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let server = Server::start(Some(Arc::new(ShowLooks)));
    let mut connection = server.connect(1).await;
    let script = Arc::new(Script::default());
    let tool = WaitingTool {
        origin: ToolOrigin::new("quick_look").unwrap(),
        schema: ToolSchema::new(
            "quick_look",
            json!({"type":"object", "properties":{}, "required":[], "additionalProperties":false}),
        )
        .unwrap(),
        entered: Arc::clone(&entered),
        release: Arc::clone(&release),
    };
    let agent = AgentSpec::builder()
        .id(AgentId::new("lead"))
        .name("Lead")
        .instructions("lead")
        .tools(vec![Arc::new(tool) as Arc<dyn Tool>])
        .build()
        .unwrap();
    // This also exercises delivery without persistent storage.
    let thread = AgentThread::builder(
        SessionId::new(THREAD),
        AgentBinding::direct(agent),
        Arc::new(Resolver(Arc::clone(&script))) as Arc<dyn ModelResolver>,
    )
    .build();
    server.load_thread(thread, &[&connection]).await;
    script.push(Step::Respond(tool_call("call-item", "call-1")));
    script.push(Step::Respond(final_message("answer-1", "Done")));
    server.turn_start(&mut connection, "look").await;
    timeout(PATIENCE, entered.notified()).await.unwrap();
    let started = connection
        .until(|v| v["method"] == "item/started" && v["params"]["item"]["id"] == "call-1")
        .await;
    assert_eq!(started["params"]["item"]["status"], "inProgress");
    release.notify_one();
    let notifications = connection.turn_notifications().await;
    assert_eq!(
        notifications
            .iter()
            .filter(|(method, item)| method == "item/completed" && item["id"] == "call-1")
            .count(),
        1
    );
}

/// A record the runner records again once settlement changes it — here an assistant message whose
/// phase settlement decides — updates its item: it starts once and completes with what it settled
/// as, as Codex starts and completes an item once.
#[tokio::test]
async fn a_record_recorded_again_starts_its_item_once() {
    let server = Server::start(Some(Arc::new(ShowLooks)));
    let mut connection = server.connect(1).await;
    let script = Arc::new(Script::default());
    server.load(&script, &[&connection]).await;
    script.push(Step::Respond(ModelResponse::new(vec![
        RunItem::new(
            ItemId::new("note"),
            RunItemKind::Message(Message::new(
                MessageRole::Assistant,
                vec![ContentBlock::text("checking")],
            )),
        ),
        RunItem::new(
            ItemId::new("call-item"),
            RunItemKind::ToolCall(ToolCall::new(
                CallId::new("call-1"),
                "quick_look",
                json!({}),
            )),
        ),
    ])));
    script.push(Step::Respond(final_message("answer-1", "Done")));

    server.turn_start(&mut connection, "look").await;
    let notifications = connection.turn_notifications().await;
    let note: Vec<_> = notifications
        .iter()
        .filter(|(_, item)| item["id"] == "note")
        .map(|(method, item)| (method.as_str(), item["phase"].clone()))
        .collect();
    assert_eq!(
        note.first().map(|(method, _)| *method),
        Some("item/started")
    );
    assert_eq!(
        note.iter()
            .filter(|(method, _)| *method == "item/started")
            .count(),
        1,
        "{note:?}"
    );
    assert_eq!(note.last(), Some(&("item/completed", json!("commentary"))));
}

/// A model whose terminal response can be held after it has narrated the turn's items.
struct NarratingModel {
    release: Option<Arc<Notify>>,
    fail: bool,
    empty_answer: bool,
}

#[async_trait]
impl Model for NarratingModel {
    async fn get_response(&self, _: ModelRequest) -> Result<ModelResponse> {
        Err(Error::caller("this fixture uses streaming"))
    }

    fn stream_response(&self, _: ModelRequest) -> ra_core::model::ModelStream<'_> {
        use futures::{StreamExt, stream};
        use ra_core::model::{
            ModelStreamDelta, ModelStreamDeltaKind, ModelStreamEvent, RunItemStreamEvent,
        };
        let reasoning = RunItem::new(
            ItemId::new("reasoning"),
            RunItemKind::Reasoning(Reasoning::new()),
        );
        let message = RunItem::new(
            ItemId::new("answer"),
            RunItemKind::Message(Message::new(MessageRole::Assistant, vec![])),
        );
        let events = vec![
            ModelStreamEvent::OutputItemAdded(RunItemStreamEvent::new("reasoning", reasoning)),
            ModelStreamEvent::Delta(ModelStreamDelta::new(
                ItemId::new("reasoning"),
                ModelStreamDeltaKind::ReasoningSummaryPartAdded { summary_index: 1 },
            )),
            ModelStreamEvent::Delta(ModelStreamDelta::new(
                ItemId::new("reasoning"),
                ModelStreamDeltaKind::ReasoningSummaryText {
                    summary_index: 1,
                    delta: "summary".into(),
                },
            )),
            ModelStreamEvent::Delta(ModelStreamDelta::new(
                ItemId::new("reasoning"),
                ModelStreamDeltaKind::ReasoningText {
                    content_index: 2,
                    delta: "body".into(),
                },
            )),
            ModelStreamEvent::OutputItemAdded(RunItemStreamEvent::new("message", message)),
            ModelStreamEvent::Delta(ModelStreamDelta::new(
                ItemId::new("answer"),
                ModelStreamDeltaKind::OutputText {
                    delta: "Hello".into(),
                },
            )),
            ModelStreamEvent::Delta(ModelStreamDelta::new(
                ItemId::new("answer"),
                ModelStreamDeltaKind::OutputText {
                    delta: " world".into(),
                },
            )),
        ];
        let release = self.release.clone();
        let fail = self.fail;
        let answer = if self.empty_answer { "" } else { "Hello world" };
        stream::iter(events.into_iter().map(Ok))
            .chain(stream::once(async move {
                if let Some(release) = release {
                    release.notified().await;
                }
                if fail {
                    return Err(Error::caller("generation failed"));
                }
                Ok(ModelStreamEvent::Completed(Box::new(ModelResponse::new(
                    vec![
                        RunItem::new(
                            ItemId::new("reasoning"),
                            RunItemKind::Reasoning(
                                Reasoning::new()
                                    .with_summary(vec![String::new(), "summary".into()])
                                    .with_content(vec![
                                        String::new(),
                                        String::new(),
                                        "body".into(),
                                    ]),
                            ),
                        ),
                        RunItem::new(
                            ItemId::new("answer"),
                            RunItemKind::Message(Message::assistant(answer, OutputPhase::Final)),
                        ),
                    ],
                ))))
            }))
            .boxed()
    }
}

struct NarratingResolver(Arc<NarratingModel>);
impl ModelResolver for NarratingResolver {
    fn resolve_model(&self, model_name: Option<&str>) -> Result<ResolvedModel> {
        Ok(ResolvedModel::new(
            ModelSelector::new(
                ProviderKey::new("neutral"),
                model_name.map(str::to_owned),
                ApiProtocol::OpenAiResponses,
            ),
            Arc::clone(&self.0) as Arc<dyn Model>,
            ModelSettings::new(),
            ModelSettings::new(),
        ))
    }
}

/// Both immediate and suspended generation must send started, then deltas, then completed.
#[rstest::rstest]
#[case::immediate(false, false)]
#[case::suspended(true, false)]
#[case::failed(true, true)]
#[tokio::test]
async fn text_deltas_arrive_before_settlement_and_never_become_records(
    #[case] gated: bool,
    #[case] fail: bool,
) {
    let server = Server::start(None);
    let mut connection = server.connect(1).await;
    let mut outsider = server.connect(2).await;
    let release = gated.then(|| Arc::new(Notify::new()));
    let model = Arc::new(NarratingModel {
        release: release.clone(),
        fail,
        empty_answer: false,
    });
    let recorder = server.create_thread().await;
    let thread = AgentThread::builder(
        SessionId::new(THREAD),
        AgentBinding::direct(agent()),
        Arc::new(NarratingResolver(model)),
    )
    .with_rollout_recorder(recorder)
    .build();
    server.load_thread(thread.clone(), &[&connection]).await;
    let turn_id = server.turn_start(&mut connection, "go").await;
    connection.notification("turn/started").await;
    let mut seen = Vec::new();
    loop {
        let message = connection.next().await;
        let last_delta = message["method"] == "item/agentMessage/delta"
            && message["params"]["delta"] == " world";
        let completed = message["method"] == "turn/completed";
        seen.push(message);
        if last_delta && let Some(release) = &release {
            assert!(
                thread.is_running(),
                "narration is visible while generation is suspended"
            );
            let page = server
                .turns_list(
                    &mut connection,
                    ThreadTurnsListParams::new(THREAD).with_items_view(TurnItemsView::Full),
                )
                .await;
            assert_eq!(page.data()[0].status(), TurnStatus::InProgress);
            assert_eq!(
                page.data()[0].items().len(),
                1,
                "incomplete model narration is absent from history"
            );
            release.notify_one();
        }
        if completed {
            break;
        }
    }
    let deltas: Vec<_> = seen
        .iter()
        .filter(|message| {
            matches!(
                message["method"].as_str(),
                Some(
                    "item/agentMessage/delta"
                        | "item/reasoning/summaryPartAdded"
                        | "item/reasoning/summaryTextDelta"
                        | "item/reasoning/textDelta"
                )
            )
        })
        .collect();
    assert_eq!(deltas.len(), 5);
    assert_eq!(
        deltas[0]["params"],
        json!({"threadId":THREAD,"turnId":turn_id,"itemId":"reasoning","summaryIndex":1})
    );
    assert_eq!(deltas[1]["params"]["summaryIndex"], 1);
    assert_eq!(deltas[2]["params"]["contentIndex"], 2);
    assert_eq!(deltas[3]["params"]["delta"], "Hello");
    assert_eq!(deltas[4]["params"]["delta"], " world");
    for delta in &deltas {
        assert_eq!(delta["params"]["threadId"], THREAD);
        assert_eq!(delta["params"]["turnId"], turn_id);
        assert!(delta["emittedAtMs"].is_i64());
    }
    for id in ["reasoning", "answer"] {
        let starts: Vec<_> = seen
            .iter()
            .enumerate()
            .filter(|(_, m)| m["method"] == "item/started" && m["params"]["item"]["id"] == id)
            .collect();
        assert_eq!(starts.len(), 1, "an item must never start twice: {seen:?}");
        let first_delta = seen
            .iter()
            .position(|m| m["params"]["itemId"] == id)
            .unwrap();
        assert!(starts[0].0 < first_delta);
        let ends: Vec<_> = seen
            .iter()
            .enumerate()
            .filter(|(_, m)| m["method"] == "item/completed" && m["params"]["item"]["id"] == id)
            .collect();
        assert_eq!(ends.len(), usize::from(!fail));
        if !fail {
            assert!(first_delta < ends[0].0);
        }
    }
    let page = server
        .turns_list(
            &mut connection,
            ThreadTurnsListParams::new(THREAD).with_items_view(TurnItemsView::Full),
        )
        .await;
    assert_eq!(
        page.data()[0].status(),
        if fail {
            TurnStatus::Failed
        } else {
            TurnStatus::Completed
        }
    );
    assert_eq!(page.data()[0].items().len(), if fail { 1 } else { 3 });
    let request_id = server
        .request(&mut outsider, "thread/loaded/list", json!({}))
        .await;
    outsider.response(request_id).await;
    assert!(
        !outsider
            .passed
            .iter()
            .any(|m| m["method"] == "item/agentMessage/delta")
    );
}

/// An item opened during generation must complete even when its final text is empty.
#[tokio::test]
async fn an_empty_streamed_message_still_completes() {
    let server = Server::start(None);
    let mut connection = server.connect(1).await;
    let model = Arc::new(NarratingModel {
        release: None,
        fail: false,
        empty_answer: true,
    });
    let recorder = server.create_thread().await;
    let thread = AgentThread::builder(
        SessionId::new(THREAD),
        AgentBinding::direct(agent()),
        Arc::new(NarratingResolver(model)),
    )
    .with_rollout_recorder(recorder)
    .build();
    server.load_thread(thread, &[&connection]).await;
    server.turn_start(&mut connection, "go").await;
    let notifications = connection.turn_notifications().await;
    let completed = notifications
        .iter()
        .find(|(method, item)| method == "item/completed" && item["id"] == "answer")
        .expect("a started empty item completes");
    assert_eq!(completed.1["text"], "");
    assert_eq!(
        notifications
            .iter()
            .filter(|(method, item)| method == "item/started" && item["id"] == "answer")
            .count(),
        1
    );
}

/// Narrates an answer, drops the connection once, then answers on the retried request.
struct DroppedOnceModel {
    calls: std::sync::atomic::AtomicUsize,
}

#[async_trait]
impl Model for DroppedOnceModel {
    async fn get_response(&self, _: ModelRequest) -> Result<ModelResponse> {
        Err(Error::caller("this fixture uses streaming"))
    }

    fn stream_response(&self, _: ModelRequest) -> ra_core::model::ModelStream<'_> {
        use futures::{StreamExt, stream};
        use ra_core::{
            error::ProviderErrorKind,
            model::{
                ModelStreamDelta, ModelStreamDeltaKind, ModelStreamEvent, NormalizedProviderError,
                RunItemStreamEvent,
            },
        };
        let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let id = format!("answer-{call}");
        let started = RunItem::new(
            ItemId::new(id.clone()),
            RunItemKind::Message(Message::new(MessageRole::Assistant, vec![])),
        );
        let mut events = vec![
            Ok(ModelStreamEvent::OutputItemAdded(RunItemStreamEvent::new(
                "message", started,
            ))),
            Ok(ModelStreamEvent::Delta(ModelStreamDelta::new(
                ItemId::new(id.clone()),
                ModelStreamDeltaKind::OutputText {
                    delta: "Hel".into(),
                },
            ))),
        ];
        if call == 0 {
            events.push(Err(NormalizedProviderError::new(
                ProviderErrorKind::Network,
                "connection reset",
            )
            .into_error()));
        } else {
            events.push(Ok(ModelStreamEvent::Completed(Box::new(
                ModelResponse::new(vec![RunItem::new(
                    ItemId::new(id),
                    RunItemKind::Message(Message::assistant("Hello", OutputPhase::Final)),
                )]),
            ))));
        }
        stream::iter(events).boxed()
    }

    fn get_retry_advice(
        &self,
        _: &ra_core::model::ModelRetryAdviceRequest<'_>,
    ) -> Option<ra_core::model::RetryAdvice> {
        Some(
            ra_core::model::RetryAdvice::new()
                .with_replay_safety(ra_core::model::ReplaySafety::Safe),
        )
    }
}

struct DroppedOnceResolver(Arc<DroppedOnceModel>);
impl ModelResolver for DroppedOnceResolver {
    fn resolve_model(&self, model_name: Option<&str>) -> Result<ResolvedModel> {
        let retry = ra_core::model::ModelRetrySettings::new()
            .with_max_retries(1)
            .with_backoff(
                ra_core::model::RetryBackoffSettings::new()
                    .with_initial_delay(Duration::ZERO)
                    .with_jitter(false),
            )
            .with_policy(Arc::new(ra_core::model::NetworkErrorRetryPolicy));
        let settings = ModelSettings::new().with_retry(retry);
        Ok(ResolvedModel::new(
            ModelSelector::new(
                ProviderKey::new("neutral"),
                model_name.map(str::to_owned),
                ApiProtocol::OpenAiResponses,
            ),
            Arc::clone(&self.0) as Arc<dyn Model>,
            settings.clone(),
            settings,
        ))
    }
}

/// Like Codex, a thread retries a dropped stream after narrating it, and says so first.
#[rstest::rstest]
#[case::default(false)]
#[case::partial_messages(true)]
#[tokio::test]
async fn a_dropped_stream_retries_after_narration_and_reports_it(#[case] partial_messages: bool) {
    let server = Server::start(None);
    let mut connection = server.connect(1).await;
    let model = Arc::new(DroppedOnceModel {
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let recorder = server.create_thread().await;
    let thread = AgentThread::builder(
        SessionId::new(THREAD),
        AgentBinding::direct(agent()),
        Arc::new(DroppedOnceResolver(Arc::clone(&model))),
    )
    .with_config(RunConfig::new().with_partial_messages(partial_messages))
    .with_rollout_recorder(recorder)
    .build();
    server.load_thread(thread, &[&connection]).await;
    let turn_id = server.turn_start(&mut connection, "go").await;
    connection.notification("turn/started").await;
    let mut seen = Vec::new();
    loop {
        let message = connection.next().await;
        let completed = message["method"] == "turn/completed";
        seen.push(message);
        if completed {
            break;
        }
    }
    assert_eq!(model.calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    let errors: Vec<_> = seen.iter().filter(|m| m["method"] == "error").collect();
    assert_eq!(errors.len(), 1, "{seen:?}");
    let error = &errors[0]["params"];
    assert_eq!(error["willRetry"], true);
    assert_eq!(error["turnId"], turn_id);
    assert_eq!(error["error"]["message"], "Reconnecting... 1/1");
    assert!(
        error["error"]["additionalDetails"]
            .as_str()
            .is_some_and(|details| details.contains("connection reset")),
        "{error}"
    );
    let position = |predicate: &dyn Fn(&Value) -> bool| seen.iter().position(predicate).unwrap();
    let first_delta = position(&|m| m["params"]["itemId"] == "answer-0");
    let retry = position(&|m| m["method"] == "error");
    let retried_delta = position(&|m| m["params"]["itemId"] == "answer-1");
    assert!(first_delta < retry && retry < retried_delta);
    let turn = &seen.last().unwrap()["params"]["turn"];
    assert_eq!(turn["status"], "completed", "{turn}");
    assert!(turn["error"].is_null());
}

/// Wire payloads keep Codex's field names and signed numeric part indexes.
#[test]
fn incremental_notifications_match_codex_wire_shapes() {
    use ra_protocol::server::protocol::{
        AgentMessageDeltaNotification, CommandExecutionOutputDeltaNotification,
        ReasoningSummaryPartAddedNotification, ReasoningSummaryTextDeltaNotification,
        ReasoningTextDeltaNotification,
    };
    macro_rules! round_trip {
        ($ty:ty, $wire:expr $(,)?) => {{
            let wire = $wire;
            let value: $ty = serde_json::from_value(wire.clone()).unwrap();
            assert_eq!(serde_json::to_value(value).unwrap(), wire);
        }};
    }
    let text = json!({"threadId":"thread-1","turnId":"turn-1","itemId":"item-1","delta":"hello"});
    round_trip!(AgentMessageDeltaNotification, text.clone());
    round_trip!(CommandExecutionOutputDeltaNotification, text.clone());
    let mut summary = text.clone();
    summary["summaryIndex"] = json!(2);
    round_trip!(ReasoningSummaryTextDeltaNotification, summary);
    let mut content = text;
    content["contentIndex"] = json!(3);
    round_trip!(ReasoningTextDeltaNotification, content);
    round_trip!(
        ReasoningSummaryPartAddedNotification,
        json!({"threadId":"thread-1","turnId":"turn-1","itemId":"item-1","summaryIndex":2}),
    );
}
