//! The approval requests of a turn whose run stops on approvals, as Codex's app-server sends them
//! (`app-server/src/bespoke_event_handling.rs`): what is asked, how the answers settle the run, and
//! how the calls' items end, live and read back. They port the parts of Codex's `turn_start` and
//! `turn_interrupt` approval cases the server carries without a product: a product adapter here
//! asks about a test tool's calls, and the coding host's own requests are its own cases.
//!
//! The run stops once the calls it was making have settled and asks about all of them at once, as
//! the interruption contract has it; it continues once every answer is in. A cancellation aborts
//! the turn at once, and the turn's end withdraws the requests still waiting.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use ra_core::{
    agent::{AgentId, AgentSpec},
    error::{Error, Result},
    event::{ApprovalResolution, HostEvent, HostEventBody, HostEventEmitter},
    item::{
        CallId, ItemId, Message, ModelInputItem, ModelResponse, OutputPhase, RunItem, RunItemKind,
        ToolApproval, ToolCall, ToolCallOutput,
    },
    model::{
        ApiProtocol, Model, ModelRequest, ModelResolver, ModelSelector, ModelSettings, ProviderKey,
        ResolvedModel,
    },
    session::{SessionId, rollout::RolloutRecorder},
    tool::{
        Tool, ToolApprovalPolicy, ToolApprovalRequest, ToolContext, ToolOptions, ToolOrigin,
        ToolOutput, ToolSchema,
    },
};
use ra_protocol::{
    frame::{JsonRpcMessage, JsonRpcRequest, JsonRpcResponse, RequestId},
    server::{
        AppServer, PendingApproval, ReviewDecision, ThreadSettingsAdapter, ThreadToolItemEvent,
        ThreadToolItems,
        protocol::{
            ClientInfo, CommandExecutionItem, CommandExecutionRequestApprovalParams,
            CommandExecutionStatus, FileChangeRequestApprovalParams, InitializeCapabilities,
            InitializeParams, ServerRequestPayload, ThreadItem, ThreadPolicyOverrides,
            ThreadTurnsListParams, ThreadTurnsListResponse, TurnInterruptParams, TurnItemsView,
            TurnStartParams, UserInput,
        },
        test_api::ThreadLoader,
    },
    transport::{
        CHANNEL_CAPACITY, ConnectionId, ConnectionOrigin, QueuedOutgoingMessage, TransportEvent,
        test_api::connection_opened,
    },
};
use ra_runtime::{agent::AgentBinding, runner::RunConfig, thread::AgentThread};
use ra_session::{
    RolloutSessionMeta,
    rollout::RolloutPayload,
    store::{CreateThreadParams, InMemoryThreadStore, ReadThreadParams, ThreadStore},
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
// A scripted model
// ---------------------------------------------------------------------------------------------

#[derive(Default)]
struct Script {
    responses: Mutex<VecDeque<ModelResponse>>,
    inputs: Mutex<Vec<Vec<ModelInputItem>>>,
    called: Notify,
    response_gate: Mutex<Option<Arc<Notify>>>,
}

impl Script {
    fn push(&self, response: ModelResponse) {
        self.responses.lock().unwrap().push_back(response);
    }

    fn calls(&self) -> usize {
        self.inputs.lock().unwrap().len()
    }

    fn gate_next_response(&self) -> Arc<Notify> {
        let gate = Arc::new(Notify::new());
        *self.response_gate.lock().unwrap() = Some(Arc::clone(&gate));
        gate
    }

    async fn wait_for_calls(&self, count: usize) {
        timeout(PATIENCE, async {
            loop {
                let notified = self.called.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if self.calls() >= count {
                    return;
                }
                notified.await;
            }
        })
        .await
        .expect("the model is called");
    }

    /// The text of the outputs of `call_id` the model was given on its last call.
    fn last_output(&self, call_id: &str) -> Option<String> {
        let inputs = self.inputs.lock().unwrap();
        inputs.last()?.iter().find_map(|item| match item {
            ModelInputItem::ToolCallOutput(output) if output.call_id().as_str() == call_id => {
                Some(output_text(output))
            }
            _ => None,
        })
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
    async fn get_response(&self, request: ModelRequest) -> Result<ModelResponse> {
        self.0.inputs.lock().unwrap().push(request.input().to_vec());
        self.0.called.notify_waiters();
        let gate = self.0.response_gate.lock().unwrap().take();
        if let Some(gate) = gate {
            gate.notified().await;
        }
        self.0
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| Error::caller("the model ran out of responses"))
    }
}

fn output_text(output: &ToolCallOutput) -> String {
    ToolOutput::from_stored(output.output())
        .ok()
        .flatten()
        .and_then(|output| output.as_text().map(str::to_owned))
        .unwrap_or_else(|| match output.output() {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        })
}

fn final_message(id: &str, text: &str) -> ModelResponse {
    ModelResponse::new(vec![RunItem::new(
        ItemId::new(id),
        RunItemKind::Message(Message::assistant(text, OutputPhase::Final)),
    )])
}

/// One response calling `tool` once for each of `call_ids`.
fn calls(tool: &str, call_ids: &[&str]) -> ModelResponse {
    calls_of(&call_ids.iter().map(|id| (tool, *id)).collect::<Vec<_>>())
}

fn calls_of(calls: &[(&str, &str)]) -> ModelResponse {
    ModelResponse::new(
        calls
            .iter()
            .map(|(tool, call_id)| {
                RunItem::new(
                    ItemId::new(format!("item-{call_id}")),
                    RunItemKind::ToolCall(ToolCall::new(CallId::new(*call_id), *tool, json!({}))),
                )
            })
            .collect(),
    )
}

// ---------------------------------------------------------------------------------------------
// Tools that ask first, and a product that asks the client about them
// ---------------------------------------------------------------------------------------------

/// A tool every call of which asks first. `asks_again` makes an approved call ask once more from
/// inside its invocation before it runs.
struct GuardedTool {
    origin: ToolOrigin,
    schema: ToolSchema,
    asks_again: bool,
    runs: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl Tool for GuardedTool {
    fn origin(&self) -> &ToolOrigin {
        &self.origin
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    fn options(&self) -> ToolOptions {
        ToolOptions::new().with_approval(ToolApprovalPolicy::Always)
    }

    async fn call(&self, context: ToolContext<'_>) -> Result<ToolOutput> {
        if self.asks_again && context.resume_state().is_none() {
            return Err(
                ToolApprovalRequest::new(json!({"stage": "again"})).into_error(self.origin.name())
            );
        }
        self.runs
            .lock()
            .unwrap()
            .push(context.call_id().as_str().to_owned());
        // Reports that it ran, as a tool's runtime reports what it does, into the run's rollout.
        if let Some(emitter) = context.event_emitter() {
            emitter.emit(HostEventBody::Unknown {
                family: "ran".to_owned(),
                data: json!(context.call_id().as_str()),
            })?;
        }
        Ok(ToolOutput::text("ran"))
    }
}

/// Asks about `run` and `again` as commands and `edit` as file changes; asks nothing for the
/// other tools. Keeps the decisions it is told of.
#[derive(Default)]
struct Product {
    decisions: Mutex<Vec<(String, ReviewDecision)>>,
    request_models: Mutex<Vec<Option<String>>>,
    decision_models: Mutex<Vec<Option<String>>>,
    decided: Notify,
    decision_gate: Mutex<Option<Arc<Notify>>>,
}

impl Product {
    async fn wait_for_started_decision(&self) {
        timeout(PATIENCE, async {
            loop {
                let notified = self.decided.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if !self.decision_models.lock().unwrap().is_empty() {
                    return;
                }
                notified.await;
            }
        })
        .await
        .expect("the product starts applying the answer");
    }

    async fn wait_for_decisions(&self, count: usize) {
        timeout(PATIENCE, async {
            loop {
                let notified = self.decided.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if self.decisions.lock().unwrap().len() >= count {
                    return;
                }
                notified.await;
            }
        })
        .await
        .expect("the product receives the answer");
    }
}

#[async_trait]
impl ThreadSettingsAdapter for Product {
    fn prepare(
        &self,
        current: &RunConfig,
        _update: &ThreadPolicyOverrides,
    ) -> std::result::Result<RunConfig, String> {
        Ok(current.clone())
    }

    fn describe(&self, _config: &RunConfig) -> Value {
        json!({})
    }

    fn approval_request(
        &self,
        config: &RunConfig,
        pending: PendingApproval<'_>,
    ) -> Option<ServerRequestPayload> {
        self.request_models
            .lock()
            .unwrap()
            .push(config.model().map(str::to_owned));
        let approval = pending.approval();
        let item_id = approval.call_id().as_str();
        match approval.tool_name() {
            "run" | "again" => Some(ServerRequestPayload::CommandExecutionRequestApproval(
                CommandExecutionRequestApprovalParams::new(
                    pending.thread_id(),
                    pending.turn_id(),
                    item_id,
                    pending.started_at_ms(),
                )
                .with_command(approval.tool_name()),
            )),
            "edit" => Some(ServerRequestPayload::FileChangeRequestApproval(
                FileChangeRequestApprovalParams::new(
                    pending.thread_id(),
                    pending.turn_id(),
                    item_id,
                    pending.started_at_ms(),
                ),
            )),
            _ => None,
        }
    }

    async fn apply_review_decision(
        &self,
        config: &RunConfig,
        approval: &ToolApproval,
        decision: &ReviewDecision,
    ) {
        self.decision_models
            .lock()
            .unwrap()
            .push(config.model().map(str::to_owned));
        self.decided.notify_waiters();
        let gate = self.decision_gate.lock().unwrap().take();
        if let Some(gate) = gate {
            gate.notified().await;
        }
        self.decisions
            .lock()
            .unwrap()
            .push((approval.call_id().as_str().to_owned(), decision.clone()));
        self.decided.notify_waiters();
    }
}

/// Shows every call as a command, which ends as its approval's answer says when the call was not
/// approved.
#[derive(Debug)]
struct ShowCalls;

fn command(call_id: &str, status: CommandExecutionStatus) -> ThreadItem {
    CommandExecutionItem::new(call_id, "tool", "/work", status).into()
}

impl ThreadToolItems for ShowCalls {
    fn started(&self, call: &ToolCall) -> Option<ThreadItem> {
        Some(command(
            call.call_id().as_str(),
            CommandExecutionStatus::InProgress,
        ))
    }

    fn completed(&self, call: &ToolCall, output: &ToolCallOutput) -> Option<ThreadItem> {
        Some(command(
            call.call_id().as_str(),
            if output.is_error() {
                CommandExecutionStatus::Failed
            } else {
                CommandExecutionStatus::Completed
            },
        ))
    }

    fn event(&self, event: &HostEvent, _items: &[ThreadItem]) -> Option<ThreadToolItemEvent> {
        let HostEventBody::Approval(approval) = event.body() else {
            return None;
        };
        let status = match approval.resolution() {
            ApprovalResolution::Declined | ApprovalResolution::Cancelled => {
                CommandExecutionStatus::Declined
            }
            ApprovalResolution::Failed => CommandExecutionStatus::Failed,
            _ => return None,
        };
        Some(ThreadToolItemEvent::Completed(command(
            approval.call_id().as_str(),
            status,
        )))
    }
}

struct Thread {
    runs: Arc<Mutex<Vec<String>>>,
    background_emitter: Arc<Mutex<Option<HostEventEmitter>>>,
}

/// Retains the emitter as a yielded command's exit watcher does after its tool call returns.
struct BackgroundTool {
    origin: ToolOrigin,
    schema: ToolSchema,
    emitter: Arc<Mutex<Option<HostEventEmitter>>>,
}

#[async_trait]
impl Tool for BackgroundTool {
    fn origin(&self) -> &ToolOrigin {
        &self.origin
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    fn options(&self) -> ToolOptions {
        ToolOptions::new().with_approval(ToolApprovalPolicy::Never)
    }

    async fn call(&self, context: ToolContext<'_>) -> Result<ToolOutput> {
        *self.emitter.lock().unwrap() = context.event_emitter();
        Ok(ToolOutput::text("background started"))
    }
}

impl Thread {
    fn runs(&self) -> Vec<String> {
        self.runs.lock().unwrap().clone()
    }
}

fn tool(name: &str, asks_again: bool, runs: &Arc<Mutex<Vec<String>>>) -> Arc<dyn Tool> {
    Arc::new(GuardedTool {
        origin: ToolOrigin::new(name).unwrap(),
        schema: ToolSchema::new(
            name,
            json!({"type": "object", "properties": {}, "required": [], "additionalProperties": false}),
        )
        .unwrap(),
        asks_again,
        runs: Arc::clone(runs),
    })
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
    fn start() -> Self {
        let store = Arc::new(InMemoryThreadStore::new());
        let server = AppServer::new(std::env::temp_dir().join("rusty-agent-approvals-test"))
            .unwrap()
            .with_thread_store(Arc::clone(&store) as Arc<dyn ThreadStore>)
            .with_thread_tool_items(Arc::new(ShowCalls));
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

    /// Loads a thread on `script` recording into the store, asking through `product` if any.
    async fn load(
        &self,
        script: &Arc<Script>,
        product: Option<Arc<Product>>,
        connection: &Connection,
    ) -> Thread {
        let recorder: Arc<dyn RolloutRecorder> = self
            .store
            .create_thread_with(&CreateThreadParams::new(RolloutSessionMeta::new(
                SessionId::new(THREAD),
            )))
            .await
            .unwrap();
        let runs = Arc::new(Mutex::new(Vec::new()));
        let background_emitter = Arc::new(Mutex::new(None));
        let agent = AgentSpec::builder()
            .id(AgentId::new("lead"))
            .name("Lead")
            .instructions("lead")
            .tools(vec![
                tool("run", false, &runs),
                tool("edit", false, &runs),
                tool("again", true, &runs),
                tool("unasked", false, &runs),
                Arc::new(BackgroundTool {
                    origin: ToolOrigin::new("background").unwrap(),
                    schema: ToolSchema::new(
                        "background",
                        json!({"type": "object", "properties": {}, "required": [], "additionalProperties": false}),
                    )
                    .unwrap(),
                    emitter: Arc::clone(&background_emitter),
                }),
            ])
            .build()
            .unwrap();
        let thread = AgentThread::builder(
            SessionId::new(THREAD),
            AgentBinding::direct(agent),
            Arc::new(Resolver(Arc::clone(script))) as Arc<dyn ModelResolver>,
        )
        .with_config(RunConfig::new())
        .with_step_model_switching(true)
        .with_rollout_recorder(recorder)
        .build();
        self.loader
            .load_with_adapter(
                thread,
                &[connection.id],
                product.map(|product| product as Arc<dyn ThreadSettingsAdapter>),
            )
            .await;
        Thread {
            runs,
            background_emitter,
        }
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

    /// Starts a turn and returns its id.
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

    /// Answers the server's request `request` with `result`.
    async fn answer(&self, connection: &Connection, request: &Value, result: Value) {
        let id: RequestId = serde_json::from_value(request["id"].clone()).unwrap();
        self.send(connection, JsonRpcResponse::new(id, result))
            .await;
    }

    /// The host events the thread's rollout holds, in order.
    async fn recorded_events(&self) -> Vec<HostEvent> {
        let stored = self
            .store
            .read_thread(&ReadThreadParams::new(SessionId::new(THREAD)).with_history())
            .await
            .unwrap();
        stored
            .history()
            .unwrap()
            .records()
            .iter()
            .filter_map(|record| match record.payload().unwrap() {
                RolloutPayload::Event(event) => Some(event),
                _ => None,
            })
            .collect()
    }

    /// The items of the thread's turns, read back from its rollout.
    async fn items_read_back(&self, connection: &mut Connection) -> Vec<Vec<ThreadItem>> {
        let id = self
            .request(
                connection,
                "thread/turns/list",
                serde_json::to_value(
                    ThreadTurnsListParams::new(THREAD).with_items_view(TurnItemsView::Full),
                )
                .unwrap(),
            )
            .await;
        let page: ThreadTurnsListResponse =
            serde_json::from_value(connection.response(id).await).unwrap();
        let mut turns: Vec<_> = page
            .data()
            .iter()
            .map(|turn| turn.items().to_vec())
            .collect();
        turns.reverse();
        turns
    }
}

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

    async fn notification(&mut self, method: &str) -> Value {
        self.until(|message| message["method"] == method && message.get("id").is_none())
            .await["params"]
            .clone()
    }

    /// The next request the server sends, of any method.
    async fn server_request(&mut self) -> Value {
        self.until(|message| message.get("id").is_some() && message.get("method").is_some())
            .await
    }

    /// The `item/completed` of `call_id`'s item.
    async fn completed_item(&mut self, call_id: &str) -> Value {
        self.until(|message| {
            message["method"] == "item/completed" && message["params"]["item"]["id"] == call_id
        })
        .await["params"]["item"]
            .clone()
    }

    /// How many messages read past so far `matches` accepts.
    fn passed_count(&self, matches: impl Fn(&Value) -> bool) -> usize {
        self.passed
            .iter()
            .filter(|message| matches(message))
            .count()
    }
}

fn status_of(items: &[ThreadItem], call_id: &str) -> Option<CommandExecutionStatus> {
    items.iter().find_map(|item| match item {
        ThreadItem::CommandExecution { id, status, .. } if id == call_id => Some(*status),
        _ => None,
    })
}

fn waiting_on_approval(notification: &Value) -> bool {
    notification["status"]["activeFlags"]
        .as_array()
        .is_some_and(|flags| flags.contains(&json!("waitingOnApproval")))
}

// ---------------------------------------------------------------------------------------------
// Cases
// ---------------------------------------------------------------------------------------------

/// Codex's `turn_start_exec_approval_toggle_v2`: the call's approval is asked of the client for
/// its item in its turn, the thread waits on approval meanwhile, and once the client accepts, the
/// request is resolved, the product is told the decision, and the call runs.
#[tokio::test]
async fn an_accepted_call_runs_once_its_request_is_answered() {
    let server = Server::start();
    let mut connection = server.connect(1).await;
    let script = Arc::new(Script::default());
    let product = Arc::new(Product::default());
    let thread = server
        .load(&script, Some(Arc::clone(&product)), &connection)
        .await;
    script.push(calls("run", &["call-1"]));
    script.push(final_message("answer", "done"));

    let turn_id = server.turn_start(&mut connection, "run it").await;
    let request = connection.server_request().await;
    assert_eq!(request["method"], "item/commandExecution/requestApproval");
    assert_eq!(request["params"]["threadId"], THREAD);
    assert_eq!(request["params"]["turnId"], turn_id.as_str());
    assert_eq!(request["params"]["itemId"], "call-1");
    let status = connection
        .until(|message| {
            message["method"] == "thread/status/changed" && waiting_on_approval(&message["params"])
        })
        .await;
    assert_eq!(status["params"]["threadId"], THREAD);
    assert!(thread.runs().is_empty());

    server
        .answer(&connection, &request, json!({"decision": "accept"}))
        .await;
    let resolved = connection.notification("serverRequest/resolved").await;
    assert_eq!(resolved["requestId"], request["id"]);
    let item = connection.completed_item("call-1").await;
    assert_eq!(item["status"], "completed");
    connection.notification("turn/completed").await;

    assert_eq!(thread.runs(), ["call-1"]);
    assert_eq!(script.last_output("call-1").as_deref(), Some("ran"));
    assert_eq!(
        *product.decisions.lock().unwrap(),
        [("call-1".to_owned(), ReviewDecision::Approved)]
    );
    assert!(
        connection
            .passed
            .iter()
            .filter(|message| message["method"] == "thread/status/changed")
            .any(|message| !waiting_on_approval(&message["params"])),
        "the thread stops waiting on approval once answered"
    );

    // The answer is recorded in the run's rollout before the call it settled runs, and the
    // resumed run numbers its own events after it.
    let events = server.recorded_events().await;
    let run_events: Vec<_> = events
        .iter()
        .filter(|event| event.run_id().as_str() == turn_id)
        .collect();
    let answered = run_events
        .iter()
        .position(|event| matches!(event.body(), HostEventBody::Approval(_)))
        .expect("the answer is recorded");
    let ran = run_events
        .iter()
        .position(|event| matches!(event.body(), HostEventBody::Unknown { family, .. } if family == "ran"))
        .expect("the call's run is recorded");
    assert!(answered < ran);
    assert!(run_events[answered].seq() < run_events[ran].seq());
}

/// Codex's `turn_start_exec_approval_decline_v2` and `_invalid_response_v2`: a declined call does
/// not run, the model is told Codex's rejection, and its item ends declined — or failed when no
/// answer could be read — once, live and read back.
#[tokio::test]
async fn a_refused_call_ends_as_its_answer_says_live_and_read_back() {
    for (answer, status, rejection) in [
        (
            json!({"decision": "decline"}),
            CommandExecutionStatus::Declined,
            "rejected by user",
        ),
        (
            json!({"unexpected": "response"}),
            CommandExecutionStatus::Failed,
            "approval request failed",
        ),
    ] {
        let server = Server::start();
        let mut connection = server.connect(1).await;
        let script = Arc::new(Script::default());
        let thread = server
            .load(&script, Some(Arc::default()), &connection)
            .await;
        script.push(calls("run", &["call-1"]));
        script.push(final_message("answer", "done"));

        server.turn_start(&mut connection, "run it").await;
        let request = connection.server_request().await;
        server.answer(&connection, &request, answer).await;
        let item = connection.completed_item("call-1").await;
        assert_eq!(
            serde_json::from_value::<CommandExecutionStatus>(item["status"].clone()).unwrap(),
            status
        );
        connection.notification("turn/completed").await;

        assert!(thread.runs().is_empty());
        assert_eq!(script.last_output("call-1").as_deref(), Some(rejection));
        assert_eq!(
            connection.passed_count(|message| {
                message["method"] == "item/completed" && message["params"]["item"]["id"] == "call-1"
            }),
            0,
            "the call's item completes once"
        );
        let turns = server.items_read_back(&mut connection).await;
        assert_eq!(status_of(&turns[0], "call-1"), Some(status));
    }
}

/// Every call the run stopped on is asked at once, a command and a file change each by its own
/// method, and the run continues only once every answer is in, whatever order they come in.
#[tokio::test]
async fn every_answer_is_settled_before_the_run_continues() {
    let server = Server::start();
    let mut connection = server.connect(1).await;
    let script = Arc::new(Script::default());
    let product = Arc::new(Product::default());
    let thread = server
        .load(&script, Some(Arc::clone(&product)), &connection)
        .await;
    script.push(calls_of(&[("run", "call-1"), ("edit", "call-2")]));
    script.push(final_message("answer", "done"));

    server.turn_start(&mut connection, "two").await;
    let first = connection.server_request().await;
    let second = connection.server_request().await;
    let (command, file) = if first["method"] == "item/commandExecution/requestApproval" {
        (first, second)
    } else {
        (second, first)
    };
    assert_eq!(command["params"]["itemId"], "call-1");
    assert_eq!(file["method"], "item/fileChange/requestApproval");
    assert_eq!(file["params"]["itemId"], "call-2");

    server
        .answer(&connection, &file, json!({"decision": "accept"}))
        .await;
    connection.notification("serverRequest/resolved").await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(thread.runs().is_empty(), "the run waits for every answer");
    assert_eq!(script.calls(), 1);

    server
        .answer(&connection, &command, json!({"decision": "decline"}))
        .await;
    connection.notification("turn/completed").await;
    assert_eq!(thread.runs(), ["call-2"]);
    assert_eq!(
        script.last_output("call-1").as_deref(),
        Some("rejected by user")
    );
    assert_eq!(script.last_output("call-2").as_deref(), Some("ran"));
    let mut decisions = product.decisions.lock().unwrap().clone();
    decisions.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        decisions,
        [
            (
                "call-1".to_owned(),
                ReviewDecision::denied("rejected by user")
            ),
            ("call-2".to_owned(), ReviewDecision::Approved),
        ]
    );
}

/// Codex's cancellation: the turn is aborted as soon as the client cancels, the requests still
/// waiting are withdrawn, and the cancelled call's item ends declined, live and read back. A late
/// answer to a withdrawn request settles nothing, in this turn or the next.
#[tokio::test]
async fn a_cancel_aborts_the_turn_and_withdraws_the_other_requests() {
    let server = Server::start();
    let mut connection = server.connect(1).await;
    let script = Arc::new(Script::default());
    let thread = server
        .load(&script, Some(Arc::default()), &connection)
        .await;
    script.push(calls("run", &["call-1", "call-2", "call-3"]));

    server.turn_start(&mut connection, "three").await;
    let mut requests = Vec::new();
    for _ in 0..3 {
        requests.push(connection.server_request().await);
    }
    requests.sort_by_key(|request| request["params"]["itemId"].as_str().unwrap().to_owned());
    let cancelled = requests.remove(0);
    server
        .answer(&connection, &cancelled, json!({"decision": "cancel"}))
        .await;
    let item = connection.completed_item("call-1").await;
    assert_eq!(item["status"], "declined");
    let turn = connection.notification("turn/completed").await;
    assert_eq!(turn["turn"]["status"], "interrupted");
    for withdrawn in &requests {
        let resolved = connection
            .until(|message| {
                message["method"] == "serverRequest/resolved"
                    && message["params"]["requestId"] == withdrawn["id"]
            })
            .await;
        assert_eq!(resolved["params"]["threadId"], THREAD);
    }
    let withdrawn = requests.remove(0);
    assert!(thread.runs().is_empty());

    // The next turn stops on a call of its own; the withdrawn request's late answer settles none.
    script.push(calls("run", &["call-4"]));
    script.push(final_message("answer", "done"));
    server.turn_start(&mut connection, "again").await;
    let next = connection.server_request().await;
    assert_eq!(next["params"]["itemId"], "call-4");
    server
        .answer(&connection, &withdrawn, json!({"decision": "accept"}))
        .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(thread.runs().is_empty());
    server
        .answer(&connection, &next, json!({"decision": "accept"}))
        .await;
    connection.notification("turn/completed").await;
    assert_eq!(thread.runs(), ["call-4"]);

    let turns = server.items_read_back(&mut connection).await;
    assert_eq!(
        status_of(&turns[0], "call-1"),
        Some(CommandExecutionStatus::Declined)
    );
}

/// Codex's `turn_interrupt` with an approval waiting: the listener goes on handling the thread's
/// events while answers wait, so an interrupt ends the turn and withdraws the request.
#[tokio::test]
async fn the_listener_keeps_working_while_answers_wait() {
    let server = Server::start();
    let mut connection = server.connect(1).await;
    let script = Arc::new(Script::default());
    let thread = server
        .load(&script, Some(Arc::default()), &connection)
        .await;
    script.push(calls("run", &["call-1"]));

    let turn_id = server.turn_start(&mut connection, "run it").await;
    let request = connection.server_request().await;
    let interrupt = server
        .request(
            &mut connection,
            "turn/interrupt",
            serde_json::to_value(TurnInterruptParams::new(THREAD, turn_id)).unwrap(),
        )
        .await;
    connection.response(interrupt).await;
    let resolved = connection.notification("serverRequest/resolved").await;
    assert_eq!(resolved["requestId"], request["id"]);
    let turn = connection.notification("turn/completed").await;
    assert_eq!(turn["turn"]["status"], "interrupted");
    assert!(thread.runs().is_empty());
}

/// A call that asks again from inside its invocation once approved is asked about in a new
/// request, and runs once that is answered too.
#[tokio::test]
async fn a_call_that_asks_again_is_asked_in_a_new_request() {
    let server = Server::start();
    let mut connection = server.connect(1).await;
    let script = Arc::new(Script::default());
    let thread = server
        .load(&script, Some(Arc::default()), &connection)
        .await;
    script.push(calls("again", &["call-1"]));
    script.push(final_message("answer", "done"));

    server.turn_start(&mut connection, "run it").await;
    let first = connection.server_request().await;
    server
        .answer(&connection, &first, json!({"decision": "accept"}))
        .await;
    let second = connection.server_request().await;
    assert_ne!(second["id"], first["id"]);
    assert_eq!(second["params"]["itemId"], "call-1");
    assert!(thread.runs().is_empty());
    server
        .answer(&connection, &second, json!({"decision": "accept"}))
        .await;
    connection.notification("turn/completed").await;
    assert_eq!(thread.runs(), ["call-1"]);
    assert_eq!(script.last_output("call-1").as_deref(), Some("ran"));
}

/// A call the product asks nothing for, and every call of a thread loaded without a product, is
/// refused as Codex refuses a call whose approval could not be obtained, without a request.
#[tokio::test]
async fn a_call_no_one_is_asked_about_is_refused() {
    for (product, tool) in [(Some(Arc::default()), "unasked"), (None, "run")] {
        let server = Server::start();
        let mut connection = server.connect(1).await;
        let script = Arc::new(Script::default());
        let thread = server.load(&script, product, &connection).await;
        script.push(calls(tool, &["call-1"]));
        script.push(final_message("answer", "done"));

        server.turn_start(&mut connection, "run it").await;
        let item = connection.completed_item("call-1").await;
        assert_eq!(item["status"], "failed");
        connection.notification("turn/completed").await;
        assert!(thread.runs().is_empty());
        assert_eq!(
            script.last_output("call-1").as_deref(),
            Some("approval request failed")
        );
        assert_eq!(
            connection.passed_count(|message| {
                message.get("id").is_some() && message.get("method").is_some()
            }),
            0,
            "nothing is asked"
        );
    }
}

/// A refusal completes immediately, even while another request is unanswered. Cancelling that
/// other request must not erase the first answer from either live items or the rollout.
#[tokio::test]
async fn a_decline_is_kept_when_another_request_cancels_the_turn() {
    let server = Server::start();
    let mut connection = server.connect(1).await;
    let script = Arc::new(Script::default());
    let thread = server
        .load(&script, Some(Arc::default()), &connection)
        .await;
    script.push(calls("run", &["call-1", "call-2"]));
    server.turn_start(&mut connection, "two").await;
    let first = connection.server_request().await;
    let second = connection.server_request().await;
    let first_id = first["params"]["itemId"].as_str().unwrap();
    server
        .answer(&connection, &first, json!({"decision": "decline"}))
        .await;
    assert_eq!(
        connection.completed_item(first_id).await["status"],
        "declined"
    );
    assert!(thread.runs().is_empty());
    server
        .answer(&connection, &second, json!({"decision": "cancel"}))
        .await;
    connection.notification("turn/completed").await;
    let turns = server.items_read_back(&mut connection).await;
    assert_eq!(
        status_of(&turns[0], first_id),
        Some(CommandExecutionStatus::Declined)
    );
    let events = server.recorded_events().await;
    assert!(events.iter().any(|event| matches!(event.body(), HostEventBody::Approval(approval)
        if approval.call_id().as_str() == first_id && approval.resolution() == ApprovalResolution::Declined)));
}

/// Product effects such as session caching are applied before the batch can be cancelled, by
/// another answer or by an explicit interrupt. An unanswered call never runs meanwhile.
#[tokio::test]
async fn a_session_answer_reaches_the_product_before_cancel_or_interrupt() {
    for interrupt in [false, true] {
        let server = Server::start();
        let mut connection = server.connect(1).await;
        let script = Arc::new(Script::default());
        let product = Arc::new(Product::default());
        let thread = server
            .load(&script, Some(Arc::clone(&product)), &connection)
            .await;
        script.push(calls("run", &["call-1", "call-2"]));
        let turn_id = server.turn_start(&mut connection, "two").await;
        let first = connection.server_request().await;
        let second = connection.server_request().await;
        server
            .answer(&connection, &first, json!({"decision": "acceptForSession"}))
            .await;
        product.wait_for_decisions(1).await;
        assert_eq!(
            *product.decisions.lock().unwrap(),
            [(
                first["params"]["itemId"].as_str().unwrap().to_owned(),
                ReviewDecision::ApprovedForSession,
            )]
        );
        assert!(thread.runs().is_empty());
        if interrupt {
            let id = server
                .request(
                    &mut connection,
                    "turn/interrupt",
                    serde_json::to_value(TurnInterruptParams::new(THREAD, turn_id)).unwrap(),
                )
                .await;
            connection.response(id).await;
        } else {
            server
                .answer(&connection, &second, json!({"decision": "cancel"}))
                .await;
        }
        connection.notification("turn/completed").await;
        let events = server.recorded_events().await;
        assert!(events.iter().any(
            |event| matches!(event.body(), HostEventBody::Approval(approval)
            if approval.call_id().as_str() == first["params"]["itemId"].as_str().unwrap()
                && approval.resolution() == ApprovalResolution::Approved)
        ));
        assert_eq!(product.decisions.lock().unwrap().len(), 1);
    }
}

/// A retained emitter can report while the run waits, after it resumes, and after the turn ends.
/// All segments and approval events must draw unique sequence numbers from that same run.
#[tokio::test]
async fn approvals_and_background_events_share_sequences_across_repeated_stops() {
    let server = Server::start();
    let mut connection = server.connect(1).await;
    let script = Arc::new(Script::default());
    let thread = server
        .load(&script, Some(Arc::default()), &connection)
        .await;
    script.push(calls_of(&[("background", "bg"), ("again", "call-1")]));
    script.push(final_message("answer", "done"));
    server.turn_start(&mut connection, "run it").await;
    let first = connection.server_request().await;
    let emitter = thread.background_emitter.lock().unwrap().clone().unwrap();
    emitter
        .emit(HostEventBody::Unknown {
            family: "background".to_owned(),
            data: json!("first pause"),
        })
        .unwrap();
    server
        .answer(&connection, &first, json!({"decision": "accept"}))
        .await;
    let second = connection.server_request().await;
    emitter
        .emit(HostEventBody::Unknown {
            family: "background".to_owned(),
            data: json!("second pause"),
        })
        .unwrap();
    server
        .answer(&connection, &second, json!({"decision": "accept"}))
        .await;
    connection.notification("turn/completed").await;
    emitter
        .emit(HostEventBody::Unknown {
            family: "background".to_owned(),
            data: json!("after turn"),
        })
        .unwrap();
    let events = server.recorded_events().await;
    let mut seen = std::collections::HashSet::new();
    for event in &events {
        assert!(
            seen.insert((event.run_id().clone(), event.seq())),
            "duplicate event sequence: {events:?}"
        );
    }
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event.body(), HostEventBody::Approval(_)))
            .count(),
        2
    );
    assert_eq!(events.iter().filter(|event| matches!(event.body(), HostEventBody::Unknown { family, .. } if family == "background")).count(), 3);
    assert!(events.windows(2).all(|pair| pair[0].seq() < pair[1].seq()));
}

/// Settings published while a model request is in flight belong to subsequent steps. Approval
/// request construction and product callbacks retain the configuration that issued the call.
#[tokio::test]
async fn approval_requests_and_answers_keep_the_issuing_step_settings() {
    let server = Server::start();
    let mut connection = server.connect(1).await;
    let script = Arc::new(Script::default());
    let product = Arc::new(Product::default());
    server
        .load(&script, Some(Arc::clone(&product)), &connection)
        .await;
    let gate = script.gate_next_response();
    script.push(calls("run", &["call-1"]));
    script.push(final_message("answer", "done"));
    let turn_id = server.turn_start(&mut connection, "run it").await;
    script.wait_for_calls(1).await;
    let id = server
        .request(
            &mut connection,
            "turn/settings/update",
            json!({
                "threadId": THREAD, "turnId": turn_id, "model": "future"
            }),
        )
        .await;
    assert_eq!(connection.response(id).await, json!({"status": "applied"}));
    gate.notify_one();
    let request = connection.server_request().await;
    assert_eq!(*product.request_models.lock().unwrap(), [None]);
    let id = server
        .request(
            &mut connection,
            "turn/settings/update",
            json!({
                "threadId": THREAD, "turnId": turn_id, "model": "later"
            }),
        )
        .await;
    assert_eq!(connection.response(id).await, json!({"status": "applied"}));
    server
        .answer(&connection, &request, json!({"decision": "accept"}))
        .await;
    connection.notification("turn/completed").await;
    assert_eq!(*product.decision_models.lock().unwrap(), [None]);
}

/// Persisting an approved amendment may wait on storage, but a different request's cancellation
/// must still abort the turn before that write finishes. The accepted effect is allowed to finish.
#[tokio::test]
async fn cancelling_does_not_wait_for_a_products_pending_effect() {
    let server = Server::start();
    let mut connection = server.connect(1).await;
    let script = Arc::new(Script::default());
    let product = Arc::new(Product::default());
    let gate = Arc::new(Notify::new());
    *product.decision_gate.lock().unwrap() = Some(Arc::clone(&gate));
    let thread = server
        .load(&script, Some(Arc::clone(&product)), &connection)
        .await;
    script.push(calls("run", &["call-1", "call-2"]));
    server.turn_start(&mut connection, "two").await;
    let first = connection.server_request().await;
    let second = connection.server_request().await;
    server
        .answer(&connection, &first, json!({"decision": "acceptForSession"}))
        .await;
    product.wait_for_started_decision().await;
    server
        .answer(&connection, &second, json!({"decision": "cancel"}))
        .await;
    let turn = connection.notification("turn/completed").await;
    assert_eq!(turn["turn"]["status"], "interrupted");
    assert!(thread.runs().is_empty());
    assert!(product.decisions.lock().unwrap().is_empty());
    gate.notify_one();
    product.wait_for_decisions(1).await;
    assert!(matches!(
        product.decisions.lock().unwrap()[0].1,
        ReviewDecision::ApprovedForSession
    ));
}
