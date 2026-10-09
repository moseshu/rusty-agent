//! A WebSocket app server set to restart gracefully, receiving real signals: the Unix cases of
//! Codex's `connection_handling_websocket_unix` suite.
//!
//! Each case runs the server in a child process — this test binary again — whose one thread's
//! turns take a set time on a scripted model, connects to it over a real WebSocket, starts a turn,
//! and signals the process. The cases that wait for the turn give it seconds, against
//! sub-second checks that the process is still there, so the checks cannot pass by a turn that
//! ended early; they can only miss a server that left a little too late.

#![cfg(unix)]

use std::{
    io::{BufRead, BufReader},
    net::{Ipv4Addr, SocketAddr},
    process::{Child, Command, ExitStatus, Stdio},
    sync::{Arc, mpsc as std_mpsc},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use ra_core::{
    agent::{AgentId, AgentSpec},
    error::Result,
    item::{ItemId, Message, ModelResponse, OutputPhase, RunItem, RunItemKind},
    model::{
        ApiProtocol, Model, ModelRequest, ModelResolver, ModelSelector, ModelSettings, ProviderKey,
        ResolvedModel,
    },
    session::SessionId,
};
use ra_protocol::{
    server::{AppServer, test_api::ThreadLoader},
    transport::{
        CHANNEL_CAPACITY, ConnectionId,
        websocket::{auth::WebsocketAuthPolicy, start_websocket_acceptor},
    },
};
use ra_runtime::{agent::AgentBinding, runner::RunConfig, thread::AgentThread};
use serde_json::{Value, json};
use tokio::{sync::mpsc, time::timeout};
use tokio_tungstenite::{connect_async, tungstenite::Message as WsMessage};
use tokio_util::sync::CancellationToken;

const PATIENCE: Duration = Duration::from_secs(10);
const THREAD: &str = "thread-1";

/// Marks the run of this binary that serves, and how long its turns take in milliseconds.
const CHILD_TURN_MS: &str = "RA_GRACEFUL_RESTART_CHILD_TURN_MS";

/// A model whose every answer takes a set time.
struct SlowModel(Duration);

#[async_trait]
impl Model for SlowModel {
    async fn get_response(&self, _request: ModelRequest) -> Result<ModelResponse> {
        tokio::time::sleep(self.0).await;
        Ok(ModelResponse::new(vec![RunItem::new(
            ItemId::new("final"),
            RunItemKind::Message(Message::assistant("Done", OutputPhase::Final)),
        )]))
    }
}

struct Resolver(Duration);

impl ModelResolver for Resolver {
    fn resolve_model(&self, _model_name: Option<&str>) -> Result<ResolvedModel> {
        Ok(ResolvedModel::new(
            ModelSelector::new(
                ProviderKey::new("test-provider"),
                Some("mock-model".to_owned()),
                ApiProtocol::OpenAiResponses,
            ),
            Arc::new(SlowModel(self.0)) as Arc<dyn Model>,
            ModelSettings::new(),
            ModelSettings::new(),
        ))
    }
}

/// The child of the cases below: a WebSocket server set to restart gracefully on the process's
/// signals, with one thread loaded for its first connection. Prints the address it listens on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "runs only as the child process of the graceful restart tests"]
async fn graceful_restart_child() {
    let turn_ms: u64 = std::env::var(CHILD_TURN_MS)
        .expect("set by the parent test")
        .parse()
        .unwrap();
    let (events, transport_event_rx) = mpsc::channel(CHANNEL_CAPACITY);
    let shutdown = CancellationToken::new();
    let (addr, acceptor) = start_websocket_acceptor(
        (Ipv4Addr::LOCALHOST, 0).into(),
        events,
        shutdown.clone(),
        WebsocketAuthPolicy::default(),
    )
    .await
    .unwrap();
    let server = AppServer::new(std::env::temp_dir().join("rusty-agent-restart-test"))
        .unwrap()
        .with_graceful_restart(shutdown);
    let agent = AgentSpec::builder()
        .id(AgentId::new("lead"))
        .name("Lead")
        .instructions("lead")
        .build()
        .unwrap();
    let thread = AgentThread::builder(
        SessionId::new(THREAD),
        AgentBinding::direct(agent),
        Arc::new(Resolver(Duration::from_millis(turn_ms))) as Arc<dyn ModelResolver>,
    )
    .with_config(RunConfig::new())
    .build();
    // Connection ids count from zero in a process; the client is this process's first.
    ThreadLoader::new(&server)
        .load(thread, &[ConnectionId::new(0)])
        .await;
    println!("LISTENING {addr}");
    server.run(transport_event_rx).await;
    acceptor.await.unwrap();
}

/// The child process and what it prints.
struct ChildServer {
    process: Child,
    addr: SocketAddr,
}

impl ChildServer {
    fn start(turn: Duration) -> Self {
        let mut process = Command::new(std::env::current_exe().unwrap())
            .args([
                "graceful_restart_child",
                "--exact",
                "--ignored",
                "--nocapture",
                "--quiet",
            ])
            .env(CHILD_TURN_MS, turn.as_millis().to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let stdout = process.stdout.take().unwrap();
        let (line_tx, lines) = std_mpsc::channel::<String>();
        std::thread::spawn(move || {
            // Read to the end, so the child's last lines do not meet a closed pipe.
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                let _ = line_tx.send(line);
            }
        });
        let addr = loop {
            let line = lines
                .recv_timeout(PATIENCE)
                .expect("the child prints its address");
            if let Some(addr) = line.strip_prefix("LISTENING ") {
                break addr.parse().unwrap();
            }
        };
        Self { process, addr }
    }

    fn signal(&self, signal: &str) {
        let status = Command::new("kill")
            .arg(signal)
            .arg(self.process.id().to_string())
            .status()
            .unwrap();
        assert!(status.success(), "kill {signal}: {status}");
    }

    /// Fails if the process exits within `window`.
    fn assert_still_running_for(&mut self, window: Duration) {
        let deadline = Instant::now() + window;
        while Instant::now() < deadline {
            if let Some(status) = self.process.try_wait().unwrap() {
                panic!("the server exited too early during its drain: {status}");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn wait_for_exit_within(&mut self, window: Duration) -> ExitStatus {
        let deadline = Instant::now() + window;
        loop {
            if let Some(status) = self.process.try_wait().unwrap() {
                return status;
            }
            if Instant::now() > deadline {
                let _ = self.process.kill();
                panic!("the server did not exit within {window:?}");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for ChildServer {
    fn drop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

type WsClient =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn send(ws: &mut WsClient, id: i64, method: &str, params: Value) {
    let text = json!({"id": id, "method": method, "params": params}).to_string();
    ws.send(WsMessage::Text(text.into())).await.unwrap();
}

/// The answer to request `id`, passing over anything else.
async fn answer(ws: &mut WsClient, id: i64) -> Value {
    loop {
        let frame = timeout(PATIENCE, ws.next())
            .await
            .expect("the server writes a frame")
            .expect("the connection stays open")
            .unwrap();
        if let WsMessage::Text(text) = frame {
            let message: Value = serde_json::from_str(&text).unwrap();
            if message["id"] == id && message.get("method").is_none() {
                return message;
            }
        }
    }
}

/// Connects, initializes, and starts a turn on the child's thread.
async fn connect_and_start_turn(server: &ChildServer) -> WsClient {
    let (mut ws, _) = connect_async(format!("ws://{}", server.addr)).await.unwrap();
    send(
        &mut ws,
        1,
        "initialize",
        json!({"clientInfo": {"name": "ws_graceful_shutdown", "version": "0.1.0"},
               "capabilities": {"experimentalApi": true}}),
    )
    .await;
    assert!(answer(&mut ws, 1).await.get("result").is_some());
    send(&mut ws, 2, "turn/start", turn_start()).await;
    assert!(answer(&mut ws, 2).await.get("result").is_some());
    ws
}

fn turn_start() -> Value {
    json!({"threadId": THREAD, "input": [{"type": "text", "text": "Hello"}]})
}

/// Reads until the server closes the connection.
async fn expect_disconnect(ws: &mut WsClient) {
    timeout(PATIENCE, async {
        while let Some(frame) = ws.next().await {
            if matches!(frame, Err(_) | Ok(WsMessage::Close(_))) {
                return;
            }
        }
    })
    .await
    .expect("the server disconnects the client");
}

/// Codex's `websocket_transport_sigterm_waits_for_running_turn_before_exit`, with its check that
/// turn work is refused during the drain.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sigterm_waits_for_the_running_turn_before_exit() {
    let mut server = ChildServer::start(Duration::from_secs(3));
    let mut ws = connect_and_start_turn(&server).await;

    server.signal("-TERM");
    send(&mut ws, 3, "turn/start", turn_start()).await;
    assert_eq!(
        answer(&mut ws, 3).await["error"],
        json!({"code": -32600, "message": "Server is draining; retry after reconnecting"})
    );
    server.assert_still_running_for(Duration::from_millis(300));

    let status = server.wait_for_exit_within(PATIENCE);
    assert!(status.success(), "expected graceful exit, got {status}");
    expect_disconnect(&mut ws).await;
}

/// Codex's `websocket_transport_second_ctrl_c_forces_exit_while_turn_running`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_sigint_forces_exit_while_the_turn_runs() {
    let mut server = ChildServer::start(Duration::from_secs(60));
    let mut ws = connect_and_start_turn(&server).await;

    server.signal("-INT");
    send(&mut ws, 3, "turn/start", turn_start()).await;
    assert_eq!(answer(&mut ws, 3).await["error"]["code"], -32600);
    server.assert_still_running_for(Duration::from_millis(300));

    server.signal("-INT");
    let status = server.wait_for_exit_within(Duration::from_secs(5));
    assert!(status.success(), "expected graceful exit, got {status}");
    expect_disconnect(&mut ws).await;
}

/// Codex's `websocket_transport_repeated_sighup_keeps_waiting_for_running_turn`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repeated_sighup_keeps_waiting_for_the_running_turn() {
    let mut server = ChildServer::start(Duration::from_secs(3));
    let mut ws = connect_and_start_turn(&server).await;

    server.signal("-HUP");
    send(&mut ws, 3, "turn/start", turn_start()).await;
    assert_eq!(answer(&mut ws, 3).await["error"]["code"], -32600);
    server.assert_still_running_for(Duration::from_millis(300));
    server.signal("-HUP");
    server.assert_still_running_for(Duration::from_millis(300));

    let status = server.wait_for_exit_within(PATIENCE);
    assert!(status.success(), "expected graceful exit, got {status}");
    expect_disconnect(&mut ws).await;
}
