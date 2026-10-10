//! The lifecycle of threads and connections, as Codex's app-server has it
//! (`request_processors/thread_processor.rs`, `request_processors/thread_lifecycle.rs`,
//! `connection_rpc_gate.rs`, `request_serialization.rs`, `turn_admission.rs` and the processor
//! loop of `lib.rs`).
//!
//! A turn's end ends only the turn; a thread stays loaded while it has subscribers or an active
//! turn, and is unloaded — `thread/closed` — once it has had neither for the unload delay. A
//! closing connection's queued requests never run, its running ones finish first, and its
//! threads' server requests stay with the threads. Requests for one thread run in the order they
//! arrived. A server set to restart gracefully drains on a signal: turn work is refused, the
//! turns running finish, and then every client is disconnected.
//!
//! The server runs in this process with connections whose write queues the test holds, and
//! threads loaded through the crate's `test-api` door — `thread/start` is not ported yet — whose
//! turns run on a scripted model. The unload cases run on Tokio's paused clock, so the delays are
//! exact and no case sleeps. They port Codex's `thread_unsubscribe` suite, its
//! `connection_rpc_gate`, `request_serialization` and `turn_admission` unit tests, the drain cases
//! of `connection_handling_websocket_unix` and its background-terminal serialization tests.

use std::{
    collections::VecDeque,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use futures::{FutureExt, future::BoxFuture};
use ra_core::{
    agent::{AgentId, AgentSpec},
    error::{Error, Result},
    item::{CallId, ItemId, Message, ModelResponse, OutputPhase, RunItem, RunItemKind, ToolCall},
    model::{
        ApiProtocol, Model, ModelRequest, ModelResolver, ModelSelector, ModelSettings, ProviderKey,
        ResolvedModel,
    },
    session::SessionId,
    tool::Tool,
};
use ra_protocol::{
    frame::{JsonRpcMessage, JsonRpcRequest, RequestId},
    server::{
        AppServer, BackgroundTerminalInfo, ThreadBackgroundTerminals,
        protocol::{
            ClientInfo, ClientRequest, CommandExecutionRequestApprovalParams,
            InitializeCapabilities, InitializeParams, ServerRequestPayload,
            ThreadBackgroundTerminalsCleanParams, ThreadBackgroundTerminalsListParams,
            ThreadBackgroundTerminalsTerminateParams, ThreadClosedNotification,
            ThreadUnsubscribeParams, ThreadUnsubscribeStatus, TurnStartParams, UserInput,
        },
        test_api::{
            RestartSignals, ThreadLoader, ThreadRequester, command_execution_approval_decision,
        },
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
const OTHER_THREAD: &str = "thread-2";

// ---------------------------------------------------------------------------------------------
// A scripted model
// ---------------------------------------------------------------------------------------------

enum Step {
    Respond(ModelResponse),
    After(BoxFuture<'static, ()>, ModelResponse),
    Hang,
}

#[derive(Default)]
struct Script {
    steps: Mutex<VecDeque<Step>>,
    calls: Mutex<usize>,
    called: Notify,
}

impl Script {
    fn with(steps: impl IntoIterator<Item = Step>) -> Arc<Self> {
        let script = Arc::new(Self::default());
        script.steps.lock().unwrap().extend(steps);
        script
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
            Some(Step::After(effect, response)) => {
                effect.await;
                Ok(response)
            }
            Some(Step::Hang) => std::future::pending().await,
            None => Err(Error::caller("the model ran out of responses")),
        }
    }
}

fn final_message(text: &str) -> ModelResponse {
    ModelResponse::new(vec![RunItem::new(
        ItemId::new(format!("message-{text}")),
        RunItemKind::Message(Message::assistant(text, OutputPhase::Final)),
    )])
}

fn tool_call(call_id: &str, name: &str, arguments: Value) -> ModelResponse {
    ModelResponse::new(vec![RunItem::new(
        ItemId::new(format!("item-{call_id}")),
        RunItemKind::ToolCall(ToolCall::new(CallId::new(call_id), name, arguments)),
    )])
}

/// A gate a model step waits behind, and the handle that opens it.
fn gate() -> (Arc<Notify>, BoxFuture<'static, ()>) {
    let gate = Arc::new(Notify::new());
    let waiter = Arc::clone(&gate);
    (gate, async move { waiter.notified().await }.boxed())
}

fn agent_thread(thread_id: &str, script: &Arc<Script>) -> AgentThread {
    agent_thread_with_tools(thread_id, script, Vec::new())
}

fn agent_thread_with_tools(
    thread_id: &str,
    script: &Arc<Script>,
    tools: Vec<Arc<dyn Tool>>,
) -> AgentThread {
    let mut agent = AgentSpec::builder()
        .id(AgentId::new("lead"))
        .name("Lead")
        .instructions("lead");
    for tool in tools {
        agent = agent.tool(tool);
    }
    AgentThread::builder(
        SessionId::new(thread_id),
        AgentBinding::direct(agent.build().unwrap()),
        Arc::new(Resolver(Arc::clone(script))) as Arc<dyn ModelResolver>,
    )
    .with_config(RunConfig::new())
    .build()
}

/// Waits until the thread's turn has ended, without reading the time.
async fn until_idle(thread: &AgentThread) {
    timeout(PATIENCE, async {
        while thread.is_running() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the turn ends");
}

/// Lets every task woken so far run before the case goes on.
async fn settle() {
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }
}

// ---------------------------------------------------------------------------------------------
// Background terminals a test controls
// ---------------------------------------------------------------------------------------------

#[derive(Default)]
struct FakeTerminals {
    running: Mutex<Vec<BackgroundTerminalInfo>>,
    terminated: Mutex<Vec<String>>,
    cleaned: Mutex<usize>,
    /// When set, `list` waits until it is notified.
    list_gate: Mutex<Option<Arc<Notify>>>,
    listing: Notify,
}

impl FakeTerminals {
    fn with(ids: &[&str]) -> Arc<Self> {
        let terminals = Arc::new(Self::default());
        terminals.running.lock().unwrap().extend(
            ids.iter()
                .map(|id| BackgroundTerminalInfo::new(format!("call-{id}"), *id, "sleep 30", "/w")),
        );
        terminals
    }

    fn hold_list(&self) -> Arc<Notify> {
        let gate = Arc::new(Notify::new());
        *self.list_gate.lock().unwrap() = Some(Arc::clone(&gate));
        gate
    }

    fn terminated(&self) -> Vec<String> {
        self.terminated.lock().unwrap().clone()
    }

    fn cleaned(&self) -> usize {
        *self.cleaned.lock().unwrap()
    }
}

impl ThreadBackgroundTerminals for FakeTerminals {
    fn list(&self) -> Pin<Box<dyn Future<Output = Vec<BackgroundTerminalInfo>> + Send + '_>> {
        Box::pin(async move {
            let gate = self.list_gate.lock().unwrap().take();
            if let Some(gate) = gate {
                let opened = gate.notified();
                self.listing.notify_waiters();
                opened.await;
            }
            self.running.lock().unwrap().clone()
        })
    }

    fn terminate<'a>(
        &'a self,
        process_id: &'a str,
    ) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>> {
        Box::pin(async move {
            self.terminated.lock().unwrap().push(process_id.to_owned());
            let mut running = self.running.lock().unwrap();
            let before = running.len();
            running.retain(|terminal| terminal.process_id() != process_id);
            running.len() != before
        })
    }

    fn clean(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            *self.cleaned.lock().unwrap() += 1;
            self.running.lock().unwrap().clear();
        })
    }
}

