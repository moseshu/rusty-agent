//! Dynamic predicate errors exclude only the affected tool and keep diagnostics credential-safe.

#[path = "../support/mod.rs"]
mod support;

use ra_core::{
    agent::{AgentId, AgentSpec},
    context::RunContext,
    error::Error,
    state::RunId,
};
use ra_mcp::{client::McpClientOptions, filter::ToolFilter, in_process::InProcessToolServer};
use std::sync::{Arc, Mutex};

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
    fn make_writer(&'a self) -> Self {
        self.clone()
    }
}

#[tokio::test]
async fn sync_and_async_errors_exclude_only_failing_tools_and_redact_logs() {
    let logs = CapturedLogs::default();
    tracing::subscriber::set_global_default(
        tracing_subscriber::fmt()
            .with_writer(logs.clone())
            .with_max_level(tracing::Level::ERROR)
            .with_ansi(false)
            .without_time()
            .finish(),
    )
    .unwrap();
    let agent = AgentSpec::builder()
        .id(AgentId::new("test"))
        .name("Test")
        .build()
        .unwrap();
    let context = RunContext::new(RunId::new("test"), &agent);
    for asynchronous in [false, true] {
        let predicate = |_: ra_mcp::filter::ToolFilterContext<'_>, tool: rmcp::model::Tool| {
            if tool.name == "SECRET_TOOL_NAME" {
                Err(Error::caller("SECRET_FILTER_ERROR"))
            } else {
                Ok(true)
            }
        };
        let filter = if asynchronous {
            ToolFilter::dynamic(move |context, tool| {
                Box::pin(async move { predicate(context, tool) })
            })
        } else {
            ToolFilter::dynamic_sync(predicate)
        };
        let mut fixture = support::Fixture::new("logs");
        fixture.tools_override = Some(vec![
            support::declaration("before"),
            support::declaration("SECRET_TOOL_NAME"),
            support::declaration("after"),
        ]);
        let mut server = InProcessToolServer::connect(
            "streamable_http: https://SECRET_USER:SECRET_PASSWORD@example.test/SECRET_PATH?token=SECRET_QUERY#SECRET_FRAGMENT",
            fixture,
            McpClientOptions::default().with_cache_tools_list(true).with_tool_filter(filter),
        ).await.unwrap();
        for _ in 0..2 {
            let tools = server
                .client()
                .list_tools_with_context(Some(&context), Some(context.agent()))
                .await
                .unwrap();
            assert_eq!(
                tools
                    .iter()
                    .map(|tool| tool.name.as_ref())
                    .collect::<Vec<_>>(),
                ["before", "after"]
            );
        }
        assert_eq!(server.client().cached_tools().unwrap().len(), 3);
        server.client().set_tool_filter(ToolFilter::default());
        let resolver = ra_mcp::resolver::McpToolResolver::new(
            vec![(std::sync::Arc::new(server.client()), ToolFilter::default())],
            ra_mcp::tool::McpDiscoveryOptions::new(),
        );
        let tools =
            ra_core::tool::ToolResolver::get_tools(&resolver, &context, &Default::default())
                .await
                .unwrap();
        for tool in tools {
            let binding = tool.recipient_binding().unwrap();
            assert_eq!(
                binding[0].as_str(),
                Some("streamable_http: https://example.test/SECRET_PATH")
            );
            let approval = ra_core::item::ToolApproval::new(
                ra_core::item::CallId::new("binding"),
                "tool",
                serde_json::json!({}),
            )
            .with_recipient_binding(Some(binding));
            assert!(!format!("{approval:?}").contains("SECRET_"));
        }
        server.cleanup().await.unwrap();
    }
    let output = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
    assert_eq!(output.matches("Error applying MCP tool filter").count(), 4);
    assert!(!output.contains("SECRET_"), "{output}");
}
