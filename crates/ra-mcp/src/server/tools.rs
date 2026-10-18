//! Complete, unfiltered tool snapshots with generation-safe concurrent refreshes.

use crate::filter::ToolFilter;
use ra_core::error::{Error, Result, ToolErrorKind};
use rmcp::model::{JsonObject, Tool};
use std::{
    future::Future,
    sync::{Mutex, MutexGuard},
};

#[derive(Default)]
pub(crate) struct ToolsCache(Mutex<CacheState>);

#[derive(Default)]
struct CacheState {
    cache_tools_list: bool,
    filter: ToolFilter,
    tools: Option<Vec<Tool>>,
    dirty: bool,
    generation: u64,
    refresh_sequence: u64,
    last_published_sequence: u64,
}

impl ToolsCache {
    pub(crate) fn new(cache_tools_list: bool, filter: ToolFilter) -> Self {
        Self(Mutex::new(CacheState {
            cache_tools_list,
            filter,
            ..Default::default()
        }))
    }
    #[cfg(any(feature = "stdio", feature = "http"))]
    pub(crate) fn configure(&self, cache_tools_list: bool, filter: ToolFilter) {
        let mut state = self.state();
        state.cache_tools_list = cache_tools_list;
        state.filter = filter;
    }
    pub(crate) fn set_filter(&self, filter: ToolFilter) {
        self.state().filter = filter;
    }
    pub(crate) fn filter(&self) -> ToolFilter {
        self.state().filter.clone()
    }
    pub(crate) fn set_cache_tools_list(&self, enabled: bool) {
        self.state().cache_tools_list = enabled;
    }
    fn state(&self) -> MutexGuard<'_, CacheState> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    pub(crate) fn snapshot(&self) -> Option<Vec<Tool>> {
        self.state().tools.clone()
    }
    pub(crate) fn invalidate(&self) {
        let mut state = self.state();
        state.generation += 1;
        state.dirty = true;
    }
    pub(crate) async fn list(
        &self,
        fetch: impl Future<Output = Result<Vec<Tool>>>,
    ) -> Result<Vec<Tool>> {
        let (generation, sequence) = {
            let mut state = self.state();
            if state.cache_tools_list
                && !state.dirty
                && let Some(tools) = &state.tools
                && !tools.is_empty()
            {
                return Ok(tools.clone());
            }
            state.refresh_sequence += 1;
            (state.generation, state.refresh_sequence)
        };
        let tools = fetch.await?;
        let mut state = self.state();
        if generation == state.generation && sequence > state.last_published_sequence {
            state.tools = Some(tools.clone());
            state.dirty = false;
            state.last_published_sequence = sequence;
        }
        Ok(tools)
    }
    pub(crate) fn validate_required(
        &self,
        name: &str,
        arguments: Option<&JsonObject>,
        server_name: &str,
    ) -> Result<()> {
        let state = self.state();
        if state.dirty {
            return Ok(());
        }
        let Some(tool) = state
            .tools
            .as_ref()
            .and_then(|tools| tools.iter().find(|tool| tool.name == name))
        else {
            return Ok(());
        };
        let Some(required) = tool
            .input_schema
            .get("required")
            .and_then(serde_json::Value::as_array)
        else {
            return Ok(());
        };
        let mut missing: Vec<_> = required
            .iter()
            .filter_map(serde_json::Value::as_str)
            .filter(|name| arguments.is_none_or(|arguments| !arguments.contains_key(*name)))
            .collect();
        if missing.is_empty() {
            return Ok(());
        }
        missing.sort_unstable();
        Err(Error::tool(
            ToolErrorKind::InvalidInput,
            "mcp",
            format!(
                "Failed to call tool '{name}' on MCP server '{}': missing required parameters: {}",
                crate::catalog::safe_server_name(server_name),
                missing.join(", ")
            ),
        ))
    }
}
