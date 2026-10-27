//! Exercise the vendored environment-proxy path in an isolated child process.
use futures::{SinkExt, StreamExt};
use ra_http::{
    HttpClientFactory, OutboundProxyPolicy,
    websocket::{WebSocketConnector, WebSocketTlsMode},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::{
    Message, client::IntoClientRequest, protocol::WebSocketConfig,
};

#[allow(
    clippy::result_large_err,
    reason = "The WebSocket handshake callback retains its typed protocol error."
)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn environment_proxy_preserves_target_and_completes_websocket_upgrade() {
    const CHILD: &str = "RA_EXECUTOR_PROXY_TEST_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let factory = HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault);
        let connector =
            WebSocketConnector::new_with_tls_mode(&factory, WebSocketTlsMode::TungsteniteDefault)
                .unwrap();
        let (mut connection, _) = connector
            .connect(
                "ws://executor-proxy-test.invalid:8765/plugin"
                    .into_client_request()
                    .unwrap(),
                WebSocketConfig::default(),
            )
            .await
            .unwrap();
        assert_eq!(
            connection.next().await.unwrap().unwrap(),
            Message::text("proxied")
        );
        connection.close().await.unwrap();
        return;
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        while !request.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            stream.read_exact(&mut byte).await.unwrap();
            request.push(byte[0]);
            assert!(request.len() < 8192);
        }
        let request = String::from_utf8(request).unwrap();
        assert!(request.starts_with("CONNECT executor-proxy-test.invalid:8765 HTTP/1.1\r\n"));
        stream
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await
            .unwrap();
        let mut ws = tokio_tungstenite::accept_hdr_async(
            stream,
            |request: &tokio_tungstenite::tungstenite::handshake::server::Request, response| {
                assert_eq!(request.uri().path(), "/plugin");
                assert_eq!(
                    request.headers()["host"],
                    "executor-proxy-test.invalid:8765"
                );
                Ok(response)
            },
        )
        .await
        .unwrap();
        ws.send(Message::text("proxied")).await.unwrap();
        let _ = ws.next().await;
    });
    let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "environment_proxy_preserves_target_and_completes_websocket_upgrade",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .env("HTTP_PROXY", &proxy_url)
        .env_remove("http_proxy")
        .env_remove("HTTPS_PROXY")
        .env_remove("https_proxy")
        .env_remove("ALL_PROXY")
        .env_remove("all_proxy")
        .env("NO_PROXY", "")
        .env_remove("no_proxy");
    let output = tokio::time::timeout(std::time::Duration::from_secs(10), command.output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        output.status.success(),
        "child failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    tokio::time::timeout(std::time::Duration::from_secs(2), server)
        .await
        .unwrap()
        .unwrap();
}
