//! Control socket cases ported from Codex `44fe510`'s `unix_socket_tests.rs`.
#![cfg(unix)]

use futures::{SinkExt, StreamExt};
use ra_protocol::{
    frame::{JsonRpcMessage, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse},
    server::AppServer,
    transport::{
        CHANNEL_CAPACITY, ConnectionId, ConnectionOrigin, QueuedOutgoingMessage, TransportEvent,
        unix_socket::{
            DaemonShutdownAccess, acquire_app_server_startup_lock, app_server_control_socket_path,
            app_server_startup_lock_path, connect_control_socket, start_control_socket_acceptor,
        },
    },
};
use serde_json::json;
use std::{
    io::ErrorKind,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{
    net::UnixStream,
    sync::{mpsc, oneshot},
    task::JoinHandle,
    time::timeout,
};
use tokio_tungstenite::{
    WebSocketStream, client_async,
    tungstenite::{
        Bytes, Message as WebSocketMessage, client::IntoClientRequest, protocol::WebSocketConfig,
    },
};
use tokio_util::sync::CancellationToken;

#[test]
fn control_paths_use_the_shared_home_override() {
    let home = tempfile::tempdir().unwrap();
    ra_paths::AbsolutePathBufGuard::with_home_directory(home.path(), || {
        let expected = home.path().join("state");
        assert_eq!(
            app_server_control_socket_path("~/state").unwrap(),
            app_server_control_socket_path(&expected).unwrap()
        );
        assert_eq!(
            app_server_startup_lock_path("~/state").unwrap(),
            app_server_startup_lock_path(&expected).unwrap()
        );
    });
}

const PATIENCE: Duration = Duration::from_secs(10);

#[cfg(unix)]
#[tokio::test]
async fn long_control_socket_paths_connect_to_distinct_daemons() {
    let temp_dir = tempfile::TempDir::new().expect("temp dir");
    let long_parent = temp_dir.path().join("x".repeat(120));
    let mut sockets = Vec::new();
    let mut acceptors = Vec::new();

    for name in ["first", "second"] {
        let codex_home = long_parent.join(name);
        std::fs::create_dir_all(&codex_home).expect("codex home");
        let socket_path = app_server_control_socket_path(&codex_home).expect("socket path");
        let (transport_event_tx, _transport_event_rx) =
            mpsc::channel::<TransportEvent>(CHANNEL_CAPACITY);
        let shutdown_token = CancellationToken::new();
        let acceptor = start_control_socket_acceptor(
            socket_path.clone(),
            transport_event_tx,
            shutdown_token.clone(),
            DaemonShutdownAccess::Disabled,
        )
        .await
        .expect("control socket acceptor should start");
        sockets.push(socket_path);
        acceptors.push((shutdown_token, acceptor));
    }

    assert_ne!(
        std::fs::read_link(sockets[0].as_path()).expect("first socket target"),
        std::fs::read_link(sockets[1].as_path()).expect("second socket target")
    );
    for socket in &sockets {
        connect_to_socket(socket.as_path())
            .await
            .expect("client should connect through long path");
    }
    for (shutdown_token, acceptor) in acceptors {
        shutdown_token.cancel();
        acceptor.await.expect("acceptor should stop");
    }
}

#[tokio::test]
async fn control_socket_acceptor_upgrades_and_forwards_websocket_text_messages_and_pings() {
    let temp_dir = tempfile::TempDir::new().expect("temp dir");
    let socket_path = test_socket_path(temp_dir.path());
    let (transport_event_tx, mut transport_event_rx) =
        mpsc::channel::<TransportEvent>(CHANNEL_CAPACITY);
    let shutdown_token = CancellationToken::new();
    let accept_handle = start_control_socket_acceptor(
        socket_path.clone(),
        transport_event_tx,
        shutdown_token.clone(),
        DaemonShutdownAccess::Disabled,
    )
    .await
    .expect("control socket acceptor should start");

    let stream = connect_to_socket(socket_path.as_path())
        .await
        .expect("client should connect");
    let (mut websocket, response) = client_async("ws://localhost/rpc", stream)
        .await
        .expect("websocket upgrade should complete");
    assert_eq!(response.status().as_u16(), 101);
    let advertised_max = response
        .headers()
        .get("x-codex-websocket-max-unfragmented-message-bytes")
        .expect("byte cap header should be advertised")
        .to_str()
        .expect("byte cap header should be ASCII")
        .parse::<usize>()
        .expect("byte cap header should be a number");
    let websocket_config = WebSocketConfig::default();
    assert_eq!(
        advertised_max,
        [
            websocket_config.max_frame_size,
            websocket_config.max_message_size,
        ]
        .into_iter()
        .flatten()
        .min()
        .expect("default websocket config should have an incoming size limit")
    );

    let opened = timeout(PATIENCE, transport_event_rx.recv())
        .await
        .expect("connection opened event should arrive")
        .expect("connection opened event");
    let connection_id = match opened {
        TransportEvent::ConnectionOpened { connection_id, .. } => connection_id,
        _ => panic!("expected connection opened event"),
    };

    let notification = JsonRpcMessage::Notification(JsonRpcNotification::new("initialized"));
    websocket
        .send(WebSocketMessage::Text(
            serde_json::to_string(&notification)
                .expect("notification should serialize")
                .into(),
        ))
        .await
        .expect("notification should send");

    let incoming = timeout(PATIENCE, transport_event_rx.recv())
        .await
        .expect("incoming message event should arrive")
        .expect("incoming message event");
    assert_eq!(
        match incoming {
            TransportEvent::IncomingMessage {
                connection_id: incoming_connection_id,
                message,
            } => (incoming_connection_id, message),
            _ => panic!("expected incoming message event"),
        },
        (connection_id, notification)
    );

    websocket
        .send(WebSocketMessage::Ping(Bytes::from_static(b"check")))
        .await
        .expect("ping should send");
    let pong = timeout(PATIENCE, websocket.next())
        .await
        .expect("pong should arrive")
        .expect("pong frame")
        .expect("pong should be valid");
    assert_eq!(pong, WebSocketMessage::Pong(Bytes::from_static(b"check")));

    websocket.close(None).await.expect("close should send");
    let closed = timeout(PATIENCE, transport_event_rx.recv())
        .await
        .expect("connection closed event should arrive")
        .expect("connection closed event");
    assert!(matches!(
        closed,
        TransportEvent::ConnectionClosed {
            connection_id: closed_connection_id,
        } if closed_connection_id == connection_id
    ));

    shutdown_token.cancel();
    accept_handle.await.expect("acceptor should join");
    assert_socket_path_removed(socket_path.as_path());
}

#[tokio::test]
async fn shutdown_is_only_accepted_on_managed_local_socket_for_its_own_pid() {
    let temp_dir = tempfile::TempDir::new().expect("temp dir");
    let socket_path = test_socket_path(temp_dir.path());
    let (tx, mut rx) = mpsc::channel(CHANNEL_CAPACITY);
    let shutdown = CancellationToken::new();
    let acceptor = start_control_socket_acceptor(
        socket_path.clone(),
        tx,
        shutdown.clone(),
        DaemonShutdownAccess::Disabled,
    )
    .await
    .expect("acceptor");

    let stream = connect_to_socket(socket_path.as_path())
        .await
        .expect("connect");
    assert!(
        client_async("ws://localhost/daemon/shutdown", stream)
            .await
            .is_err()
    );
    assert!(rx.try_recv().is_err());
    shutdown.cancel();
    acceptor.await.expect("acceptor shutdown");

    let (tx, mut rx) = mpsc::channel(CHANNEL_CAPACITY);
    let shutdown = CancellationToken::new();
    let acceptor = start_control_socket_acceptor(
        socket_path.clone(),
        tx,
        shutdown.clone(),
        DaemonShutdownAccess::Managed,
    )
    .await
    .expect("managed acceptor");
    let stream = connect_to_socket(socket_path.as_path())
        .await
        .expect("connect");
    let (mut websocket, _) = client_async("ws://localhost/daemon/shutdown", stream)
        .await
        .expect("upgrade");
    websocket
        .send(WebSocketMessage::Text("0".into()))
        .await
        .expect("wrong pid");
    assert!(!matches!(
        websocket.next().await,
        Some(Ok(WebSocketMessage::Text(_)))
    ));
    assert!(rx.try_recv().is_err());

    let stream = connect_to_socket(socket_path.as_path())
        .await
        .expect("connect");
    let (mut websocket, _) = client_async("ws://localhost/daemon/shutdown", stream)
        .await
        .expect("upgrade");
    let pid = std::process::id().to_string();
    websocket
        .send(WebSocketMessage::Text(pid.clone().into()))
        .await
        .expect("request");
    assert_eq!(
        websocket.next().await.expect("ack").expect("ack frame"),
        WebSocketMessage::Text(pid.into())
    );
    assert!(
        rx.try_recv().is_err(),
        "server must wait until the ack is received"
    );
    websocket.close(None).await.expect("confirm receipt");
    assert!(matches!(
        timeout(Duration::from_secs(2), rx.recv()).await,
        Ok(Some(TransportEvent::DaemonShutdown))
    ));
    shutdown.cancel();
    acceptor.await.expect("acceptor shutdown");
}

#[tokio::test]
async fn app_server_startup_lock_serializes_waiters() {
    let temp_dir = tempfile::TempDir::new().expect("temp dir");
    let lock_path = test_startup_lock_path(temp_dir.path());
    let first_lock = acquire_app_server_startup_lock(lock_path.clone())
        .await
        .expect("first startup lock should succeed");
    let mut second_lock = tokio::spawn(acquire_app_server_startup_lock(lock_path));

    assert!(
        timeout(Duration::from_millis(100), &mut second_lock)
            .await
            .is_err()
    );

    drop(first_lock);
    second_lock
        .await
        .expect("second startup lock task should join")
        .expect("second startup lock should succeed");
}

#[cfg(unix)]
#[tokio::test]
async fn control_socket_rejects_writable_parent_without_changing_permissions() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().unwrap();
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o777)).unwrap();
    let path = directory.path().join("rpc.sock");
    let (tx, _rx) = mpsc::channel(CHANNEL_CAPACITY);
    let error = start_control_socket_acceptor(
        path.clone(),
        tx,
        CancellationToken::new(),
        DaemonShutdownAccess::Disabled,
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    assert_eq!(
        std::fs::metadata(directory.path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o777
    );
    assert!(std::fs::symlink_metadata(path).is_err());
}

#[cfg(unix)]
#[tokio::test]
async fn control_socket_file_is_private_after_bind() {
    use std::os::unix::fs::PermissionsExt;

    let temp_dir = tempfile::TempDir::new().expect("temp dir");
    let socket_path = test_socket_path(temp_dir.path());
    let parent = socket_path.as_path().parent().unwrap();
    std::fs::create_dir_all(parent).unwrap();
    std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o755)).unwrap();
    let (transport_event_tx, _transport_event_rx) =
        mpsc::channel::<TransportEvent>(CHANNEL_CAPACITY);
    let shutdown_token = CancellationToken::new();
    let accept_handle = start_control_socket_acceptor(
        socket_path.clone(),
        transport_event_tx,
        shutdown_token.clone(),
        DaemonShutdownAccess::Disabled,
    )
    .await
    .expect("control socket acceptor should start");

    let metadata = tokio::fs::metadata(socket_path.as_path())
        .await
        .expect("socket metadata should exist");
    assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
    assert_eq!(
        std::fs::metadata(parent).unwrap().permissions().mode() & 0o777,
        0o755
    );
    let physical_path = std::fs::read_link(socket_path.as_path()).expect("rendezvous symlink");
    assert_eq!(
        physical_path.parent(),
        Some(ra_uds::shared_daemon_socket_directory().unwrap().as_path())
    );

    shutdown_token.cancel();
    accept_handle.await.expect("acceptor should join");
    assert!(!physical_path.exists());
    assert!(std::fs::symlink_metadata(socket_path.as_path()).is_err());

    // Simulate a dangling rendezvous left by an interrupted cleanup.
    std::os::unix::fs::symlink(&physical_path, socket_path.as_path()).unwrap();
    let (sender, _receiver) = mpsc::channel::<TransportEvent>(CHANNEL_CAPACITY);
    let shutdown = CancellationToken::new();
    let (first, second) = tokio::join!(
        start_control_socket_acceptor(
            socket_path.clone(),
            sender.clone(),
            shutdown.clone(),
            DaemonShutdownAccess::Disabled,
        ),
        start_control_socket_acceptor(
            socket_path.clone(),
            sender,
            shutdown.clone(),
            DaemonShutdownAccess::Disabled,
        ),
    );
    let (acceptor, error) = match (first, second) {
        (Ok(acceptor), Err(error)) | (Err(error), Ok(acceptor)) => (acceptor, error),
        _ => panic!("exactly one concurrent restart should replace the stale symlink"),
    };
    assert_eq!(error.kind(), std::io::ErrorKind::AddrInUse);
    let _client = connect_to_socket(socket_path.as_path()).await.unwrap();

    // Cleanup must not remove a replacement at the advertised path.
    std::fs::remove_file(socket_path.as_path()).unwrap();
    std::fs::write(socket_path.as_path(), b"replacement").unwrap();
    shutdown.cancel();
    acceptor.await.unwrap();
    assert_eq!(
        std::fs::read(socket_path.as_path()).unwrap(),
        b"replacement"
    );
    assert!(!physical_path.exists());
}

