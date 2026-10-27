// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
use futures::SinkExt;
use futures::StreamExt;
use http::HeaderMap;
use ra_executor_protocol::JSONRPCMessage;
use ra_executor_protocol::JSONRPCNotification;
use ra_executor_protocol::JSONRPCResponse;
use ra_http::HttpClientFactory;
use ra_http::OutboundProxyPolicy;
use ra_paths::PathUri;

use std::collections::HashMap;
#[cfg(unix)]
use std::path::Path;
#[cfg(unix)]
use std::process::Command;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::net::TcpStream;
use tokio::sync::oneshot;
use tokio::sync::watch;
use tokio::time::Duration;
#[cfg(unix)]
use tokio::time::sleep;
use tokio::time::timeout;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;

use ra_executor::EnvironmentObservedStatus;
use ra_executor::test_api::client::ExecServerClient;
use ra_executor::test_api::client::ExecServerError;
use ra_executor::test_api::client::LazyRemoteExecServerClient;

#[cfg(not(windows))]
use ra_executor::test_api::client_api::DEFAULT_REMOTE_EXEC_SERVER_INITIALIZE_TIMEOUT;
use ra_executor::test_api::client_api::ExecServerTransportParams;
use ra_executor::test_api::client_api::RemoteExecServerConnectArgs;
use ra_executor::test_api::client_api::StdioExecServerCommand;
use ra_executor::test_api::client_api::StdioExecServerConnectArgs;

use ra_executor::protocol::EXEC_OUTPUT_DELTA_METHOD;
use ra_executor::protocol::EnvironmentInfo;
use ra_executor::protocol::FS_OPEN_METHOD;
use ra_executor::protocol::FsOpenMode;
use ra_executor::protocol::FsOpenParams;
use ra_executor::protocol::FsWriteBlockParams;
use ra_executor::protocol::INITIALIZE_METHOD;
use ra_executor::protocol::INITIALIZED_METHOD;
use ra_executor::protocol::InitializeResponse;
async fn accept_websocket(listener: &TcpListener) -> WebSocketStream<TcpStream> {
    let (stream, _) = listener.accept().await.expect("listener should accept");
    accept_async(stream)
        .await
        .expect("websocket handshake should succeed")
}
async fn read_jsonrpc_websocket(websocket: &mut WebSocketStream<TcpStream>) -> JSONRPCMessage {
    loop {
        match timeout(Duration::from_secs(1), websocket.next())
            .await
            .expect("json-rpc websocket read should not time out")
            .expect("websocket should stay open")
            .expect("websocket frame should read")
        {
            Message::Text(text) => {
                return serde_json::from_str(text.as_ref())
                    .expect("json-rpc text frame should parse");
            }
            Message::Binary(bytes) => {
                return serde_json::from_slice(bytes.as_ref())
                    .expect("json-rpc binary frame should parse");
            }
            Message::Ping(_) | Message::Pong(_) => {}
            other => panic!("expected json-rpc websocket frame, got {other:?}"),
        }
    }
}
async fn write_jsonrpc_websocket(
    websocket: &mut WebSocketStream<TcpStream>,
    message: JSONRPCMessage,
) {
    let encoded = serde_json::to_string(&message).expect("json-rpc should serialize");
    websocket
        .send(Message::Text(encoded.into()))
        .await
        .expect("json-rpc websocket frame should write");
}
async fn complete_websocket_initialize(
    websocket: &mut WebSocketStream<TcpStream>,
    session_id: &str,
    expected_resume_session_id: Option<&str>,
) {
    complete_websocket_initialize_with_environment_info(
        websocket,
        session_id,
        expected_resume_session_id,
        None,
    )
    .await;
}
async fn complete_websocket_initialize_with_environment_info(
    websocket: &mut WebSocketStream<TcpStream>,
    session_id: &str,
    expected_resume_session_id: Option<&str>,
    environment_info: Option<EnvironmentInfo>,
) {
    let initialize = read_jsonrpc_websocket(websocket).await;
    let request = match initialize {
        JSONRPCMessage::Request(request) if request.method == INITIALIZE_METHOD => request,
        other => panic!("expected initialize request, got {other:?}"),
    };
    let params: ra_executor::protocol::InitializeParams =
        serde_json::from_value(request.params.expect("initialize params should exist"))
            .expect("initialize params should deserialize");
    assert_eq!(
        params.resume_session_id.as_deref(),
        expected_resume_session_id
    );
    write_jsonrpc_websocket(
        websocket,
        JSONRPCMessage::Response(JSONRPCResponse {
            id: request.id,
            result: serde_json::to_value(InitializeResponse {
                session_id: session_id.to_string(),
                environment_info,
            })
            .expect("initialize response should serialize"),
        }),
    )
    .await;
    let initialized = read_jsonrpc_websocket(websocket).await;
    match initialized {
        JSONRPCMessage::Notification(notification) if notification.method == INITIALIZED_METHOD => {
        }
        other => panic!("expected initialized notification, got {other:?}"),
    }
}
#[cfg(not(windows))]
#[tokio::test]
async fn connect_stdio_command_initializes_json_rpc_client() {
    let client = ExecServerClient::connect_stdio_command(StdioExecServerConnectArgs {
        command: StdioExecServerCommand {
            program: "sh".to_string(),
            args: vec![
                "-c".to_string(),
                "read _line; printf '%s\\n' '{\"id\":1,\"result\":{\"sessionId\":\"stdio-test\"}}'; read _line; sleep 60".to_string(),
            ],
            env: HashMap::new(),
            cwd: None,
        },
        client_name: "stdio-test-client".to_string(),
        initialize_timeout: Duration::from_secs(1),
        resume_session_id: None,
    })
    .await
    .expect("stdio client should connect");

    assert_eq!(client.session_id().as_deref(), Some("stdio-test"));
}

