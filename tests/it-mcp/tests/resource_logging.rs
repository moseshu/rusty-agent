//! Resource failure diagnostics follow Python's credential-safe MCP log naming.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ra_core::{
    agent::{AgentId, AgentSpec},
    context::RunContext,
    error::{Error, Result},
    item::CallId,
    state::RunId,
    tool::ToolContext,
};
use ra_mcp::{
    client::{McpClient, McpClientOptions},
    in_process::InProcessToolServer,
    tool::McpToolClientSource,
};
use rmcp::{ErrorData, RoleServer, ServerHandler, model::*, service::RequestContext};
use serde_json::json;

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

struct UnavailableSource;

#[async_trait]
impl McpToolClientSource for UnavailableSource {
    async fn client(&self) -> Result<McpClient> {
        Err(Error::config("MCP startup failed")
            .with_source(std::io::Error::other("private-source-secret")))
    }
}

struct UnavailableResources;

impl ServerHandler for UnavailableResources {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_resources().build())
    }
    async fn list_resources(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> std::result::Result<ListResourcesResult, ErrorData> {
        Err(ErrorData::invalid_params("resources unavailable", None))
    }
    async fn list_resource_templates(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> std::result::Result<ListResourceTemplatesResult, ErrorData> {
        Err(ErrorData::invalid_params("templates unavailable", None))
    }
}

#[tokio::test]
async fn resource_failure_logs_redact_names_for_unavailable_sources_and_remote_errors() {
    let captured = CapturedLogs::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(captured.clone())
        .with_max_level(tracing::Level::WARN)
        .with_ansi(false)
        .without_time()
        .finish();
    // This integration binary owns its subscriber, including all spawned listing tasks.
    tracing::subscriber::set_global_default(subscriber).unwrap();
    let agent = AgentSpec::builder()
        .id(AgentId::new("resources"))
        .name("Resources")
        .build()
        .unwrap();
    let run = RunContext::new(RunId::new("resource-logging"), &agent);
    let call_id = CallId::new("resource-list");
    let arguments = json!({});
    for (name, safe_name) in [
        (
            "https://review-user:review-password@example.test/mcp?token=review-query#review-fragment",
            "https://example.test/mcp",
        ),
        (
            "streamable_http: https://review-user:review-password@example.test/mcp?token=review-query#review-fragment",
            "streamable_http: https://example.test/mcp",
        ),
        (
            "sse: https://review-user:review-password@example.test/mcp?token=review-query#review-fragment",
            "sse: https://example.test/mcp",
        ),
        ("https://review-password@", "<invalid-url>"),
        ("documents", "documents"),
    ] {
        let mut server =
            InProcessToolServer::connect(name, UnavailableResources, McpClientOptions::default())
                .await
                .unwrap();
        for source in [
            Arc::new(UnavailableSource) as Arc<dyn McpToolClientSource>,
            Arc::new(server.client()),
        ] {
            let tools =
                ra_tools::mcp_ns::resource_tools_from_sources(vec![(name.into(), source)]).unwrap();
            for tool in &tools[..2] {
                captured.0.lock().unwrap().clear();
                let output = tool
                    .call(ToolContext::new(&run, tool.as_ref(), &call_id, &arguments))
                    .await
                    .unwrap();
                assert!(output.as_text().unwrap().contains("[]"));
                let logs = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
                assert!(
                    logs.contains("Failed to list MCP resources for a server"),
                    "{logs}"
                );
                assert!(logs.contains(safe_name), "{logs}");
                for secret in [
                    "review-user",
                    "review-password",
                    "review-query",
                    "review-fragment",
                    "private-source-secret",
                ] {
                    assert!(
                        !logs.contains(secret),
                        "resource failure log retained {secret}"
                    );
                }
            }
        }
        server.cleanup().await.unwrap();
    }
}