fn test_socket_path(directory: &Path) -> PathBuf {
    app_server_control_socket_path(directory).expect("socket path")
}

fn test_startup_lock_path(directory: &Path) -> PathBuf {
    app_server_startup_lock_path(directory).expect("startup lock path")
}

async fn connect_to_socket(path: &Path) -> std::io::Result<UnixStream> {
    timeout(PATIENCE, connect_control_socket(path))
        .await
        .expect("connect deadline")
}

fn assert_socket_path_removed(path: &Path) {
    assert!(std::fs::symlink_metadata(path).is_err());
}

struct Acceptor {
    _directory: tempfile::TempDir,
    path: PathBuf,
    shutdown: CancellationToken,
    task: JoinHandle<()>,
    events: mpsc::Receiver<TransportEvent>,
}

impl Acceptor {
    async fn start(access: DaemonShutdownAccess) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let path = test_socket_path(directory.path());
        let shutdown = CancellationToken::new();
        let (sender, events) = mpsc::channel(CHANNEL_CAPACITY);
        let task = start_control_socket_acceptor(&path, sender, shutdown.clone(), access)
            .await
            .unwrap();
        Self {
            _directory: directory,
            path,
            shutdown,
            task,
            events,
        }
    }

    async fn connect(&self, url: &str) -> WebSocketStream<UnixStream> {
        let stream = connect_to_socket(&self.path).await.unwrap();
        timeout(PATIENCE, client_async(url, stream))
            .await
            .unwrap()
            .unwrap()
            .0
    }

    async fn event(&mut self) -> TransportEvent {
        timeout(PATIENCE, self.events.recv())
            .await
            .unwrap()
            .expect("transport event")
    }

    async fn opened(
        &mut self,
    ) -> (
        ConnectionId,
        mpsc::Sender<QueuedOutgoingMessage>,
        CancellationToken,
    ) {
        match self.event().await {
            TransportEvent::ConnectionOpened {
                connection_id,
                origin,
                writer,
                disconnect_sender,
                ..
            } => {
                assert_eq!(origin, ConnectionOrigin::WebSocket);
                (connection_id, writer, disconnect_sender.unwrap())
            }
            event => panic!("expected opened: {event:?}"),
        }
    }

    async fn stop(&mut self) {
        self.shutdown.cancel();
        timeout(PATIENCE, &mut self.task).await.unwrap().unwrap();
    }
}