#[cfg(not(windows))]
#[tokio::test]
async fn connect_for_transport_initializes_stdio_command() {
    let client = ra_executor::test_api::client::connect_for_transport(
        ExecServerTransportParams::StdioCommand {
            command: StdioExecServerCommand {
                program: "sh".to_string(),
                args: vec![
                    "-c".to_string(),
                    "read _line; printf '%s\\n' '{\"id\":1,\"result\":{\"sessionId\":\"stdio-test\"}}'; read _line; sleep 60".to_string(),
                ],
                env: HashMap::new(),
                cwd: None,
            },
            initialize_timeout: DEFAULT_REMOTE_EXEC_SERVER_INITIALIZE_TIMEOUT,
        },
        ra_http::HttpClientFactory::new(
            ra_http::OutboundProxyPolicy::ReqwestDefault,
        ),
        /*started*/ &mut None,
    )
    .await
    .expect("stdio transport should connect");

    assert_eq!(client.session_id().as_deref(), Some("stdio-test"));
}

#[cfg(windows)]
#[tokio::test]
async fn connect_stdio_command_initializes_json_rpc_client_on_windows() {
    let client = ExecServerClient::connect_stdio_command(StdioExecServerConnectArgs {
        command: StdioExecServerCommand {
            program: "powershell".to_string(),
            args: vec![
                "-NoProfile".to_string(),
                "-Command".to_string(),
                "$null = [Console]::In.ReadLine(); [Console]::Out.WriteLine('{\"id\":1,\"result\":{\"sessionId\":\"stdio-test\"}}'); $null = [Console]::In.ReadLine(); Start-Sleep -Seconds 60".to_string(),
            ],
            env: HashMap::new(),
            cwd: None,
        },
        client_name: "stdio-test-client".to_string(),
        initialize_timeout: Duration::from_secs(1),
        resume_session_id: None,
    })
    .await
    .expect("stdio client should connect");

    assert_eq!(client.session_id().as_deref(), Some("stdio-test"));
}

