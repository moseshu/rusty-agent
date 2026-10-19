//! Shared initialized-session catalog and invocation fixtures.
use super::*;
use ra_core::{
    item::CallId,
    tool::{Tool as CoreTool, ToolContext, ToolOutput},
};
use ra_mcp::tool::McpToolCatalog;

pub(super) fn seed(name: &str, options: McpClientOptions) -> McpToolCatalog {
    McpToolCatalog::new(
        name,
        vec![
            serde_json::from_value(
                json!({"name":"echo_1","inputSchema":{"type":"object","properties":{}}}),
            )
            .unwrap(),
        ],
        options,
    )
}
pub(super) async fn invoke(tool: &dyn CoreTool) -> Result<ToolOutput> {
    let context = context();
    let call = CallId::new("lazy-call");
    let arguments = json!({});
    tool.call(ToolContext::new(&context, tool, &call, &arguments))
        .await
}
pub(super) async fn settle() {
    for _ in 0..30 {
        tokio::task::yield_now().await;
    }
}
