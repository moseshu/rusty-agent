//! Cleanup diagnostics follow Python's credential-safe MCP server naming.

use ra_mcp::{
    McpServer,
    client::stdio::{McpServerStdio, McpStdioParams},
};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

fn params() -> McpStdioParams {
    McpStdioParams::new(env!("CARGO_BIN_EXE_mcp_fixture"))
}

#[derive(Clone, Default)]
struct CapturedLogs(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for CapturedLogs {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturedLogs {
    type Writer = Self;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

#[tokio::test]
async fn cleanup_and_reconnect_logs_redact_server_name_credentials() {
    let captured = CapturedLogs::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(captured.clone())
        .with_max_level(tracing::Level::WARN)
        .with_ansi(false)
        .without_time()
        .finish();
    // This integration binary owns its subscriber so other tests cannot affect callsite caching.
    tracing::subscriber::set_global_default(subscriber).unwrap();
    for (name, safe_name) in [
        (
            "https://review-user:review-password@example.test/mcp?token=review-query#review-fragment",
            "https://example.test/mcp",
        ),
        (
            "streamable_http: https://review-user:review-password@example.test/mcp?token=review-query#review-fragment",
            "streamable_http: https://example.test/mcp",
        ),
        ("https://review-password@", "<invalid-url>"),
        ("documents", "documents"),
    ] {
        for reconnect in [false, true] {
            captured.0.lock().unwrap().clear();
            {
                let server = McpServerStdio::new(params().with_args(["--wire=malformed".into()]))
                    .with_name(name);
                server.connect().await.unwrap();
                let original = server.client().unwrap();
                assert!(server.read_resource("memo://fixture/note").await.is_err());
                if reconnect {
                    tokio::time::timeout(Duration::from_secs(2), async {
                        while server.is_connected().await {
                            tokio::time::sleep(Duration::from_millis(5)).await;
                        }
                    })
                    .await
                    .unwrap();
                    server.connect().await.unwrap();
                    assert!(server.is_connected().await);
                }
                server.cleanup().await.unwrap();
                assert_eq!(server.name(), name);
                assert_eq!(original.server_name(), name);
            }
            let logs = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
            assert!(
                logs.contains("Error cleaning up MCP server"),
                "missing cleanup log for {safe_name}, reconnect={reconnect}: {logs}"
            );
            assert!(logs.contains(safe_name), "{logs}");
            for secret in [
                "review-user",
                "review-password",
                "review-query",
                "review-fragment",
            ] {
                assert!(!logs.contains(secret), "cleanup log retained {secret}");
            }
        }
    }
}