// ---------------------------------------------------------------------------------------------
// The server in this process
// ---------------------------------------------------------------------------------------------

struct Server {
    events: mpsc::Sender<TransportEvent>,
    loader: ThreadLoader,
    task: JoinHandle<()>,
    restart: Option<RestartSignals>,
    transport_shutdown: CancellationToken,
    /// Sends server requests for [`THREAD`] to connection 1, made before the server runs.
    requester: ThreadRequester,
}

impl Server {
    fn start() -> Self {
        Self::start_with(None, false)
    }

    fn with_unload_delay(delay: Duration) -> Self {
        Self::start_with(Some(delay), false)
    }

    fn restartable() -> Self {
        Self::start_with(None, true)
    }

    fn start_with(unload_delay: Option<Duration>, restartable: bool) -> Self {
        let mut server =
            AppServer::new(std::env::temp_dir().join("rusty-agent-lifecycle-test")).unwrap();
        if let Some(delay) = unload_delay {
            server = server.with_thread_unload_delay(delay);
        }
        let transport_shutdown = CancellationToken::new();
        let mut restart = None;
        if restartable {
            let (installed, signals) = RestartSignals::install(server, transport_shutdown.clone());
            server = installed;
            restart = Some(signals);
        }
        let loader = ThreadLoader::new(&server);
        // Every case subscribes connection 1 to the thread it loads anyway.
        let requester =
            ThreadRequester::new(&server, &[ConnectionId::new(1)], SessionId::new(THREAD));
        let (events, transport_event_rx) = mpsc::channel(CHANNEL_CAPACITY);
        let task = tokio::spawn(server.run(transport_event_rx));
        Self {
            events,
            loader,
            task,
            restart,
            transport_shutdown,
            requester,
        }
    }

    /// Opens and initializes a WebSocket-like connection, opting into the experimental API.
    async fn connect(&self, id: u64) -> Connection {
        self.connect_with(id, true).await
    }

    async fn connect_with(&self, id: u64, experimental: bool) -> Connection {
        let id = ConnectionId::new(id);
        let (writer, written) = mpsc::channel(CHANNEL_CAPACITY);
        let disconnect = CancellationToken::new();
        self.events
            .send(connection_opened(
                id,
                ConnectionOrigin::WebSocket,
                writer,
                Some(disconnect.clone()),
            ))
            .await
            .unwrap();
        let mut connection = Connection {
            id,
            written,
            passed: VecDeque::new(),
            next_id: 1,
            disconnect,
        };
        let params = InitializeParams::new(ClientInfo::new("client", "0.1.0"))
            .with_capabilities(InitializeCapabilities::new().with_experimental_api(experimental));
        let request = self
            .request(
                &mut connection,
                "initialize",
                serde_json::to_value(&params).unwrap(),
            )
            .await;
        connection.response(request).await;
        // A second `initialize` is answered once the router knows the connection is initialized.
        let again = self
            .request(
                &mut connection,
                "initialize",
                serde_json::to_value(&params).unwrap(),
            )
            .await;
        connection.error(again).await;
        connection
    }

    async fn close(&self, connection: &Connection) {
        self.events
            .send(TransportEvent::ConnectionClosed {
                connection_id: connection.id,
            })
            .await
            .unwrap();
    }

    /// Loads a thread with `connections` subscribed.
    async fn load(&self, thread: &AgentThread, connections: &[&Connection]) {
        let ids: Vec<_> = connections.iter().map(|connection| connection.id).collect();
        self.loader.load(thread.clone(), &ids).await;
    }

    async fn load_with_terminals(
        &self,
        thread: &AgentThread,
        connections: &[&Connection],
        terminals: Arc<dyn ThreadBackgroundTerminals>,
    ) {
        let ids: Vec<_> = connections.iter().map(|connection| connection.id).collect();
        self.loader
            .load_with_terminals(thread.clone(), &ids, terminals)
            .await;
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

    async fn call(&self, connection: &mut Connection, method: &str, params: Value) -> Value {
        let id = self.request(connection, method, params).await;
        connection.answer(id).await
    }

    async fn turn_start(&self, connection: &mut Connection, thread_id: &str) -> i64 {
        self.request(
            connection,
            "turn/start",
            serde_json::to_value(TurnStartParams::new(
                thread_id,
                vec![UserInput::text("hello")],
            ))
            .unwrap(),
        )
        .await
    }

    async fn unsubscribe(&self, connection: &mut Connection, thread_id: &str) -> Value {
        let answer = self
            .call(
                connection,
                "thread/unsubscribe",
                json!({"threadId": thread_id}),
            )
            .await;
        answer["result"]["status"].clone()
    }

    /// Whether `thread_id` is loaded, asked from `prober`, a connection never subscribed to it.
    async fn is_loaded(&self, prober: &mut Connection, thread_id: &str) -> bool {
        match self.unsubscribe(prober, thread_id).await.as_str() {
            Some("notSubscribed") => true,
            Some("notLoaded") => false,
            other => panic!("the prober is not subscribed: {other:?}"),
        }
    }

    /// Whether the server's loop has returned, once what is already on its way has been handled.
    async fn has_stopped(&self) -> bool {
        settle().await;
        self.task.is_finished()
    }
}

/// A connection, what the server writes to it, and what a case read past while waiting.
struct Connection {
    id: ConnectionId,
    written: mpsc::Receiver<QueuedOutgoingMessage>,
    passed: VecDeque<Value>,
    next_id: i64,
    disconnect: CancellationToken,
}

impl Connection {
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

