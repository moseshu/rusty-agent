//! Shared SDK server used by memory, process, and HTTP transport tests.
#![allow(dead_code)]
use rmcp::{ErrorData, RoleServer, ServerHandler, model::*, service::RequestContext};
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[derive(Clone)]
pub struct Fixture {
    pub label: String,
    pub tools_override: Option<Vec<Tool>>,
    pub mode: &'static str,
    pub entered: Arc<tokio::sync::Notify>,
    pub cancelled: Arc<tokio::sync::Notify>,
    pub pages: Arc<AtomicUsize>,
    pub instructions: Option<String>,
}
impl Fixture {
    pub fn new(label: &str) -> Self {
        Self {
            label: label.to_owned(),
            tools_override: None,
            mode: "normal",
            entered: Arc::default(),
            cancelled: Arc::default(),
            pages: Arc::default(),
            instructions: None,
        }
    }
}
pub fn declaration(name: &str) -> Tool {
    serde_json::from_value(json!({"name": name, "description": "Search fixture",
        "inputSchema": {"type": "object", "properties": {"query": {"type": "string"}}},
        "annotations": {"readOnlyHint": true}}))
    .unwrap()
}
impl ServerHandler for Fixture {
    fn get_info(&self) -> ServerConfig {
        let mut config = ServerConfig::new(ServerCapabilities::builder().enable_tools().build());
        config.instructions = self.instructions.clone();
        config
    }
    async fn list_tools(
        &self,
        request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let count = self.pages.fetch_add(1, Ordering::SeqCst);
        let cursor = request.and_then(|r| r.cursor);
        let mut result = ListToolsResult::default();
        if let Some(tools) = &self.tools_override {
            result.tools = tools.clone();
            return Ok(result);
        }
        match self.mode {
            "cycle" => {
                result.next_cursor = Some("repeat".into());
            }
            "pages" => {
                result.next_cursor = Some(count.to_string());
            }
            "oversized_cursor" => {
                result.next_cursor = Some("a".repeat(65537));
            }
            "overflow" => {
                result.tools = vec![declaration("tool"); 2049];
            }
            "across_pages" => {
                result.tools = vec![declaration("tool"); if cursor.is_none() { 2048 } else { 1 }];
                result.next_cursor = if cursor.is_none() {
                    Some("last".into())
                } else {
                    None
                };
            }
            "slow_list" => {
                context.ct.cancelled().await;
            }
            _ => {
                result.tools = vec![declaration(if cursor.is_none() {
                    "first-page"
                } else {
                    "search"
                })];
                result.next_cursor = if cursor.is_none() {
                    Some(String::new())
                } else {
                    None
                };
            }
        }
        Ok(result)
    }
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let result = match request.name.as_ref() {
            "wait" => {
                self.entered.notify_one();
                context.ct.cancelled().await;
                self.cancelled.notify_one();
                CallToolResult::success(vec![ContentBlock::text("cancelled")])
            }
            "argument_state" => {
                CallToolResult::success(vec![ContentBlock::text(if request.arguments.is_none() {
                    "omitted"
                } else {
                    "object"
                })])
            }
            "failure" => CallToolResult::error(vec![ContentBlock::text("actionable failure")]),
            "protocol_error" => {
                return Err(ErrorData::internal_error("server detail", None));
            }
            "invalid_params" => {
                return Err(ErrorData::invalid_params("query is required", None));
            }
            "meta_echo" => CallToolResult::success(vec![ContentBlock::text(
                serde_json::to_string(&context.meta).unwrap(),
            )]),
            "env" => CallToolResult::success(vec![ContentBlock::text(
                json!({
                    "cwd": std::env::current_dir().unwrap(),
                    "value": std::env::var("MCP_FIXTURE_VALUE").ok(),
                    "path": std::env::var("PATH").ok(),
                })
                .to_string(),
            )]),
            _ => CallToolResult::success(vec![ContentBlock::text(format!(
                "{}:{}:{}",
                self.label,
                request.name,
                request
                    .arguments
                    .unwrap_or_default()
                    .get("query")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
            ))]),
        };
        Ok(result.into())
    }
}
