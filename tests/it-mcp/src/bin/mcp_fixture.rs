//! A real stdio MCP server for transport acceptance.
#[path = "../../support/mod.rs"]
mod support;
use rmcp::ServiceExt;
#[tokio::main]
async fn main() {
    let service = support::Fixture::new("stdio")
        .serve(rmcp::transport::stdio())
        .await
        .unwrap();
    service.waiting().await.unwrap();
}
