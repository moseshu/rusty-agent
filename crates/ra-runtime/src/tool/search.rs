//! Client-executed deferred discovery, ported from Codex's tool search handler.
//!
//! The Python runner does not execute client search calls. This runtime extension uses Codex's
//! BM25 engine and result order while retaining provider-neutral definitions in persisted output.

use std::sync::{Arc, Mutex, PoisonError, Weak};

use async_trait::async_trait;
use bm25::{Document, Language, SearchEngine, SearchEngineBuilder};
use ra_core::{
    error::{Error, Result, ToolErrorKind},
    model::ModelToolDefinition,
    permission::PermissionScope,
    tool::{Tool, ToolConcurrency, ToolContext, ToolOptions, ToolOrigin, ToolOutput, ToolSchema},
};
use serde::Deserialize;
use serde_json::{Value, json};

const DEFAULT_LIMIT: usize = 8;
const DESCRIPTION: &str = "# Tool discovery\n\nSearches over deferred tool metadata with BM25 and exposes matching tools for the next model call.\n\nSome of the tools may not have been provided to you upfront, and you should use this tool (`tool_search`) to search for the required tools. For MCP tool discovery, always use `tool_search` instead of `list_mcp_resources` or `list_mcp_resource_templates`.";

/// A binding owns its cache, so clones and repeated preparations reuse one index.
#[derive(Default)]
pub(crate) struct ToolSearchHandlerCache {
    cached: Mutex<Option<CachedHandler>>,
}

struct CachedHandler {
    sources: Vec<Weak<dyn Tool>>,
    handler: Arc<ToolSearchHandler>,
}

impl ToolSearchHandlerCache {
    pub(crate) fn get_or_build(&self, tools: &[Arc<dyn Tool>]) -> Result<Option<Arc<dyn Tool>>> {
        let sources: Vec<_> = tools
            .iter()
            .filter(|tool| tool.options().is_discoverable())
            .collect();
        if sources.is_empty() {
            *self.cached.lock().unwrap_or_else(PoisonError::into_inner) = None;
            return Ok(None);
        }
        // Tools can change their model projection without changing allocation identity. Check both
        // the registry entries and their current metadata; unlike Codex, Tool has no immutable-spec
        // marker, so even stable allocations need this comparison.
        let definitions: Vec<_> = sources
            .iter()
            .map(|tool| tool.model_definition().with_defer_loading(true))
            .collect();
        let sources: Vec<_> = sources.iter().map(|tool| Arc::downgrade(tool)).collect();
        {
            let cached = self.cached.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(entry) = cached.as_ref()
                && Self::matches(entry, &sources, &definitions)
            {
                return Ok(Some(entry.handler.clone()));
            }
        }
        // Build outside the cache lock, then recheck it as Codex does: concurrent preparations
        // can reuse a handler another caller built while this index was being assembled.
        let handler = Arc::new(ToolSearchHandler::new(definitions)?);
        let mut cached = self.cached.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(entry) = cached.as_ref()
            && Self::matches(entry, &sources, &handler.definitions)
        {
            return Ok(Some(entry.handler.clone()));
        }
        *cached = Some(CachedHandler {
            sources,
            handler: Arc::clone(&handler),
        });
        Ok(Some(handler))
    }
    fn matches(
        entry: &CachedHandler,
        sources: &[Weak<dyn Tool>],
        definitions: &[ModelToolDefinition],
    ) -> bool {
        entry.handler.definitions == definitions
            && entry.sources.len() == sources.len()
            && entry
                .sources
                .iter()
                .zip(sources)
                .all(|(left, right)| Weak::ptr_eq(left, right))
    }
}

struct ToolSearchHandler {
    origin: ToolOrigin,
    schema: ToolSchema,
    definitions: Vec<ModelToolDefinition>,
    engine: SearchEngine<usize>,
}

