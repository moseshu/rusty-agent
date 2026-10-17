//! Shared SDK server used by memory, process, and HTTP transport tests.
#![allow(dead_code)]
use rmcp::{ErrorData, RoleServer, ServerHandler, model::*, service::RequestContext};
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

pub struct RejectInitialize(pub ErrorData);

impl ServerHandler for RejectInitialize {
    async fn initialize(
        &self,
        _: InitializeRequestParams,
        _: RequestContext<RoleServer>,
    ) -> Result<InitializeResult, ErrorData> {
        Err(self.0.clone())
    }
}

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
        let mut config = ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_prompts()
                .enable_resources()
                .build(),
        );
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
    async fn list_prompts(
        &self,
        request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListPromptsResult, ErrorData> {
        self.wait_for_request(&context).await?;
        self.pages.fetch_add(1, Ordering::SeqCst);
        let cursor = request.and_then(|params| params.cursor);
        let index = cursor
            .as_deref()
            .and_then(|cursor| cursor.parse::<usize>().ok())
            .unwrap_or(0);
        let next = match self.mode {
            "cycle" => Some("private-secret-cursor".to_owned()),
            "pages" => Some((index + 1).to_string()),
            "many_prompts" if index < 100 => Some((index + 1).to_string()),
            "overflow" => None,
            _ if cursor.is_none() => Some(String::new()),
            _ => None,
        };
        let name = if cursor.is_none() {
            "review"
        } else {
            "summary"
        };
        Ok(serde_json::from_value(json!({
            "prompts": vec![json!({"name": name, "description":"Prompt fixture"}); if self.mode == "overflow" { 2049 } else { 1 }],
            "nextCursor": next, "_meta": {"first":cursor.is_none()}
        })).unwrap())
    }
    async fn get_prompt(
        &self,
        request: GetPromptRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<GetPromptResponse, ErrorData> {
        self.wait_for_request(&context).await?;
        if request.name == "missing" {
            return Err(ErrorData::invalid_params("prompt not found", None));
        }
        Ok(serde_json::from_value::<GetPromptResult>(json!({"description":"Generated prompt", "messages":[
            {"role":"user", "content":{"type":"text","text":json!({"name":request.name,"arguments":request.arguments}).to_string()}}
        ]})).unwrap().into())
    }
    async fn list_resources(
        &self,
        request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        self.wait_for_request(&context).await?;
        let cursor = request.and_then(|params| params.cursor);
        Ok(serde_json::from_value(json!({"resources":[{"uri":"memo://fixture/note", "name":"note", "mimeType":"text/plain"}],
            "nextCursor": if cursor.is_none() { Some("") } else { None }, "_meta":{"cursor":cursor}
        })).unwrap())
    }
    async fn list_resource_templates(
        &self,
        request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, ErrorData> {
        self.wait_for_request(&context).await?;
        let cursor = request.and_then(|params| params.cursor);
        Ok(serde_json::from_value(json!({"resourceTemplates":[{"uriTemplate":"memo://fixture/{name}", "name":"notes"}],
            "nextCursor": if cursor.is_none() { Some("") } else { None }, "_meta":{"cursor":cursor}
        })).unwrap())
    }
    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, ErrorData> {
        self.wait_for_request(&context).await?;
        if request.uri == "memo://missing" {
            return Err(ErrorData::invalid_params("resource not found", None));
        }
        Ok(
            serde_json::from_value::<ReadResourceResult>(json!({"contents":[
                {"uri":request.uri, "mimeType":"text/plain", "text":"café"},
                {"uri":"memo://fixture/blob", "mimeType":"application/octet-stream", "blob":"AAH/"}
            ]}))
            .unwrap()
            .into(),
        )
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
                    "pid": std::process::id(),
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

impl Fixture {
    async fn wait_for_request(
        &self,
        context: &RequestContext<RoleServer>,
    ) -> Result<(), ErrorData> {
        if self.mode == "delayed_catalog" {
            tokio::select! {
                () = tokio::time::sleep(std::time::Duration::from_secs(6)) => {},
                () = context.ct.cancelled() => {},
            }
        }
        if self.mode == "slow_catalog" {
            self.entered.notify_one();
            context.ct.cancelled().await;
            self.cancelled.notify_one();
        }
        if self.mode == "catalog_errors" {
            return Err(ErrorData::invalid_params("catalog request rejected", None));
        }
        Ok(())
    }
}
