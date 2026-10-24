// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Reuse parsed manifest revisions within an explicitly owned plugin store.
//! Store clones share the cache; independently created stores do not share entries or warnings.

use super::LoadedPluginManifest;
use super::PluginManifest;
use super::PluginManifestFormat;
use super::parse_resolved_plugin_manifest;
use crate::plugins::AbsolutePathBuf;
use crate::plugins::discovery::AGENT_PLUGIN_MANIFEST_RELATIVE_PATH;
use crate::plugins::discovery::find_plugin_manifest_path;
use sha2::Digest;
use sha2::Sha256;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::fs;
use std::path::Path;
use std::sync::Mutex;

const CAPACITY: usize = 128;
const MAX_CACHABLE_INPUT_LEN: usize = 64 * 1024;

pub(crate) struct ManifestCache {
    state: Option<Mutex<ManifestCacheState>>,
}

#[derive(Default)]
struct ManifestCacheState {
    entries: HashMap<(CachePath, CachePath), Entry>,
    recency: VecDeque<(CachePath, CachePath)>,
}

struct Entry {
    contents_digest: [u8; 32],
    overlay: Option<(CachePath, [u8; 32])>,
    manifest: PluginManifest,
}

impl std::fmt::Debug for ManifestCache {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ManifestCache")
            .finish_non_exhaustive()
    }
}

impl Default for ManifestCache {
    fn default() -> Self {
        Self {
            state: Some(Mutex::default()),
        }
    }
}

impl ManifestCache {
    pub(crate) fn disabled() -> Self {
        Self { state: None }
    }

    pub(crate) fn load(&self, plugin_root: &Path) -> Option<LoadedPluginManifest> {
        let manifest_path = find_plugin_manifest_path(plugin_root)?;
        let contents = fs::read_to_string(&manifest_path).ok()?;
        let is_agent_plugin =
            manifest_path == plugin_root.join(AGENT_PLUGIN_MANIFEST_RELATIVE_PATH);
        let overlay = if is_agent_plugin {
            let overlay_path = plugin_root.join(".codex-plugin/plugin.json");
            fs::read_to_string(&overlay_path)
                .ok()
                .map(|contents| (overlay_path, contents))
        } else {
            None
        };
        match parse_resolved_plugin_manifest(
            self,
            plugin_root,
            &manifest_path,
            &contents,
            overlay
                .as_ref()
                .map(|(path, contents)| (path.as_path(), contents.as_str())),
        ) {
            Ok(manifest) => Some(LoadedPluginManifest {
                manifest,
                format: if is_agent_plugin {
                    PluginManifestFormat::AgentPlugin
                } else {
                    PluginManifestFormat::Legacy
                },
            }),
            Err(err) => {
                tracing::warn!(
                    path = %manifest_path.display(),
                    "failed to parse plugin manifest: {err}"
                );
                None
            }
        }
    }

    pub(crate) fn parse(
        &self,
        root: &AbsolutePathBuf,
        path: &AbsolutePathBuf,
        contents: &str,
        overlay: Option<(&AbsolutePathBuf, &str)>,
        parse: impl FnOnce() -> Result<PluginManifest, serde_json::Error>,
    ) -> Result<PluginManifest, serde_json::Error> {
        let Some(state) = &self.state else {
            return parse();
        };
        if contents
            .len()
            .saturating_add(overlay.map_or(0, |(_, value)| value.len()))
            > MAX_CACHABLE_INPUT_LEN
        {
            state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&(cache_path(root), cache_path(path)));
            // Unbounded inputs must not serialize unrelated parses behind the cache lock.
            return parse();
        }
        // Parsing is synchronous and does no filesystem I/O. Serializing cache misses also
        // prevents concurrent capability loaders from validating the same revision twice.
        state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .parse(root, path, contents, overlay, parse)
    }
}

impl ManifestCacheState {
    fn remove(&mut self, key: &(CachePath, CachePath)) -> Option<Entry> {
        let entry = self.entries.remove(key)?;
        self.recency.retain(|existing| existing != key);
        Some(entry)
    }

    fn parse(
        &mut self,
        root: &AbsolutePathBuf,
        path: &AbsolutePathBuf,
        contents: &str,
        overlay: Option<(&AbsolutePathBuf, &str)>,
        parse: impl FnOnce() -> Result<PluginManifest, serde_json::Error>,
    ) -> Result<PluginManifest, serde_json::Error> {
        let key = (cache_path(root), cache_path(path));
        let previous = self.remove(&key);
        let contents_digest = Sha256::digest(contents.as_bytes()).into();
        let overlay = overlay
            .map(|(path, contents)| (cache_path(path), Sha256::digest(contents.as_bytes()).into()));
        if let Some(entry) = previous
            && entry.contents_digest == contents_digest
            && entry.overlay == overlay
        {
            let manifest = entry.manifest.clone();
            self.recency.push_front(key.clone());
            self.entries.insert(key, entry);
            return Ok(manifest);
        }
        let manifest = parse()?;
        if self.entries.len() == CAPACITY
            && let Some(oldest) = self.recency.pop_back()
        {
            self.entries.remove(&oldest);
        }
        self.recency.push_front(key.clone());
        self.entries.insert(
            key,
            Entry {
                contents_digest,
                overlay,
                manifest: manifest.clone(),
            },
        );
        Ok(manifest)
    }
}

// Native paths keep opaque byte identity. Representable Windows paths use the source URI's
// ASCII case-insensitive identity, including drive/UNC normalization, without a public URI API.
#[derive(Clone, PartialEq, Eq, Hash)]
enum CachePath {
    Native(AbsolutePathBuf),
    WindowsUri(String),
}

fn cache_path(path: &AbsolutePathBuf) -> CachePath {
    if cfg!(windows)
        && let Some(uri) = super::host_path_url(path)
    {
        CachePath::WindowsUri(uri.to_string().to_ascii_lowercase())
    } else {
        CachePath::Native(path.clone())
    }
}