#[cfg(unix)]
#[tokio::test]
async fn dropping_stdio_client_terminates_spawned_process() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let pid_file = tempdir.path().join("server.pid");
    let child_pid_file = tempdir.path().join("server-child.pid");
    let stdio_script = format!(
        "read _line; \
             echo \"$$\" > {}; \
             sleep 60 >/dev/null 2>&1 & echo \"$!\" > {}; \
             printf '%s\\n' '{{\"id\":1,\"result\":{{\"sessionId\":\"stdio-test\"}}}}'; \
             read _line; \
             wait",
        shell_quote(pid_file.as_path()),
        shell_quote(child_pid_file.as_path()),
    );
    let client = ExecServerClient::connect_stdio_command(StdioExecServerConnectArgs {
        command: StdioExecServerCommand {
            program: "sh".to_string(),
            args: vec!["-c".to_string(), stdio_script],
            env: HashMap::new(),
            cwd: None,
        },
        client_name: "stdio-test-client".to_string(),
        initialize_timeout: Duration::from_secs(1),
        resume_session_id: None,
    })
    .await
    .expect("stdio client should connect");
    let server_pid = read_pid_file(pid_file.as_path()).await;
    let child_pid = read_pid_file(child_pid_file.as_path()).await;
    assert!(
        process_exists(server_pid),
        "spawned stdio process should be running before client drop"
    );
    assert!(
        process_exists(child_pid),
        "spawned stdio child process should be running before client drop"
    );
    drop(client);
    wait_for_process_exit(server_pid).await;
    wait_for_process_exit(child_pid).await;
}
#[cfg(unix)]
#[tokio::test]
async fn malformed_stdio_message_terminates_spawned_process() {
    let tempdir = tempfile::tempdir().expect("tempdir should be created");
    let pid_file = tempdir.path().join("server.pid");
    let stdio_script = format!(
        "read _line; \
             echo \"$$\" > {}; \
             printf '%s\\n' 'not-json'; \
             sleep 60",
        shell_quote(pid_file.as_path()),
    );
    let result = ExecServerClient::connect_stdio_command(StdioExecServerConnectArgs {
        command: StdioExecServerCommand {
            program: "sh".to_string(),
            args: vec!["-c".to_string(), stdio_script],
            env: HashMap::new(),
            cwd: None,
        },
        client_name: "stdio-test-client".to_string(),
        initialize_timeout: Duration::from_secs(1),
        resume_session_id: None,
    })
    .await;
    assert!(result.is_err(), "malformed stdio server should not connect");
    let server_pid = read_pid_file(pid_file.as_path()).await;
    wait_for_process_exit(server_pid).await;
}
#[cfg(unix)]
async fn read_pid_file(path: &Path) -> u32 {
    for _ in 0..20 {
        if let Ok(contents) = std::fs::read_to_string(path) {
            return contents
                .trim()
                .parse()
                .expect("pid file should contain a pid");
        }
        sleep(Duration::from_millis(50)).await;
    }
    panic!("pid file {} should be written", path.display());
}
#[cfg(unix)]
async fn wait_for_process_exit(pid: u32) {
    for _ in 0..20 {
        if !process_exists(pid) {
            return;
        }
        sleep(Duration::from_millis(100)).await;
    }
    panic!("process {pid} should exit");
}
#[cfg(unix)]
fn process_exists(pid: u32) -> bool {
    Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .status()
        .is_ok_and(|status| status.success())
}
#[cfg(unix)]
fn shell_quote(path: &Path) -> String {
    let value = path.to_string_lossy();
    format!("'{}'", value.replace('\'', "'\\''"))
}
#[test_case::test_case(Some(ra_executor::test_api::environment::local_environment_info()); "from_initialize")]
#[test_case::test_case(
    Some(EnvironmentInfo {
        executor_version: "1.2.3-alpha.4".to_string(),
        provider_id: Some("sha256:fb4f62da3e84f6864dcec8ede7bc66f1c96ecaeaf55f8a786b85df994057c8ac".to_string()),
        ..ra_executor::test_api::environment::local_environment_info()
    });
    "with_executor_metadata"
)]
#[test_case::test_case(None; "legacy_server")]
#[tokio::test]
async fn environment_info_is_cached(
    initial_environment_info: Option<EnvironmentInfo>,
) -> anyhow::Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let websocket_url = format!("ws://{}", listener.local_addr()?);
    let expected_info = initial_environment_info
        .clone()
        .unwrap_or_else(ra_executor::test_api::environment::local_environment_info);
    let server_info = expected_info.clone();
    let server = tokio::spawn(async move {
        let mut websocket = accept_websocket(&listener).await;
        complete_websocket_initialize_with_environment_info(
            &mut websocket,
            "session-1",
            None,
            initial_environment_info.clone(),
        )
        .await;
        if initial_environment_info.is_none() {
            let JSONRPCMessage::Request(request) = read_jsonrpc_websocket(&mut websocket).await
            else {
                panic!("expected environment info request");
            };
            assert_eq!(request.method, "environment/info");
            write_jsonrpc_websocket(
                &mut websocket,
                JSONRPCMessage::Response(JSONRPCResponse {
                    id: request.id,
                    result: serde_json::to_value(server_info)
                        .expect("environment info should serialize"),
                }),
            )
            .await;
        }
    });
    let client = ExecServerClient::connect_websocket(RemoteExecServerConnectArgs {
        websocket_url,
        client_name: "metadata-test".to_string(),
        connect_timeout: Duration::from_secs(1),
        initialize_timeout: Duration::from_secs(1),
        resume_session_id: None,
        http_client_factory: HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
    })
    .await?;
    assert_eq!(client.recovery_timeout_for_test(), Duration::from_secs(25));
    let info = client.environment_info().await?;
    assert_eq!(info.as_ref(), &expected_info);
    server.await?;
    let cached_info = client.clone().environment_info().await?;
    assert_eq!(cached_info.as_ref(), &expected_info);
    assert!(Arc::ptr_eq(&info, &cached_info));
    Ok(())
}
#[doc = " Old executors must never receive writable opens they would silently treat as reads."]
#[tokio::test]
async fn writable_file_streams_require_executor_capability() -> anyhow::Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let websocket_url = format!("ws://{}", listener.local_addr()?);
    let read_open = FsOpenParams {
        handle_id: "read-handle".to_string(),
        path: PathUri::parse("file:///tmp/existing.txt")?,
        mode: FsOpenMode::Read,
        sandbox: None,
    };
    let server = tokio::spawn(async move {
        let mut websocket = accept_websocket(&listener).await;
        let mut info = ra_executor::test_api::environment::local_environment_info();
        info.capabilities.file_write_streaming = false;
        complete_websocket_initialize_with_environment_info(
            &mut websocket,
            "session-1",
            None,
            Some(info),
        )
        .await;
        let JSONRPCMessage::Request(request) = read_jsonrpc_websocket(&mut websocket).await else {
            panic!("expected read-only open");
        };
        assert_eq!(request.method, FS_OPEN_METHOD);
        assert_eq!(
            request.params,
            Some(serde_json::json!({
                "handleId": "read-handle",
                "path": "file:///tmp/existing.txt",
                "mode": "read",
                "sandbox": null,
            }))
        );
        write_jsonrpc_websocket(
            &mut websocket,
            JSONRPCMessage::Response(JSONRPCResponse {
                id: request.id,
                result: serde_json::json!({ "handleId": "read-handle" }),
            }),
        )
        .await;
    });
    let client = ExecServerClient::connect_websocket(RemoteExecServerConnectArgs::new(
        websocket_url,
        "file-stream-test".to_string(),
        HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
    ))
    .await?;
    let error = client
        .fs_open(FsOpenParams {
            mode: FsOpenMode::Replace,
            ..read_open.clone()
        })
        .await
        .unwrap_err();
    assert!(matches!(error, ExecServerError::Protocol(_)));
    let error = client
        .fs_write_block(FsWriteBlockParams {
            handle_id: "write-handle".to_string(),
            offset: 0,
            chunk: vec![1].into(),
        })
        .await
        .unwrap_err();
    assert!(matches!(error, ExecServerError::Protocol(_)));
    client.fs_open(read_open).await?;
    server.await?;
    Ok(())
}
#[test_case::test_case(false; "socket_closed")]
#[test_case::test_case(true; "health_check_timed_out")]
#[tokio::test]
async fn remote_websocket_client_resumes_session(health_check_timeout: bool) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener should bind");
    let websocket_url = format!(
        "ws://{}",
        listener.local_addr().expect("listener should have address")
    );
    let (resumed_tx, resumed_rx) = oneshot::channel();
    let (finish_tx, finish_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        let mut first = accept_websocket(&listener).await;
        complete_websocket_initialize(&mut first, "session-1", None).await;
        if health_check_timeout {
            let JSONRPCMessage::Request(request) = read_jsonrpc_websocket(&mut first).await else {
                panic!("expected environment info request");
            };
            assert_eq!(request.method, "environment/info");
            while let Some(Ok(message)) = first.next().await {
                assert!(matches!(message, Message::Ping(_) | Message::Close(_)));
                if matches!(message, Message::Close(_)) {
                    break;
                }
            }
        } else {
            first.close(None).await.expect("websocket should close");
        }
        let mut resumed = accept_websocket(&listener).await;
        complete_websocket_initialize(&mut resumed, "session-1", Some("session-1")).await;
        resumed_tx.send(()).expect("resume should signal");
        finish_rx.await.expect("test should finish");
    });
    let client = LazyRemoteExecServerClient::new(
        ExecServerTransportParams::WebSocketUrl {
            websocket_url,
            connect_timeout: Duration::from_secs(1),
            initialize_timeout: Duration::from_secs(1),
            http_headers: HeaderMap::new(),
        },
        HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
    );
    let stable_client = client.get().await.expect("client should connect");
    if health_check_timeout {
        let error = stable_client
            .force_environment_info()
            .await
            .expect_err("unanswered health check should time out");
        assert!(error.to_string().contains("timed out"));
    }
    timeout(Duration::from_secs(1), resumed_rx)
        .await
        .expect("session resume should not time out")
        .expect("session resume should signal");
    let reused_client = client.get().await.expect("client should stay connected");
    assert_eq!(stable_client.session_id().as_deref(), Some("session-1"));
    assert!(stable_client.shares_connection(&reused_client));
    finish_tx.send(()).expect("test should finish");
    server.await.expect("server task should finish");
}
#[tokio::test]
async fn explicit_resume_drains_notifications_before_initialize_response() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener should bind");
    let websocket_url = format!(
        "ws://{}",
        listener.local_addr().expect("listener should have address")
    );
    let (initialized_tx, initialized_rx) = oneshot::channel();
    let (finish_tx, finish_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        let mut websocket = accept_websocket(&listener).await;
        let initialize = read_jsonrpc_websocket(&mut websocket).await;
        let request = match initialize {
            JSONRPCMessage::Request(request) if request.method == INITIALIZE_METHOD => request,
            other => panic!("expected initialize request, got {other:?}"),
        };
        let params: ra_executor::protocol::InitializeParams =
            serde_json::from_value(request.params.expect("initialize params should exist"))
                .expect("initialize params should deserialize");
        assert_eq!(params.resume_session_id.as_deref(), Some("session-1"));
        for seq in 1..=256 {
            write_jsonrpc_websocket(
                &mut websocket,
                JSONRPCMessage::Notification(JSONRPCNotification {
                    method: EXEC_OUTPUT_DELTA_METHOD.to_string(),
                    params: Some(
                        serde_json::to_value(serde_json::json!({
                            "processId": "busy-process",
                            "seq": seq,
                            "stream": "stdout",
                            "chunk": "b3V0cHV0",
                        }))
                        .expect("output notification should serialize"),
                    ),
                }),
            )
            .await;
        }
        write_jsonrpc_websocket(
            &mut websocket,
            JSONRPCMessage::Response(JSONRPCResponse {
                id: request.id,
                result: serde_json::to_value(InitializeResponse {
                    session_id: "session-1".to_string(),
                    environment_info: None,
                })
                .expect("initialize response should serialize"),
            }),
        )
        .await;
        let initialized = read_jsonrpc_websocket(&mut websocket).await;
        match initialized {
            JSONRPCMessage::Notification(notification)
                if notification.method == INITIALIZED_METHOD => {}
            other => panic!("expected initialized notification, got {other:?}"),
        }
        initialized_tx
            .send(())
            .expect("initialized notification should signal");
        finish_rx.await.expect("test should finish");
    });
    let client = timeout(
        Duration::from_secs(1),
        ExecServerClient::connect_websocket(RemoteExecServerConnectArgs {
            websocket_url,
            client_name: "test-client".to_string(),
            connect_timeout: Duration::from_secs(1),
            initialize_timeout: Duration::from_secs(1),
            resume_session_id: Some("session-1".to_string()),
            http_client_factory: ra_http::HttpClientFactory::new(
                ra_http::OutboundProxyPolicy::ReqwestDefault,
            ),
        }),
    )
    .await
    .expect("explicit resume should not time out")
    .expect("explicit resume should connect");
    assert_eq!(client.session_id().as_deref(), Some("session-1"));
    timeout(Duration::from_secs(1), initialized_rx)
        .await
        .expect("initialized notification should not time out")
        .expect("initialized notification should signal");
    finish_tx.send(()).expect("test should finish");
    server.await.expect("server task should finish");
}
#[tokio::test]
async fn initial_connection_is_shared_by_all_waiters() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener should bind");
    let websocket_url = format!(
        "ws://{}",
        listener.local_addr().expect("listener should have address")
    );
    let server = tokio::spawn(async move {
        let mut connection = accept_websocket(&listener).await;
        complete_websocket_initialize(&mut connection, "startup-session", None).await;
        timeout(Duration::from_secs(1), connection.next())
            .await
            .expect("client should close after the test");
    });
    let client = LazyRemoteExecServerClient::new(
        ExecServerTransportParams::WebSocketUrl {
            websocket_url,
            connect_timeout: Duration::from_secs(1),
            initialize_timeout: Duration::from_secs(1),
            http_headers: HeaderMap::new(),
        },
        HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
    );
    assert!(!client.startup_finished());
    let _startup_task = client.start_connecting();
    let (ready, first, second) =
        tokio::join!(client.wait_until_ready(), client.get(), client.get());
    ready.expect("background startup should finish");
    let first = first.expect("first waiter should receive the client");
    let second = second.expect("second waiter should receive the same client");
    assert!(client.startup_finished());
    assert_eq!(first.session_id().as_deref(), Some("startup-session"));
    assert!(first.shares_connection(&second));
    drop(first);
    drop(second);
    drop(client);
    server.await.expect("server task should finish");
}
#[tokio::test]
async fn terminal_stdio_startup_failure_is_remembered() {
    let client = LazyRemoteExecServerClient::new(
        ExecServerTransportParams::StdioCommand {
            command: StdioExecServerCommand {
                program: "codex-missing-exec-server-for-test".to_string(),
                args: Vec::new(),
                env: HashMap::new(),
                cwd: None,
            },
            initialize_timeout: Duration::from_secs(1),
        },
        HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
    );
    assert!(client.start_connecting().is_none());
    assert!(!client.startup_finished());
    let first = match client.get().await {
        Ok(_) => panic!("missing executable should fail"),
        Err(error) => error,
    };
    assert!(client.startup_finished());
    let second = match client.get().await {
        Ok(_) => panic!("burned environment should stay failed"),
        Err(error) => error,
    };
    assert!(matches!(
        client.status().await,
        EnvironmentObservedStatus::Disconnected { .. }
    ));
    let (
        ra_executor::test_api::client::ExecServerError::ConnectionAttempt(first),
        ra_executor::test_api::client::ExecServerError::ConnectionAttempt(second),
    ) = (first, second)
    else {
        panic!("expected saved connection failures");
    };
    assert!(Arc::ptr_eq(&first, &second));
}
#[tokio::test]
async fn retryable_startup_failure_does_not_burn_environment() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener should bind");
    let websocket_url = format!(
        "ws://{}",
        listener.local_addr().expect("listener should have address")
    );
    let (replacement_initialized_tx, replacement_initialized_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut failed_startup, _) = listener.accept().await.expect("startup should arrive");
        failed_startup
            .write_all(b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n")
            .await
            .expect("failed handshake response should write");
        let mut replacement = accept_websocket(&listener).await;
        complete_websocket_initialize(&mut replacement, "replacement-session", None).await;
        replacement_initialized_tx
            .send(())
            .expect("replacement initialization should be observed");
        timeout(Duration::from_secs(1), replacement.next())
            .await
            .expect("client should close after the test");
    });
    let client = LazyRemoteExecServerClient::new(
        ExecServerTransportParams::WebSocketUrl {
            websocket_url,
            connect_timeout: Duration::from_secs(1),
            initialize_timeout: Duration::from_secs(1),
            http_headers: HeaderMap::new(),
        },
        HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
    );
    let (observed_tx, mut observed_rx) = tokio::sync::mpsc::unbounded_channel();
    assert!(
        client
            .connection_observer
            .set(Arc::new(move |_, outcome| {
                observed_tx
                    .send(outcome)
                    .expect("record connection outcome");
            }))
            .is_ok()
    );
    let failed_startup = match client.get().await {
        Ok(_) => panic!("initial connection should fail"),
        Err(error) => error,
    };
    assert!(matches!(
        failed_startup,
        ra_executor::test_api::client::ExecServerError::ConnectionAttempt(_)
    ));
    assert!(client.startup_finished());
    assert!(matches!(
        client.status().await,
        EnvironmentObservedStatus::Disconnected { .. }
    ));
    let (ready, first, second) =
        tokio::join!(client.wait_until_ready(), client.get(), client.get());
    ready.expect("later readiness check should retry startup");
    let first = first.expect("first waiter should receive the replacement client");
    let second = second.expect("second waiter should receive the same replacement client");
    assert_eq!(first.session_id().as_deref(), Some("replacement-session"));
    assert!(first.shares_connection(&second));
    replacement_initialized_rx
        .await
        .expect("server should observe replacement initialization");
    assert_eq!(
        std::iter::from_fn(|| observed_rx.try_recv().ok()).collect::<Vec<_>>(),
        [
            ra_executor::ConnectionAttemptOutcome::Failure,
            ra_executor::ConnectionAttemptOutcome::Success
        ],
    );
    drop(first);
    drop(second);
    drop(client);
    server.await.expect("server task should finish");
}
#[tokio::test]
async fn failed_reconnect_does_not_burn_environment() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener should bind");
    let websocket_url = format!(
        "ws://{}",
        listener.local_addr().expect("listener should have address")
    );
    let (replacement_initialized_tx, replacement_initialized_rx) = oneshot::channel();
    let (allow_replacement_tx, allow_replacement_rx) = watch::channel(false);
    let server = tokio::spawn(async move {
        let mut first = accept_websocket(&listener).await;
        complete_websocket_initialize(&mut first, "startup-session", None).await;
        first
            .close(None)
            .await
            .expect("startup websocket should close");
        let successful_reconnect = loop {
            let (stream, _) = listener.accept().await.expect("reconnect should arrive");
            if *allow_replacement_rx.borrow() {
                break stream;
            }
            let mut failed_reconnect = stream;
            failed_reconnect
                .write_all(b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n")
                .await
                .expect("failed handshake response should write");
        };
        let mut successful_reconnect = accept_async(successful_reconnect)
            .await
            .expect("replacement websocket handshake should succeed");
        complete_websocket_initialize(&mut successful_reconnect, "replacement-session", None).await;
        replacement_initialized_tx
            .send(())
            .expect("replacement initialization should be observed");
        timeout(Duration::from_secs(1), successful_reconnect.next())
            .await
            .expect("client should close after the test");
    });
    let client = LazyRemoteExecServerClient::with_recovery_timeout_for_test(
        ExecServerTransportParams::WebSocketUrl {
            websocket_url,
            connect_timeout: Duration::from_secs(1),
            initialize_timeout: Duration::from_secs(1),
            http_headers: HeaderMap::new(),
        },
        HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
        Duration::from_millis(500),
    );
    let initial = client.get().await.expect("startup should connect");
    assert_eq!(
        initial.recovery_timeout_for_test(),
        Duration::from_millis(500)
    );
    timeout(Duration::from_secs(1), async {
        while !ra_executor::test_api::client::is_disconnected(&initial) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("client should observe disconnect");
    let failed_reconnect = match client.get().await {
        Ok(_) => panic!("first lazy reconnect should fail"),
        Err(error) => error,
    };
    assert!(matches!(
        failed_reconnect,
        ra_executor::test_api::client::ExecServerError::ConnectionAttempt(_)
    ));
    allow_replacement_tx
        .send(true)
        .expect("server should allow a fresh client");
    let replacement = client.get().await.expect("later reconnect should succeed");
    assert_eq!(
        replacement.session_id().as_deref(),
        Some("replacement-session")
    );
    replacement_initialized_rx
        .await
        .expect("server should observe replacement initialization");
    drop(initial);
    drop(replacement);
    drop(client);
    server.await.expect("server task should finish");
}
