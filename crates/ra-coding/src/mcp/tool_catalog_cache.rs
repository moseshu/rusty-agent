// Adapted from openai/codex (Apache-2.0) @ e58bf8f5233b51e7cd1679d947e3028c1ed91b06, modified.
//! Reusable startup tool catalogs for the coding product's connection set.

use ra_mcp::{
    client::{McpClient, McpToolListRevision},
    tool::McpToolCatalog,
};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex, PoisonError},
    time::Duration,
};
use tokio::time::Instant;

const CAPACITY: usize = 32;
const TTL: Duration = Duration::from_mins(30);

#[derive(PartialEq, Eq)]
pub(super) struct CacheKey {
    pub(super) server: String,
    pub(super) connection_identity: String,
}

#[derive(Default)]
pub(super) struct ToolCatalogCache(Mutex<VecDeque<(CacheKey, Arc<CacheEntry>)>>);

impl ToolCatalogCache {
    pub(super) fn entry(&self, key: CacheKey) -> Arc<CacheEntry> {
        let mut entries = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let entry = entries
            .iter()
            .position(|(candidate, _)| *candidate == key)
            .and_then(|index| entries.remove(index))
            .map_or_else(Arc::default, |(_, entry)| entry);
        entries.push_front((key, entry.clone()));
        entries.truncate(CAPACITY);
        entry
    }
}

#[derive(Default)]
pub(super) struct CacheEntry(Mutex<CacheState>);

#[derive(Default)]
struct CacheState {
    next_generation: u64,
    published_generation: u64,
    disabled_by_server: bool,
    snapshot: Option<Snapshot>,
}

struct Snapshot {
    catalog: McpToolCatalog,
    published_at: Instant,
    revision: Option<McpToolListRevision>,
}

impl CacheEntry {
    pub(super) fn begin_fetch(&self) -> u64 {
        let mut state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        state.next_generation += 1;
        state.next_generation
    }

    pub(super) fn current_catalog(&self, track_notifications: bool) -> Option<McpToolCatalog> {
        let state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        state
            .snapshot
            .as_ref()
            .filter(|snapshot| {
                // A reader that tracks changes needs a snapshot that tracked them from publication.
                snapshot
                    .revision
                    .as_ref()
                    .map_or(!track_notifications, |revision| revision.current() == 0)
                    && snapshot.published_at.elapsed() <= TTL
            })
            .map(|snapshot| snapshot.catalog.clone())
    }

    pub(super) fn observe_initialize(&self, client: &McpClient) -> bool {
        let no_cache = client
            .server_initialize_result()
            .capabilities
            .experimental
            .as_ref()
            .and_then(|capabilities| capabilities.get("codex/tool-catalog-cache"))
            .and_then(|capability| capability.get("cacheable"))
            .and_then(serde_json::Value::as_bool)
            == Some(false);
        let mut state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if no_cache {
            state.disabled_by_server = true;
            state.snapshot = None;
        }
        !state.disabled_by_server
    }

    pub(super) fn publish_if_newest(
        &self,
        generation: u64,
        client: &McpClient,
        live_catalog: &McpToolCatalog,
        track_notifications: bool,
    ) {
        let revision = track_notifications.then(|| client.tool_list_revision());
        let mut tools = live_catalog.tools();
        for tool in &mut tools {
            // Cached declarations cannot supply approval or parallelism hints.
            tool.annotations = None;
        }
        let mut catalog =
            McpToolCatalog::new(client.server_name(), tools, client.options().clone());
        if let Some(instructions) = client.server_instructions() {
            catalog = catalog.with_server_instructions(instructions);
        }
        let mut state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if state.disabled_by_server || generation <= state.published_generation {
            return;
        }
        // A newer session whose list changed also supersedes older cached declarations.
        state.published_generation = generation;
        state.snapshot = revision
            .as_ref()
            .is_none_or(|revision| revision.current() == 0)
            .then(|| Snapshot {
                catalog,
                revision,
                published_at: Instant::now(),
            });
    }
}