    async fn answer(&mut self, id: i64) -> Value {
        self.until(|message| message["id"] == id && message.get("method").is_none())
            .await
    }

    async fn response(&mut self, id: i64) -> Value {
        let message = self.answer(id).await;
        assert!(message.get("result").is_some(), "{message}");
        message["result"].clone()
    }

    async fn error(&mut self, id: i64) -> Value {
        let message = self.answer(id).await;
        assert!(message.get("error").is_some(), "{message}");
        message["error"].clone()
    }

    async fn notification(&mut self, method: &str) -> Value {
        self.until(|message| message["method"] == method && message.get("id").is_none())
            .await["params"]
            .clone()
    }

    /// Whether a message `matches` accepts has been written so far.
    fn has_received(&mut self, matches: impl Fn(&Value) -> bool) -> bool {
        while let Ok(queued) = self.written.try_recv() {
            self.passed
                .push_back(serde_json::to_value(queued.message()).unwrap());
        }
        self.passed.iter().any(matches)
    }

    fn has_received_closed(&mut self) -> bool {
        self.has_received(|message| message["method"] == "thread/closed")
    }
}

fn invalid_request(message: &str) -> Value {
    json!({"code": -32600, "message": message})
}

fn draining() -> Value {
    invalid_request("Server is draining; retry after reconnecting")
}

// ---------------------------------------------------------------------------------------------
// Wire shapes
// ---------------------------------------------------------------------------------------------

/// Codex's `serialize_thread_background_terminals_{clean,list,terminate}`, and the unsubscribe and
/// closed payloads of its schema.
#[test]
fn lifecycle_payloads_match_codex_schema() {
    let clean = ClientRequest::try_from(
        JsonRpcRequest::new(8, "thread/backgroundTerminals/clean")
            .with_params(json!({"threadId": "thr_123"})),
    )
    .unwrap();
    assert_eq!(
        serde_json::to_value(&clean).unwrap(),
        json!({"method": "thread/backgroundTerminals/clean", "id": 8, "params": {"threadId": "thr_123"}})
    );
    let list = ClientRequest::try_from(
        JsonRpcRequest::new(8, "thread/backgroundTerminals/list")
            .with_params(json!({"threadId": "thr_123"})),
    )
    .unwrap();
    assert_eq!(
        serde_json::to_value(&list).unwrap(),
        json!({
            "method": "thread/backgroundTerminals/list",
            "id": 8,
            "params": {"threadId": "thr_123", "cursor": null, "limit": null}
        })
    );
    let terminate = ClientRequest::try_from(
        JsonRpcRequest::new(8, "thread/backgroundTerminals/terminate")
            .with_params(json!({"threadId": "thr_123", "processId": "42"})),
    )
    .unwrap();
    assert_eq!(
        serde_json::to_value(&terminate).unwrap(),
        json!({
            "method": "thread/backgroundTerminals/terminate",
            "id": 8,
            "params": {"threadId": "thr_123", "processId": "42"}
        })
    );
    for request in [&clean, &list, &terminate] {
        assert_eq!(request.experimental_reason(), Some(request.method_name()));
    }

    let unsubscribe = ClientRequest::try_from(
        JsonRpcRequest::new(3, "thread/unsubscribe").with_params(json!({"threadId": "thr_123"})),
    )
    .unwrap();
    assert_eq!(unsubscribe.experimental_reason(), None);
    let ClientRequest::ThreadUnsubscribe { request_id, params } = unsubscribe else {
        panic!("expected thread/unsubscribe");
    };
    assert_eq!(request_id, RequestId::Integer(3));
    assert_eq!(params, ThreadUnsubscribeParams::new("thr_123"));
    for (status, wire) in [
        (ThreadUnsubscribeStatus::NotLoaded, "notLoaded"),
        (ThreadUnsubscribeStatus::NotSubscribed, "notSubscribed"),
        (ThreadUnsubscribeStatus::Unsubscribed, "unsubscribed"),
    ] {
        assert_eq!(serde_json::to_value(status).unwrap(), wire);
    }
    let closed: ThreadClosedNotification =
        serde_json::from_value(json!({"threadId": "thr_123"})).unwrap();
    assert_eq!(closed.thread_id(), "thr_123");
    assert_eq!(
        serde_json::to_value(&closed).unwrap(),
        json!({"threadId": "thr_123"})
    );

    // Built the other way, the parameters read back.
    assert_eq!(
        serde_json::to_value(
            ThreadBackgroundTerminalsListParams::new("t")
                .with_cursor("c")
                .with_limit(2)
        )
        .unwrap(),
        json!({"threadId": "t", "cursor": "c", "limit": 2})
    );
    assert_eq!(
        serde_json::to_value(ThreadBackgroundTerminalsCleanParams::new("t")).unwrap(),
        json!({"threadId": "t"})
    );
    assert_eq!(
        serde_json::to_value(ThreadBackgroundTerminalsTerminateParams::new("t", "p")).unwrap(),
        json!({"threadId": "t", "processId": "p"})
    );
}

/// The background-terminal methods are experimental, as in Codex; unsubscribing is not.
#[tokio::test]
async fn background_terminal_methods_require_the_experimental_api() {
    let server = Server::start();
    let mut connection = server.connect_with(1, false).await;
    let script = Script::with([]);
    let thread = agent_thread(THREAD, &script);
    server.load(&thread, &[&connection]).await;

    for method in [
        "thread/backgroundTerminals/clean",
        "thread/backgroundTerminals/list",
        "thread/backgroundTerminals/terminate",
    ] {
        let answer = server
            .call(
                &mut connection,
                method,
                json!({"threadId": THREAD, "processId": "1"}),
            )
            .await;
        assert_eq!(
            answer["error"],
            invalid_request(&format!("{method} requires experimentalApi capability"))
        );
    }
    assert_eq!(
        server.unsubscribe(&mut connection, THREAD).await,
        "unsubscribed"
    );
}

// ---------------------------------------------------------------------------------------------
// Unsubscribing and unloading
// ---------------------------------------------------------------------------------------------

/// Codex's `thread_unsubscribe_keeps_thread_loaded_until_idle_timeout`.
#[tokio::test(start_paused = true)]
async fn thread_unsubscribe_keeps_thread_loaded_until_idle_timeout() {
    let delay = Duration::from_secs(2);
    let server = Server::with_unload_delay(delay);
    let mut client = server.connect(1).await;
    let mut prober = server.connect(2).await;
    let script = Script::with([Step::Respond(final_message("Done"))]);
    let thread = agent_thread(THREAD, &script);
    server.load(&thread, &[&client]).await;

    server.turn_start(&mut client, THREAD).await;
    client.notification("turn/completed").await;

    assert_eq!(server.unsubscribe(&mut client, THREAD).await, "unsubscribed");
    tokio::time::advance(Duration::from_millis(250)).await;
    assert!(server.is_loaded(&mut prober, THREAD).await);
    assert!(!prober.has_received_closed());

    // Subscribing again cancels the pending unload, even after the original deadline.
    server
        .loader
        .subscribe(&SessionId::new(THREAD), client.id);
    tokio::time::advance(Duration::from_millis(2200)).await;
    settle().await;
    assert!(server.is_loaded(&mut prober, THREAD).await);
    assert!(!prober.has_received_closed());

    // Losing the last subscriber starts a fresh countdown.
    assert_eq!(server.unsubscribe(&mut client, THREAD).await, "unsubscribed");
    tokio::time::advance(Duration::from_millis(250)).await;
    settle().await;
    assert!(server.is_loaded(&mut prober, THREAD).await);
    assert!(!prober.has_received_closed());

    // Every initialized connection hears that it closed, subscribed or not.
    let closed = prober.notification("thread/closed").await;
    assert_eq!(closed, json!({"threadId": THREAD}));
    assert_eq!(
        client.notification("thread/closed").await,
        json!({"threadId": THREAD})
    );
    assert!(thread.is_shut_down());
    assert!(!server.is_loaded(&mut prober, THREAD).await);
    let id = server.turn_start(&mut client, THREAD).await;
    assert_eq!(
        client.error(id).await,
        invalid_request("thread not found: thread-1")
    );
}

/// Codex's `thread_unsubscribe_during_turn_keeps_turn_running`, with no delay and with one: a
/// subscribed idle thread stays loaded, an unsubscribed one stays loaded while its turn runs, and
/// the countdown starts when the turn ends.
#[tokio::test(start_paused = true)]
async fn thread_unsubscribe_during_turn_keeps_turn_running() {
    for delay in [Duration::ZERO, Duration::from_secs(1)] {
        let server = Server::with_unload_delay(delay);
        let mut client = server.connect(1).await;
        let mut prober = server.connect(2).await;
        let (release, held) = gate();
        let script = Script::with([Step::After(held, final_message("Done"))]);
        let thread = agent_thread(THREAD, &script);
        server.load(&thread, &[&client]).await;

        // A subscribed, idle thread stays loaded even with no unload delay.
        tokio::time::advance(Duration::from_millis(250)).await;
        settle().await;
        assert!(server.is_loaded(&mut prober, THREAD).await, "{delay:?}");

        let id = server.turn_start(&mut client, THREAD).await;
        client.response(id).await;
        script.wait_for_calls(1).await;
        assert_eq!(server.unsubscribe(&mut client, THREAD).await, "unsubscribed");

        tokio::time::advance(Duration::from_millis(1200)).await;
        settle().await;
        assert!(server.is_loaded(&mut prober, THREAD).await, "{delay:?}");
        assert!(!prober.has_received_closed(), "{delay:?}");
        assert!(thread.is_running());

        release.notify_one();
        until_idle(&thread).await;
        settle().await;
        if delay > Duration::ZERO {
            // Once the turn finishes, inactivity starts a fresh countdown: from the turn's end, not
            // from when the thread was last seen running.
            tokio::time::advance(delay - Duration::from_millis(100)).await;
            settle().await;
            assert!(server.is_loaded(&mut prober, THREAD).await);
            assert!(!prober.has_received_closed());
        }
        assert_eq!(
            prober.notification("thread/closed").await,
            json!({"threadId": THREAD})
        );
    }
}

/// Codex's `thread_unsubscribe_reports_not_subscribed_before_idle_unload`, and `notLoaded` for a
/// thread that is not loaded.
#[tokio::test]
async fn thread_unsubscribe_reports_each_status() {
    let server = Server::start();
    let mut client = server.connect(1).await;
    let script = Script::with([]);
    let thread = agent_thread(THREAD, &script);
    server.load(&thread, &[&client]).await;

    assert_eq!(server.unsubscribe(&mut client, THREAD).await, "unsubscribed");
    assert_eq!(
        server.unsubscribe(&mut client, THREAD).await,
        "notSubscribed"
    );
    assert_eq!(
        server.unsubscribe(&mut client, "no-such-thread").await,
        "notLoaded"
    );
}

/// Unloading withdraws the thread's server requests still waiting, shuts the thread down and stops
/// its background terminals, as Codex's `unload_thread_without_subscribers` does.
#[tokio::test(start_paused = true)]
async fn unloading_withdraws_requests_and_stops_background_terminals() {
    let server = Server::with_unload_delay(Duration::from_secs(1));
    let mut client = server.connect(1).await;
    let mut prober = server.connect(2).await;
    let script = Script::with([]);
    let thread = agent_thread(THREAD, &script);
    let terminals = FakeTerminals::with(&["exec-1"]);
    server
        .load_with_terminals(&thread, &[&client], Arc::clone(&terminals) as _)
        .await;
    let requester = &server.requester;
    let (_, answer) = requester
        .send_request(ServerRequestPayload::CommandExecutionRequestApproval(
            CommandExecutionRequestApprovalParams::new(THREAD, "turn-1", "item-1", 0),
        ))
        .await;
    client
        .until(|message| message["method"] == "item/commandExecution/requestApproval")
        .await;

    assert_eq!(server.unsubscribe(&mut client, THREAD).await, "unsubscribed");
    assert_eq!(
        prober.notification("thread/closed").await,
        json!({"threadId": THREAD})
    );
    // Withdrawn without an answer, which the approval reads as failed, denying the command.
    let answer = timeout(PATIENCE, answer).await.expect("the request is withdrawn");
    assert!(answer.is_err());
    assert_eq!(
        format!("{:?}", command_execution_approval_decision(answer)),
        "Some(Denied { rejection: \"approval request failed\" })"
    );
    assert!(requester.pending_requests().is_empty());
    assert!(thread.is_shut_down());
    assert_eq!(terminals.cleaned(), 1);
}

/// A closing connection's threads keep their server requests — a client that subscribes again is
/// sent them — until the thread, left with no subscribers, unloads.
#[tokio::test(start_paused = true)]
async fn a_closed_connection_leaves_its_threads_requests_until_they_unload() {
    let delay = Duration::from_secs(5);
    let server = Server::with_unload_delay(delay);
    let client = server.connect(1).await;
    let mut other = server.connect(2).await;
    let script = Script::with([]);
    let thread = agent_thread(THREAD, &script);
    server.load(&thread, &[&client]).await;
    let requester = &server.requester;
    let (_, answer) = requester
        .send_request(ServerRequestPayload::CommandExecutionRequestApproval(
            CommandExecutionRequestApprovalParams::new(THREAD, "turn-1", "item-1", 0),
        ))
        .await;

    server.close(&client).await;
    tokio::time::advance(Duration::from_secs(1)).await;
    settle().await;
    assert_eq!(requester.pending_requests().len(), 1);
    assert!(server.is_loaded(&mut other, THREAD).await);

    // Another client attaches and is sent the request still waiting, which it answers.
    requester.replay_to_connection(other.id).await;
    let request = other
        .until(|message| message["method"] == "item/commandExecution/requestApproval")
        .await;
    server
        .send(
            &other,
            JsonRpcMessage::Response(ra_protocol::frame::JsonRpcResponse::new(
                serde_json::from_value::<RequestId>(request["id"].clone()).unwrap(),
                json!({"decision": "accept"}),
            )),
        )
        .await;
    let answer = timeout(PATIENCE, answer).await.expect("the request is answered");
    assert!(command_execution_approval_decision(answer).is_some());
    assert!(!thread.is_shut_down());
}

/// When its last connection closes, a thread unloads after the delay, and the other connections
/// hear of it.
#[tokio::test(start_paused = true)]
async fn closing_the_last_subscriber_unloads_the_thread_after_the_delay() {
    let server = Server::with_unload_delay(Duration::from_secs(3));
    let client = server.connect(1).await;
    let mut other = server.connect(2).await;
    let script = Script::with([]);
    let thread = agent_thread(THREAD, &script);
    server.load(&thread, &[&client]).await;

    server.close(&client).await;
    tokio::time::advance(Duration::from_secs(2)).await;
    settle().await;
    assert!(server.is_loaded(&mut other, THREAD).await);
    assert!(!other.has_received_closed());
    assert_eq!(
        other.notification("thread/closed").await,
        json!({"threadId": THREAD})
    );
    assert!(thread.is_shut_down());
}

/// The lifecycle invariant: a turn's end closes neither the connection nor the thread, and what
/// the turn's tools left running keeps running. The next turn starts on the same thread.
#[tokio::test(start_paused = true)]
async fn a_turn_end_releases_neither_the_thread_nor_its_terminals() {
    let server = Server::with_unload_delay(Duration::ZERO);
    let mut client = server.connect(1).await;
    let mut prober = server.connect(2).await;
    let script = Script::with([
        Step::Respond(final_message("first")),
        Step::Respond(final_message("second")),
    ]);
    let thread = agent_thread(THREAD, &script);
    let terminals = FakeTerminals::with(&["exec-1"]);
    server
        .load_with_terminals(&thread, &[&client], Arc::clone(&terminals) as _)
        .await;

    server.turn_start(&mut client, THREAD).await;
    client.notification("turn/completed").await;
    tokio::time::advance(Duration::from_secs(3600)).await;
    settle().await;

    assert!(server.is_loaded(&mut prober, THREAD).await);
    let list = server
        .call(
            &mut client,
            "thread/backgroundTerminals/list",
            json!({"threadId": THREAD}),
        )
        .await;
    assert_eq!(list["result"]["data"][0]["processId"], "exec-1");
    assert_eq!(terminals.cleaned(), 0);
    assert!(!thread.is_shut_down());
    assert!(!client.disconnect.is_cancelled());

    server.turn_start(&mut client, THREAD).await;
    let completed = client.notification("turn/completed").await;
    assert_eq!(completed["turn"]["status"], "completed");
    assert_eq!(completed["threadId"], THREAD);
}

/// A thread shut down while a request for it was on its way refuses it as Codex's thread refuses
/// input once its session loop has ended.
#[tokio::test]
async fn a_shut_down_thread_refuses_turns_as_codex_does() {
    let server = Server::start();
    let mut client = server.connect(1).await;
    let script = Script::with([]);
    let thread = agent_thread(THREAD, &script);
    server.load(&thread, &[&client]).await;
    thread.shutdown().await;

    let id = server.turn_start(&mut client, THREAD).await;
    assert_eq!(
        client.error(id).await,
        json!({
            "code": -32603,
            "message": "failed to submit turn input: internal error; agent loop died unexpectedly"
        })
    );
    let id = server
        .request(
            &mut client,
            "turn/steer",
            json!({"threadId": THREAD, "input": [{"type": "text", "text": "x"}], "expectedTurnId": "t"}),
        )
        .await;
    assert_eq!(
        client.error(id).await,
        json!({
            "code": -32603,
            "message": "failed to steer turn: internal error; agent loop died unexpectedly"
        })
    );
}

// ---------------------------------------------------------------------------------------------
// Connection gates and per-thread order
// ---------------------------------------------------------------------------------------------

/// Codex's `same_key_requests_run_fifo`, `different_keys_run_concurrently` and the unscoped
/// methods: requests for one thread wait for those before them, from any connection; requests for
/// another thread, and unscoped ones, do not.
#[tokio::test]
async fn requests_for_one_thread_run_in_arrival_order() {
    let server = Server::start();
    let mut first = server.connect(1).await;
    let mut second = server.connect(2).await;
    let script = Script::with([]);
    let thread = agent_thread(THREAD, &script);
    let terminals = FakeTerminals::with(&["exec-1"]);
    server
        .load_with_terminals(&thread, &[&first, &second], Arc::clone(&terminals) as _)
        .await;
    let other = agent_thread(OTHER_THREAD, &script);
    server.load(&other, &[&second]).await;

    let held = terminals.hold_list();
    let listing = terminals.listing.notified();
    let list = server
        .request(
            &mut first,
            "thread/backgroundTerminals/list",
            json!({"threadId": THREAD}),
        )
        .await;
    timeout(PATIENCE, listing).await.unwrap();
    let terminate = server
        .request(
            &mut second,
            "thread/backgroundTerminals/terminate",
            json!({"threadId": THREAD, "processId": "exec-1"}),
        )
        .await;
    let unsubscribe = server
        .request(
            &mut first,
            "thread/unsubscribe",
            json!({"threadId": THREAD}),
        )
        .await;

    // Neither waits for the held thread.
    let mock = server
        .call(&mut second, "mock/experimentalMethod", json!({"value": "v"}))
        .await;
    assert_eq!(mock["result"], json!({"echoed": "v"}));
    assert_eq!(
        server.unsubscribe(&mut second, OTHER_THREAD).await,
        "unsubscribed"
    );
    assert!(terminals.terminated().is_empty());

    held.notify_one();
    // Listed before it was terminated.
    assert_eq!(
        first.response(list).await["data"][0]["processId"],
        "exec-1"
    );
    assert_eq!(
        second.response(terminate).await,
        json!({"terminated": true})
    );
    assert_eq!(
        first.response(unsubscribe).await,
        json!({"status": "unsubscribed"})
    );
}

/// Codex's `closed_gate_request_is_skipped_and_following_requests_continue` and its cleanup
/// order: a closed connection's queued request never runs, the next connection's does, and the
/// connection's subscriptions go only once its running request has finished.
#[tokio::test(start_paused = true)]
async fn a_closed_connections_queued_requests_never_run() {
    let server = Server::with_unload_delay(Duration::ZERO);
    let mut closing = server.connect(1).await;
    let mut staying = server.connect(2).await;
    let script = Script::with([]);
    let thread = agent_thread(THREAD, &script);
    let terminals = FakeTerminals::with(&["exec-1", "exec-2"]);
    server
        .load_with_terminals(&thread, &[&closing], Arc::clone(&terminals) as _)
        .await;

    let held = terminals.hold_list();
    let listing = terminals.listing.notified();
    server
        .request(
            &mut closing,
            "thread/backgroundTerminals/list",
            json!({"threadId": THREAD}),
        )
        .await;
    timeout(PATIENCE, listing).await.unwrap();
    server
        .request(
            &mut closing,
            "thread/backgroundTerminals/terminate",
            json!({"threadId": THREAD, "processId": "exec-1"}),
        )
        .await;
    let terminate = server
        .request(
            &mut staying,
            "thread/backgroundTerminals/terminate",
            json!({"threadId": THREAD, "processId": "exec-2"}),
        )
        .await;
    server.close(&closing).await;

    // Its running request holds its subscription: the thread, with no delay, stays loaded.
    tokio::time::advance(Duration::from_secs(60)).await;
    settle().await;
    assert!(!staying.has_received_closed());
    assert!(!thread.is_shut_down());

    held.notify_one();
    assert_eq!(
        staying.response(terminate).await,
        json!({"terminated": true})
    );
    assert_eq!(terminals.terminated(), vec!["exec-2".to_owned()]);
    assert_eq!(
        staying.notification("thread/closed").await,
        json!({"threadId": THREAD})
    );
}

// ---------------------------------------------------------------------------------------------
// Draining for a restart
// ---------------------------------------------------------------------------------------------

/// Codex's `websocket_transport_ctrl_c_waits_for_running_turn_before_exit` and
/// `websocket_transport_allows_turn_interrupt_during_drain`: once a signal arrives, turn work is
/// refused while other methods are answered; the running turn may still be interrupted, and once
/// it has ended the transports are stopped and every client is disconnected.
#[tokio::test]
async fn a_drain_refuses_turn_work_and_waits_for_the_running_turn() {
    let server = Server::restartable();
    let mut client = server.connect(1).await;
    let script = Script::with([Step::Hang]);
    let thread = agent_thread(THREAD, &script);
    server.load(&thread, &[&client]).await;
    let id = server.turn_start(&mut client, THREAD).await;
    let turn_id = client.response(id).await["turn"]["id"].clone();
    script.wait_for_calls(1).await;

    assert!(server.restart.as_ref().unwrap().forceable().await);
    let refused = server.turn_start(&mut client, THREAD).await;
    assert_eq!(client.error(refused).await, draining());
    for (method, params) in [
        (
            "turn/steer",
            json!({"threadId": THREAD, "expectedTurnId": turn_id, "input": []}),
        ),
        (
            "thread/settings/update",
            json!({"threadId": THREAD, "model": "other"}),
        ),
        (
            "turn/settings/update",
            json!({"threadId": THREAD, "turnId": "unused", "model": "other"}),
        ),
    ] {
        let answer = server.call(&mut client, method, params).await;
        assert_eq!(answer["error"], draining(), "{method}");
    }
    let mock = server
        .call(&mut client, "mock/experimentalMethod", json!({"value": "v"}))
        .await;
    assert_eq!(mock["result"], json!({"echoed": "v"}));
    assert!(!server.has_stopped().await);
    assert!(!server.transport_shutdown.is_cancelled());
    assert!(!client.disconnect.is_cancelled());

    let interrupt = server
        .request(
            &mut client,
            "turn/interrupt",
            json!({"threadId": THREAD, "turnId": turn_id}),
        )
        .await;
    client.response(interrupt).await;
    let completed = client.notification("turn/completed").await;
    assert_eq!(completed["turn"]["status"], "interrupted");

    timeout(PATIENCE, server.task).await.unwrap().unwrap();
    assert!(server.transport_shutdown.is_cancelled());
    assert!(client.disconnect.is_cancelled());
    assert!(thread.is_shut_down());
}

/// Codex's `websocket_transport_second_ctrl_c_forces_exit_while_turn_running`: a second
/// forceable signal stops the server at once, leaving its threads as they are.
#[tokio::test]
async fn a_second_forceable_signal_stops_at_once() {
    let server = Server::restartable();
    let mut client = server.connect(1).await;
    let script = Script::with([Step::Hang]);
    let thread = agent_thread(THREAD, &script);
    server.load(&thread, &[&client]).await;
    server.turn_start(&mut client, THREAD).await;
    script.wait_for_calls(1).await;

    let restart = server.restart.as_ref().unwrap();
    assert!(restart.forceable().await);
    assert!(!server.has_stopped().await);
    assert!(restart.forceable().await);

    timeout(PATIENCE, server.task).await.unwrap().unwrap();
    assert!(server.transport_shutdown.is_cancelled());
    assert!(client.disconnect.is_cancelled());
    assert!(thread.is_running());
    assert!(!thread.is_shut_down());
}

/// Codex's `websocket_transport_repeated_sighup_keeps_waiting_for_running_turn`.
#[tokio::test]
async fn repeated_graceful_only_signals_keep_waiting_for_the_running_turn() {
    let server = Server::restartable();
    let mut client = server.connect(1).await;
    let (release, held) = gate();
    let script = Script::with([Step::After(held, final_message("Done"))]);
    let thread = agent_thread(THREAD, &script);
    server.load(&thread, &[&client]).await;
    server.turn_start(&mut client, THREAD).await;
    script.wait_for_calls(1).await;

    let restart = server.restart.as_ref().unwrap();
    assert!(restart.graceful_only().await);
    assert!(!server.has_stopped().await);
    assert!(restart.graceful_only().await);
    assert!(!server.has_stopped().await);
    let mock = server
        .call(&mut client, "mock/experimentalMethod", json!({"value": "v"}))
        .await;
    assert_eq!(mock["result"], json!({"echoed": "v"}));

    release.notify_one();
    let completed = client.notification("turn/completed").await;
    assert_eq!(completed["turn"]["status"], "completed");
    timeout(PATIENCE, server.task).await.unwrap().unwrap();
    assert!(client.disconnect.is_cancelled());
}

/// Codex's `drain_rejects_new_work_without_waiting_for_admitted_work` and
/// `shutdown_of_live_gate_skips_already_queued_requests`, through the server: a turn admitted
/// before the drain but still queued behind its thread holds the drain open, and is refused when
/// its turn comes.
#[tokio::test]
async fn a_queued_turn_holds_the_drain_and_is_refused_when_it_runs() {
    let server = Server::restartable();
    let mut client = server.connect(1).await;
    let script = Script::with([]);
    let thread = agent_thread(THREAD, &script);
    let terminals = FakeTerminals::with(&[]);
    server
        .load_with_terminals(&thread, &[&client], Arc::clone(&terminals) as _)
        .await;

    let held = terminals.hold_list();
    let listing = terminals.listing.notified();
    let list = server
        .request(
            &mut client,
            "thread/backgroundTerminals/list",
            json!({"threadId": THREAD}),
        )
        .await;
    timeout(PATIENCE, listing).await.unwrap();
    let queued = server.turn_start(&mut client, THREAD).await;
    // Answered after the turn was admitted, as the connection's requests are read in order.
    let mock = server
        .call(&mut client, "mock/experimentalMethod", json!({"value": "v"}))
        .await;
    assert_eq!(mock["result"], json!({"echoed": "v"}));

    assert!(server.restart.as_ref().unwrap().forceable().await);
    assert!(!server.has_stopped().await);

    held.notify_one();
    client.response(list).await;
    assert_eq!(client.error(queued).await, draining());
    timeout(PATIENCE, server.task).await.unwrap().unwrap();
    assert!(!thread.is_running());
}

/// Codex's `shutdown_of_live_gate_skips_already_queued_requests`, through the server: a turn queued
/// by a connection that then closes is released at once, so its permit no longer holds the drain
/// open while the request ahead of it still runs.
#[tokio::test]
async fn a_closed_connections_queued_turn_no_longer_holds_the_drain() {
    let server = Server::restartable();
    let mut staying = server.connect(1).await;
    let mut closing = server.connect(2).await;
    let script = Script::with([]);
    let thread = agent_thread(THREAD, &script);
    let terminals = FakeTerminals::with(&[]);
    server
        .load_with_terminals(&thread, &[&staying], Arc::clone(&terminals) as _)
        .await;

    let held = terminals.hold_list();
    let listing = terminals.listing.notified();
    server
        .request(
            &mut staying,
            "thread/backgroundTerminals/list",
            json!({"threadId": THREAD}),
        )
        .await;
    timeout(PATIENCE, listing).await.unwrap();
    server.turn_start(&mut closing, THREAD).await;
    // Answered after the turn was admitted, as the connection's requests are read in order.
    let mock = server
        .call(&mut closing, "mock/experimentalMethod", json!({"value": "v"}))
        .await;
    assert_eq!(mock["result"], json!({"echoed": "v"}));
    assert!(server.restart.as_ref().unwrap().forceable().await);
    assert!(!server.has_stopped().await);
    assert!(!server.transport_shutdown.is_cancelled());

    server.close(&closing).await;
    timeout(PATIENCE, server.transport_shutdown.cancelled())
        .await
        .expect("the drain finishes while the list still runs");
    assert!(staying.disconnect.is_cancelled());

    held.notify_one();
    timeout(PATIENCE, server.task).await.unwrap().unwrap();
    assert!(!thread.is_running());
    assert!(script.steps.lock().unwrap().is_empty());
}

/// A server that stops on its own — here, its transports gone — first shuts its threads down,
/// interrupting the turn still running and stopping their background terminals.
#[tokio::test]
async fn stopping_the_server_shuts_its_threads_down() {
    let server = Server::start();
    let mut client = server.connect(1).await;
    let script = Script::with([Step::Hang]);
    let thread = agent_thread(THREAD, &script);
    let terminals = FakeTerminals::with(&["exec-1"]);
    server
        .load_with_terminals(&thread, &[&client], Arc::clone(&terminals) as _)
        .await;
    server.turn_start(&mut client, THREAD).await;
    script.wait_for_calls(1).await;

    let Server { events, task, .. } = server;
    drop(events);
    timeout(PATIENCE, task).await.unwrap().unwrap();
    assert!(thread.is_shut_down());
    assert!(!thread.is_running());
    assert_eq!(terminals.cleaned(), 1);
}

// ---------------------------------------------------------------------------------------------
// Background terminals
// ---------------------------------------------------------------------------------------------

/// Codex's `thread/backgroundTerminals/*` handlers: listing pages by id, terminating one, and
/// cleaning them all; a thread loaded without terminals has none.
#[tokio::test]
async fn background_terminals_are_listed_terminated_and_cleaned() {
    let server = Server::start();
    let mut client = server.connect(1).await;
    let script = Script::with([]);
    let thread = agent_thread(THREAD, &script);
    let terminals = FakeTerminals::with(&["exec-c", "exec-a", "exec-b"]);
    server
        .load_with_terminals(&thread, &[&client], Arc::clone(&terminals) as _)
        .await;
    let other = agent_thread(OTHER_THREAD, &script);
    server.load(&other, &[&client]).await;

    let list = |params: Value| {
        let mut params = params;
        params["threadId"] = json!(THREAD);
        params
    };
    let page = server
        .call(&mut client, "thread/backgroundTerminals/list", list(json!({})))
        .await;
    let data: Vec<Value> = ["exec-a", "exec-b", "exec-c"]
        .iter()
        .map(|id| {
            json!({
                "itemId": format!("call-{id}"),
                "processId": id,
                "command": "sleep 30",
                "cwd": "/w",
                "osPid": null,
                "cpuPercent": null,
                "rssKb": null
            })
        })
        .collect();
    assert_eq!(page["result"], json!({"data": data, "nextCursor": null}));
    let page = server
        .call(
            &mut client,
            "thread/backgroundTerminals/list",
            list(json!({"limit": 2})),
        )
        .await;
    assert_eq!(page["result"]["data"].as_array().unwrap().len(), 2);
    assert_eq!(page["result"]["nextCursor"], "exec-b");
    let page = server
        .call(
            &mut client,
            "thread/backgroundTerminals/list",
            list(json!({"cursor": "exec-b", "limit": 2})),
        )
        .await;
    assert_eq!(page["result"]["data"][0]["processId"], "exec-c");
    assert_eq!(page["result"]["nextCursor"], Value::Null);
    // A limit of zero still returns one, as Codex's does.
    let page = server
        .call(
            &mut client,
            "thread/backgroundTerminals/list",
            list(json!({"limit": 0})),
        )
        .await;
    assert_eq!(page["result"]["data"].as_array().unwrap().len(), 1);
    assert_eq!(page["result"]["nextCursor"], "exec-a");

    let terminate = |process_id: &str| json!({"threadId": THREAD, "processId": process_id});
    let answer = server
        .call(
            &mut client,
            "thread/backgroundTerminals/terminate",
            terminate("exec-a"),
        )
        .await;
    assert_eq!(answer["result"], json!({"terminated": true}));
    let answer = server
        .call(
            &mut client,
            "thread/backgroundTerminals/terminate",
            terminate("exec-a"),
        )
        .await;
    assert_eq!(answer["result"], json!({"terminated": false}));

    let answer = server
        .call(
            &mut client,
            "thread/backgroundTerminals/clean",
            json!({"threadId": THREAD}),
        )
        .await;
    assert_eq!(answer["result"], json!({}));
    assert_eq!(terminals.cleaned(), 1);
    let page = server
        .call(&mut client, "thread/backgroundTerminals/list", list(json!({})))
        .await;
    assert_eq!(page["result"], json!({"data": [], "nextCursor": null}));

    // A thread its host loaded without terminals has none.
    let page = server
        .call(
            &mut client,
            "thread/backgroundTerminals/list",
            json!({"threadId": OTHER_THREAD}),
        )
        .await;
    assert_eq!(page["result"], json!({"data": [], "nextCursor": null}));
    let answer = server
        .call(
            &mut client,
            "thread/backgroundTerminals/terminate",
            json!({"threadId": OTHER_THREAD, "processId": "exec-a"}),
        )
        .await;
    assert_eq!(answer["result"], json!({"terminated": false}));
    let answer = server
        .call(
            &mut client,
            "thread/backgroundTerminals/clean",
            json!({"threadId": OTHER_THREAD}),
        )
        .await;
    assert_eq!(answer["result"], json!({}));

    for method in [
        "thread/backgroundTerminals/clean",
        "thread/backgroundTerminals/list",
        "thread/backgroundTerminals/terminate",
    ] {
        let answer = server
            .call(
                &mut client,
                method,
                json!({"threadId": "missing", "processId": "x"}),
            )
            .await;
        assert_eq!(
            answer["error"],
            invalid_request("thread not found: missing"),
            "{method}"
        );
    }
}

/// The coding host's terminals over the real executor: a command `exec_command` leaves running
/// outlives its turn, is listed with the call that started it, and is stopped, as in Codex's
/// `unified_exec_persists_across_requests`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exec_command_terminals_outlive_their_turn() {
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
                    Arc::new(move |_context: &ra_core::tool::ToolContext<'_>| {
                        Ok(environment.clone())
                    })
                },
                Arc::new(|_context: &ra_core::tool::ToolContext<'_>| Ok(false)),
            ),
    );
    let server = Server::start();
    let mut client = server.connect(1).await;
    let script = Script::with([
        Step::Respond(tool_call(
            "call-sleep",
            "exec_command",
            json!({
                "cmd": "sleep 30",
                "workdir": null,
                "shell": null,
                "tty": null,
                "login": null,
                "yield_time_ms": 200,
                "timeout_ms": null,
                "sandbox_permissions": null,
                "justification": null,
                "prefix_rule": null
            }),
        )),
        Step::Respond(final_message("started")),
    ]);
    let thread = agent_thread_with_tools(THREAD, &script, vec![exec_command]);
    server
        .load_with_terminals(&thread, &[&client], Arc::new(host.background_terminals()))
        .await;

    server.turn_start(&mut client, THREAD).await;
    let completed = client.notification("turn/completed").await;
    assert_eq!(completed["turn"]["status"], "completed");

    let page = server
        .call(
            &mut client,
            "thread/backgroundTerminals/list",
            json!({"threadId": THREAD}),
        )
        .await;
    let data = page["result"]["data"].as_array().unwrap().clone();
    assert_eq!(data.len(), 1, "{page}");
    assert_eq!(data[0]["itemId"], "call-sleep");
    assert_eq!(data[0]["command"], "sleep 30");
    assert_eq!(
        data[0]["cwd"],
        json!(host.workspace_root().to_string_lossy())
    );
    let process_id = data[0]["processId"].as_str().unwrap().to_owned();
    assert!(process_id.starts_with("exec-"), "{process_id}");

    let terminate = json!({"threadId": THREAD, "processId": process_id});
    let answer = server
        .call(
            &mut client,
            "thread/backgroundTerminals/terminate",
            terminate.clone(),
        )
        .await;
    assert_eq!(answer["result"], json!({"terminated": true}));
    let answer = server
        .call(&mut client, "thread/backgroundTerminals/terminate", terminate)
        .await;
    assert_eq!(answer["result"], json!({"terminated": false}));
    let page = server
        .call(
            &mut client,
            "thread/backgroundTerminals/list",
            json!({"threadId": THREAD}),
        )
        .await;
    assert_eq!(page["result"]["data"], json!([]));
}