impl Drop for Acceptor {
    fn drop(&mut self) {
        self.shutdown.cancel();
        self.task.abort();
    }
}

#[tokio::test]
async fn local_clients_are_independent_and_receive_messages_with_write_receipts() {
    let mut acceptor = Acceptor::start(DaemonShutdownAccess::Disabled).await;
    let mut first = acceptor.connect("ws://localhost/rpc").await;
    let (first_id, first_writer, first_disconnect) = acceptor.opened().await;
    let mut second = acceptor.connect("ws://localhost/any/path").await;
    let (second_id, second_writer, second_disconnect) = acceptor.opened().await;
    assert_ne!(first_id, second_id);
    assert_eq!(first_writer.capacity(), 32 * 1024);
    for client in [&mut first, &mut second] {
        client
            .send(WebSocketMessage::Text(r#"{"id":1,"method":"test"}"#.into()))
            .await
            .unwrap();
    }
    let mut seen = std::collections::BTreeSet::new();
    for _ in 0..2 {
        match acceptor.event().await {
            TransportEvent::IncomingMessage {
                connection_id,
                message: JsonRpcMessage::Request(request),
            } => {
                assert_eq!(request.id(), &1_i64.into());
                seen.insert(connection_id);
            }
            other => panic!("expected request: {other:?}"),
        }
    }
    assert_eq!(seen, [first_id, second_id].into());
    for (writer, client, value) in [
        (&first_writer, &mut first, "first"),
        (&second_writer, &mut second, "second"),
    ] {
        let (sent, received) = oneshot::channel();
        writer
            .send(
                QueuedOutgoingMessage::new(JsonRpcResponse::new(1_i64, json!(value)))
                    .with_write_complete(sent),
            )
            .await
            .unwrap();
        timeout(PATIENCE, received).await.unwrap().unwrap();
        let message = timeout(PATIENCE, client.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(
            matches!(&message, WebSocketMessage::Text(_)),
            "JSON must use a text frame"
        );
        let text = message.into_text().unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&text).unwrap()["result"],
            value
        );
    }
    first_disconnect.cancel();
    assert!(
        matches!(acceptor.event().await, TransportEvent::ConnectionClosed { connection_id } if connection_id == first_id)
    );
    second
        .send(WebSocketMessage::Text(r#"{"method":"stillOpen"}"#.into()))
        .await
        .unwrap();
    assert!(
        matches!(acceptor.event().await, TransportEvent::IncomingMessage { connection_id, .. } if connection_id == second_id)
    );
    second_disconnect.cancel();
    assert!(
        matches!(acceptor.event().await, TransportEvent::ConnectionClosed { connection_id } if connection_id == second_id)
    );
    acceptor.stop().await;
}

#[tokio::test]
async fn invalid_frames_are_skipped_and_abrupt_disconnect_is_reported() {
    let mut acceptor = Acceptor::start(DaemonShutdownAccess::Disabled).await;
    let mut client = acceptor.connect("ws://localhost/rpc").await;
    let (id, _, _) = acceptor.opened().await;
    for message in [
        WebSocketMessage::Binary(Bytes::from_static(b"binary")),
        WebSocketMessage::Text("not json".into()),
        WebSocketMessage::Text(r#"{"id":1}"#.into()),
        WebSocketMessage::Pong(Bytes::new()),
        WebSocketMessage::Text(r#"{"method":"valid"}"#.into()),
    ] {
        client.send(message).await.unwrap();
    }
    assert!(
        matches!(acceptor.event().await, TransportEvent::IncomingMessage { connection_id,
        message: JsonRpcMessage::Notification(notification) } if connection_id == id && notification.method() == "valid")
    );
    drop(client);
    assert!(
        matches!(acceptor.event().await, TransportEvent::ConnectionClosed { connection_id } if connection_id == id)
    );
    acceptor.stop().await;
}

#[tokio::test]
async fn unix_upgrades_follow_local_permissions_including_an_origin_header() {
    let mut acceptor = Acceptor::start(DaemonShutdownAccess::Disabled).await;
    let stream = connect_to_socket(&acceptor.path).await.unwrap();
    let mut request = "ws://localhost/rpc".into_client_request().unwrap();
    request
        .headers_mut()
        .insert("origin", "https://example.com".parse().unwrap());
    let (mut client, _) = timeout(PATIENCE, client_async(request, stream))
        .await
        .unwrap()
        .unwrap();
    let (id, _, _) = acceptor.opened().await;
    client.close(None).await.unwrap();
    assert!(
        matches!(acceptor.event().await, TransportEvent::ConnectionClosed { connection_id } if connection_id == id)
    );
    acceptor.stop().await;
}

#[tokio::test]
async fn an_incomplete_upgrade_does_not_block_other_connections_or_acceptor_shutdown() {
    let mut acceptor = Acceptor::start(DaemonShutdownAccess::Disabled).await;
    let incomplete = connect_to_socket(&acceptor.path).await.unwrap();
    let client = acceptor.connect("ws://localhost/rpc").await;
    let (_, _, disconnect) = acceptor.opened().await;
    disconnect.cancel();
    assert!(matches!(
        acceptor.event().await,
        TransportEvent::ConnectionClosed { .. }
    ));
    acceptor.stop().await;
    assert_socket_path_removed(&acceptor.path);
    drop((incomplete, client));
}

#[tokio::test(start_paused = true)]
async fn managed_shutdown_waits_two_seconds_for_a_client_that_keeps_the_connection_open() {
    let mut acceptor = Acceptor::start(DaemonShutdownAccess::Managed).await;
    let mut client = acceptor.connect("ws://localhost/daemon/shutdown").await;
    client
        .send(WebSocketMessage::Text(
            std::process::id().to_string().into(),
        ))
        .await
        .unwrap();
    assert_eq!(
        client.next().await.unwrap().unwrap(),
        WebSocketMessage::Text(std::process::id().to_string().into())
    );
    assert!(acceptor.events.try_recv().is_err());
    tokio::time::advance(Duration::from_secs(1)).await;
    assert!(acceptor.events.try_recv().is_err());
    tokio::time::advance(Duration::from_secs(1)).await;
    assert!(matches!(
        acceptor.event().await,
        TransportEvent::DaemonShutdown
    ));
    acceptor.stop().await;
}

#[tokio::test]
async fn managed_shutdown_drains_the_app_server_and_removes_the_socket() {
    let directory = tempfile::tempdir().unwrap();
    let path = test_socket_path(directory.path());
    let server = AppServer::new(directory.path()).unwrap();
    let shutdown = CancellationToken::new();
    let server = server.with_graceful_restart(shutdown.clone());
    let (sender, events) = mpsc::channel(CHANNEL_CAPACITY);
    let acceptor = start_control_socket_acceptor(
        &path,
        sender,
        shutdown.clone(),
        DaemonShutdownAccess::Managed,
    )
    .await
    .unwrap();
    let server = tokio::spawn(server.run(events));
    let stream = connect_to_socket(&path).await.unwrap();
    let (mut client, _) = client_async("ws://localhost/rpc", stream).await.unwrap();
    client
        .send(WebSocketMessage::Text(
            serde_json::to_string(
                &JsonRpcRequest::new(1_i64, "initialize")
                    .with_params(json!({"clientInfo":{"name":"unix-test","version":"1"}})),
            )
            .unwrap()
            .into(),
        ))
        .await
        .unwrap();
    let initialized = timeout(PATIENCE, client.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap()
        .into_text()
        .unwrap();
    assert!(
        serde_json::from_str::<serde_json::Value>(&initialized)
            .unwrap()
            .get("result")
            .is_some()
    );
    let stream = connect_to_socket(&path).await.unwrap();
    let (mut manager, _) = client_async("ws://localhost/daemon/shutdown", stream)
        .await
        .unwrap();
    let pid = std::process::id().to_string();
    manager
        .send(WebSocketMessage::Text(pid.clone().into()))
        .await
        .unwrap();
    assert_eq!(
        manager.next().await.unwrap().unwrap(),
        WebSocketMessage::Text(pid.into())
    );
    manager.close(None).await.unwrap();
    timeout(PATIENCE, server).await.unwrap().unwrap();
    timeout(PATIENCE, acceptor).await.unwrap().unwrap();
    assert!(shutdown.is_cancelled());
    assert_socket_path_removed(&path);
    assert!(!matches!(
        timeout(PATIENCE, client.next()).await.unwrap(),
        Some(Ok(WebSocketMessage::Text(_)))
    ));
}

#[tokio::test]
async fn stale_socket_is_replaced_but_existing_files_and_unrelated_symlinks_are_preserved() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("rpc.sock");
    let (sender, _events) = mpsc::channel(CHANNEL_CAPACITY);
    let shutdown = CancellationToken::new();
    drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
    let acceptor = start_control_socket_acceptor(
        &path,
        sender.clone(),
        shutdown.clone(),
        DaemonShutdownAccess::Disabled,
    )
    .await
    .unwrap();
    let physical = std::fs::read_link(&path).unwrap();
    shutdown.cancel();
    timeout(PATIENCE, acceptor).await.unwrap().unwrap();
    assert!(!physical.exists());
    for kind in ["file", "directory", "symlink", "dangling"] {
        let target = directory.path().join("target");
        match kind {
            "file" => std::fs::write(&path, "keep").unwrap(),
            "directory" => std::fs::create_dir(&path).unwrap(),
            "symlink" => {
                std::fs::write(&target, "keep").unwrap();
                std::os::unix::fs::symlink(&target, &path).unwrap();
            }
            "dangling" => std::os::unix::fs::symlink(&target, &path).unwrap(),
            _ => unreachable!(),
        }
        assert!(
            start_control_socket_acceptor(
                &path,
                sender.clone(),
                CancellationToken::new(),
                DaemonShutdownAccess::Disabled
            )
            .await
            .is_err(),
            "{kind}"
        );
        assert!(
            std::fs::symlink_metadata(&path).is_ok(),
            "{kind} must remain"
        );
        if kind == "directory" {
            std::fs::remove_dir(&path).unwrap();
        } else {
            std::fs::remove_file(&path).unwrap();
        }
        if target.exists() {
            assert_eq!(std::fs::read(&target).unwrap(), b"keep");
            std::fs::remove_file(target).unwrap();
        }
    }
}

#[tokio::test]
async fn relative_socket_and_lock_paths_are_refused_and_path_helpers_resolve_the_supplied_home() {
    let directory = tempfile::tempdir().unwrap();
    let (sender, _) = mpsc::channel(CHANNEL_CAPACITY);
    assert_eq!(
        start_control_socket_acceptor(
            "relative.sock",
            sender,
            CancellationToken::new(),
            DaemonShutdownAccess::Disabled
        )
        .await
        .unwrap_err()
        .kind(),
        ErrorKind::InvalidInput
    );
    assert!(
        matches!(acquire_app_server_startup_lock("relative.lock").await, Err(error) if error.kind() == ErrorKind::InvalidInput)
    );
    assert_eq!(
        app_server_control_socket_path(directory.path()).unwrap(),
        directory
            .path()
            .join("app-server-control/app-server-control.sock")
    );
    assert_eq!(
        app_server_startup_lock_path(directory.path()).unwrap(),
        directory
            .path()
            .join("app-server-control/app-server-startup.lock")
    );
}

#[tokio::test]
async fn private_lock_directory_is_repaired_but_symlinks_are_refused() {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir().unwrap();
    let lock_path = test_startup_lock_path(directory.path());
    let parent = lock_path.parent().unwrap();
    std::fs::create_dir(parent).unwrap();
    std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o755)).unwrap();
    drop(acquire_app_server_startup_lock(&lock_path).await.unwrap());
    assert_eq!(
        std::fs::metadata(parent).unwrap().permissions().mode() & 0o777,
        0o700
    );
    std::fs::remove_file(&lock_path).unwrap();
    std::fs::remove_dir(parent).unwrap();
    std::os::unix::fs::symlink(directory.path(), parent).unwrap();
    assert!(
        matches!(acquire_app_server_startup_lock(&lock_path).await, Err(error) if error.kind() == ErrorKind::AlreadyExists)
    );
}

#[tokio::test]
async fn a_sticky_writable_parent_is_accepted_and_keeps_its_permissions() {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir().unwrap();
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o1777)).unwrap();
    let path = directory.path().join("rpc.sock");
    let (sender, _events) = mpsc::channel(CHANNEL_CAPACITY);
    let shutdown = CancellationToken::new();
    let acceptor = start_control_socket_acceptor(
        &path,
        sender,
        shutdown.clone(),
        DaemonShutdownAccess::Disabled,
    )
    .await
    .unwrap();
    let stream = connect_to_socket(&path).await.unwrap();
    assert_eq!(
        std::fs::metadata(directory.path())
            .unwrap()
            .permissions()
            .mode()
            & 0o7777,
        0o1777
    );
    shutdown.cancel();
    timeout(PATIENCE, acceptor).await.unwrap().unwrap();
    assert_socket_path_removed(&path);
    drop(stream);
}