#[derive(Deserialize)]
struct Arguments {
    query: String,
    limit: Option<usize>,
}

impl ToolSearchHandler {
    fn new(definitions: Vec<ModelToolDefinition>) -> Result<Self> {
        let documents: Vec<_> = definitions
            .iter()
            .enumerate()
            .map(|(index, tool)| Document::new(index, search_text(tool)))
            .collect();
        let engine =
            SearchEngineBuilder::<usize>::with_documents(Language::English, documents).build();
        Ok(Self {
            // Not `ToolOrigin::new`: a function may also be named `tool_search`, and approvals or
            // permission rules pinned to one identity must not apply to the other.
            origin: ToolOrigin::tool_search(),
            schema: ToolSchema::loose("tool_search", json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "Search query for deferred tools."},
                    "limit": {"type": "number", "description": "Maximum number of tools to return. Defaults to 8."}
                },
                "required": ["query"],
                "additionalProperties": false
            }))?.with_description(DESCRIPTION),
            definitions,
            engine,
        })
    }
}

#[async_trait]
impl Tool for ToolSearchHandler {
    fn origin(&self) -> &ToolOrigin {
        &self.origin
    }
    fn schema(&self) -> &ToolSchema {
        &self.schema
    }
    fn model_definition(&self) -> ModelToolDefinition {
        ModelToolDefinition::tool_search(self.schema.input_schema().clone())
            .with_description(DESCRIPTION)
    }
    fn options(&self) -> ToolOptions {
        ToolOptions::new()
            .with_permission_scope(PermissionScope::Read)
            .with_concurrency(ToolConcurrency::Parallel)
    }
    async fn call(&self, context: ToolContext<'_>) -> Result<ToolOutput> {
        let args: Arguments = serde_json::from_value(context.arguments().clone())
            .map_err(|error| invalid_input(error.to_string()))?;
        let query = args.query.trim();
        if query.is_empty() {
            return Err(invalid_input("query must not be empty"));
        }
        let limit = args.limit.unwrap_or(DEFAULT_LIMIT);
        if limit == 0 {
            return Err(invalid_input("limit must be greater than zero"));
        }
        let tools = self
            .engine
            .search(query, limit)
            .into_iter()
            .filter_map(|result| self.definitions.get(result.document.id).cloned())
            .collect();
        Ok(ToolOutput::tool_search(tools))
    }
}

fn invalid_input(message: impl Into<String>) -> Error {
    Error::tool(ToolErrorKind::InvalidInput, "tool_search", message)
}

/// Codex indexes names, descriptions, and schema property names/descriptions, never raw schemas.
fn search_text(tool: &ModelToolDefinition) -> String {
    let mut parts = Vec::new();
    if let Some(namespace) = tool.namespace() {
        parts.push(namespace.to_owned());
    }
    if let Some(description) = tool.namespace_description() {
        parts.push(description.to_owned());
    }
    parts.push(tool.name().to_owned());
    parts.push(tool.name().replace('_', " "));
    if let Some(description) = tool.description() {
        parts.push(description.to_owned());
    }
    schema_search_text(tool.input_schema(), &mut parts);
    if let Some(ra_core::model::CustomToolFormat::Grammar { syntax, .. }) =
        tool.kind().custom_format()
    {
        parts.push(syntax.as_str().to_owned());
    }
    parts.join(" ")
}

fn schema_search_text(schema: &Value, parts: &mut Vec<String>) {
    if let Some(description) = schema.get("description").and_then(Value::as_str) {
        parts.push(description.to_owned());
    }
    if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
        for (name, schema) in properties {
            parts.push(name.clone());
            schema_search_text(schema, parts);
        }
    }
    if let Some(items) = schema.get("items") {
        schema_search_text(items, parts);
    }
    if let Some(variants) = schema.get("anyOf").and_then(Value::as_array) {
        for variant in variants {
            schema_search_text(variant, parts);
        }
    }
}
