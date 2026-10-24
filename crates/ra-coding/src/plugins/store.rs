// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Explicit local plugin installation, replacement, version selection, and removal.

use super::command_migration::migrate_plugin_commands;
use super::discovery::AgentPluginSchemaStatus;
use super::discovery::agent_plugin_schema_status;
use super::discovery::find_plugin_manifest_path;
use super::identity::PluginId;
use super::identity::validate_plugin_segment;
use super::manifest::ManifestCache;
use super::manifest::PluginManifest;
use super::manifest::PluginManifestFormat;
use super::manifest::host_absolute_path;
use super::manifest::load_plugin_manifest;
use super::manifest::parse_plugin_manifest;
use super::{AbsolutePathBuf, join_absolute_path};
use serde::Deserialize;
use serde_json::Value as JsonValue;
use sha2::Digest;
use sha2::Sha256;
use std::cmp::Ordering;
use std::fs;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

pub use super::installed::DEFAULT_PLUGIN_VERSION;
pub use super::installed::PLUGINS_CACHE_DIR;
/// Persistent plugin data below the explicitly supplied product home.
pub const PLUGINS_DATA_DIR: &str = "plugins/data";
const AGENT_PLUGINS_DATA_DIR: &str = "agent-plugins";
const DEFAULT_AGENT_PLUGIN_VERSION: &str = "1.0.0";

#[derive(Debug, Clone, PartialEq, Eq)]
/// The installed package identity, selected cache version, and destination.
pub struct PluginInstallResult {
    /// The marketplace-qualified package identity.
    pub plugin_id: PluginId,
    /// The selected directory version, which may differ from the manifest version.
    pub plugin_version: String,
    /// The installed version directory.
    pub installed_path: AbsolutePathBuf,
}

#[derive(Debug, Clone)]
/// An explicit local package store; clones share manifest validation cache ownership.
pub struct PluginStore {
    // Retained for the source Store's marketplace, metadata, and management consumers.
    #[allow(dead_code)]
    pub(crate) manifest_cache: Arc<ManifestCache>,
    // Retained for the source Store's marketplace, metadata, and management consumers.
    #[allow(dead_code)]
    codex_home: AbsolutePathBuf,
    root: AbsolutePathBuf,
    data_root: AbsolutePathBuf,
}

// Local installation identity is also consumed by the source's non-account loader.
#[allow(dead_code)]
pub(crate) struct ActivePluginInstallation {
    pub(crate) plugin_id: PluginId,
    pub(crate) root: AbsolutePathBuf,
}

#[derive(Clone, Copy)]
enum InstallManifest<'a> {
    OnDisk,
    Fallback(&'a str),
}

impl PluginStore {
    /// Creates a Store, panicking when the supplied product home cannot be resolved.
    #[must_use]
    pub fn new(codex_home: PathBuf) -> Self {
        Self::try_new(codex_home)
            .unwrap_or_else(|err| panic!("plugin cache root should be absolute: {err}"))
    }

    /// Resolves cache and data roots without creating or activating a package.
    #[allow(clippy::needless_pass_by_value)] // Preserve the upstream owned construction API.
    pub fn try_new(codex_home: PathBuf) -> Result<Self, PluginStoreError> {
        let root = host_absolute_path(&codex_home.join(PLUGINS_CACHE_DIR))
            .map_err(|err| PluginStoreError::io("failed to resolve plugin cache root", err))?;
        let data_root = host_absolute_path(&codex_home.join(PLUGINS_DATA_DIR))
            .map_err(|err| PluginStoreError::io("failed to resolve plugin data root", err))?;
        let codex_home = host_absolute_path(&codex_home)
            .map_err(|err| PluginStoreError::io("failed to resolve product home", err))?;

        Ok(Self {
            manifest_cache: Arc::new(ManifestCache::default()),
            codex_home,
            root,
            data_root,
        })
    }

    /// Returns the version-cache root.
    #[must_use]
    pub fn root(&self) -> &AbsolutePathBuf {
        &self.root
    }

    // Retained for the source Store's marketplace, metadata, and management consumers.
    #[allow(dead_code)]
    pub(crate) fn codex_home(&self) -> &AbsolutePathBuf {
        &self.codex_home
    }

    /// Returns the package cache root without selecting a version.
    #[must_use]
    pub fn plugin_base_root(&self, plugin_id: &PluginId) -> AbsolutePathBuf {
        join_absolute_path(
            &join_absolute_path(&self.root, &plugin_id.marketplace_name),
            &plugin_id.plugin_name,
        )
    }

    /// Returns the cache path for the requested version.
    #[must_use]
    pub fn plugin_root(&self, plugin_id: &PluginId, plugin_version: &str) -> AbsolutePathBuf {
        join_absolute_path(&self.plugin_base_root(plugin_id), plugin_version)
    }

    /// Returns the persistent Legacy data directory; uninstall leaves it intact.
    #[must_use]
    pub fn plugin_data_root(&self, plugin_id: &PluginId) -> AbsolutePathBuf {
        join_absolute_path(
            &self.data_root,
            format!("{}-{}", plugin_id.plugin_name, plugin_id.marketplace_name),
        )
    }

    // Retained for the source Store's marketplace, metadata, and management consumers.
    #[allow(dead_code)]
    pub(crate) fn agent_plugin_data_root(&self, plugin_id: &PluginId) -> AbsolutePathBuf {
        let mut digest = Sha256::new();
        digest.update(plugin_id.marketplace_name.as_bytes());
        digest.update([0]);
        digest.update(plugin_id.plugin_name.as_bytes());
        join_absolute_path(
            &join_absolute_path(&self.data_root, AGENT_PLUGINS_DATA_DIR),
            hex_prefix(&digest.finalize(), 32),
        )
    }

    // Retained for the source Store's marketplace, metadata, and management consumers.
    #[allow(dead_code)]
    pub(crate) fn mcp_data_root(
        &self,
        plugin_id: &PluginId,
        manifest_format: PluginManifestFormat,
    ) -> AbsolutePathBuf {
        match manifest_format {
            PluginManifestFormat::AgentPlugin => self.agent_plugin_data_root(plugin_id),
            PluginManifestFormat::Legacy => self.plugin_data_root(plugin_id),
        }
    }

    /// Selects the active installed directory using the common version algorithm.
    #[must_use]
    pub fn active_plugin_version(&self, plugin_id: &PluginId) -> Option<String> {
        super::installed::active_plugin_version(&self.plugin_base_root(plugin_id))
    }

    /// Returns the selected installed version directory, if any.
    #[must_use]
    pub fn active_plugin_root(&self, plugin_id: &PluginId) -> Option<AbsolutePathBuf> {
        self.active_plugin_version(plugin_id)
            .map(|plugin_version| self.plugin_root(plugin_id, &plugin_version))
    }

    // Retained for the source's non-account effective plugin loader.
    #[allow(dead_code)]
    pub(crate) fn active_plugin_installation(
        &self,
        plugin_id: &PluginId,
    ) -> Option<ActivePluginInstallation> {
        Some(ActivePluginInstallation {
            plugin_id: plugin_id.clone(),
            root: self.active_plugin_root(plugin_id)?,
        })
    }

    /// Whether the cache contains a selectable version directory.
    #[must_use]
    pub fn is_installed(&self, plugin_id: &PluginId) -> bool {
        self.active_plugin_version(plugin_id).is_some()
    }

    /// Stages a local package using the version selected from its manifest.
    pub fn install(
        &self,
        source_path: AbsolutePathBuf,
        plugin_id: PluginId,
    ) -> Result<PluginInstallResult, PluginStoreError> {
        self.install_with_manifest(source_path, plugin_id, InstallManifest::OnDisk)
    }

    // Retained for the source Store's marketplace, metadata, and management consumers.
    #[allow(dead_code)]
    pub(crate) fn install_with_fallback_manifest(
        &self,
        source_path: AbsolutePathBuf,
        plugin_id: PluginId,
        manifest_contents: &str,
    ) -> Result<PluginInstallResult, PluginStoreError> {
        self.install_with_manifest(
            source_path,
            plugin_id,
            InstallManifest::Fallback(manifest_contents),
        )
    }

    /// Stages a local package using an explicitly selected cache version.
    pub fn install_with_version(
        &self,
        source_path: AbsolutePathBuf,
        plugin_id: PluginId,
        plugin_version: String,
    ) -> Result<PluginInstallResult, PluginStoreError> {
        self.install_with_version_and_manifest(
            source_path,
            plugin_id,
            plugin_version,
            InstallManifest::OnDisk,
        )
    }

    // Retained for the source Store's marketplace, metadata, and management consumers.
    #[allow(dead_code)]
    pub(crate) fn install_with_version_and_fallback_manifest(
        &self,
        source_path: AbsolutePathBuf,
        plugin_id: PluginId,
        plugin_version: String,
        manifest_contents: &str,
    ) -> Result<PluginInstallResult, PluginStoreError> {
        self.install_with_version_and_manifest(
            source_path,
            plugin_id,
            plugin_version,
            InstallManifest::Fallback(manifest_contents),
        )
    }

    fn install_with_manifest(
        &self,
        source_path: AbsolutePathBuf,
        plugin_id: PluginId,
        manifest: InstallManifest<'_>,
    ) -> Result<PluginInstallResult, PluginStoreError> {
        let manifest = resolve_install_manifest(source_path.as_path(), manifest);
        let plugin_version = plugin_version_for_install_manifest(source_path.as_path(), manifest)?;
        self.install_with_version_and_manifest(source_path, plugin_id, plugin_version, manifest)
    }

    #[allow(clippy::needless_pass_by_value)] // Preserve the source's owned installation flow.
    fn install_with_version_and_manifest(
        &self,
        source_path: AbsolutePathBuf,
        plugin_id: PluginId,
        plugin_version: String,
        manifest: InstallManifest<'_>,
    ) -> Result<PluginInstallResult, PluginStoreError> {
        if !source_path.as_path().is_dir() {
            return Err(PluginStoreError::Invalid(format!(
                "plugin source path is not a directory: {}",
                source_path.display()
            )));
        }

        let manifest = resolve_install_manifest(source_path.as_path(), manifest);
        let plugin_name = plugin_name_for_source(source_path.as_path(), manifest)?;
        if plugin_name != plugin_id.plugin_name {
            return Err(PluginStoreError::Invalid(format!(
                "plugin.json name `{plugin_name}` does not match marketplace plugin name `{}`",
                plugin_id.plugin_name
            )));
        }
        validate_plugin_version_segment(&plugin_version).map_err(PluginStoreError::Invalid)?;
        let installed_path = self.plugin_root(&plugin_id, &plugin_version);
        replace_plugin_root_atomically(
            source_path.as_path(),
            self.plugin_base_root(&plugin_id).as_path(),
            &plugin_version,
            manifest,
        )?;

        Ok(PluginInstallResult {
            plugin_id,
            plugin_version,
            installed_path,
        })
    }

    /// Removes the package cache, preserving its persistent data and product configuration.
    pub fn uninstall(&self, plugin_id: &PluginId) -> Result<(), PluginStoreError> {
        remove_existing_target(self.plugin_base_root(plugin_id).as_path())
    }
}

#[derive(Debug, thiserror::Error)]
/// A package validation failure or contextual filesystem error.
pub enum PluginStoreError {
    #[error("{context}: {source}")]
    /// A contextual filesystem failure; an activation error may follow partial side effects.
    Io {
        /// The original operation context.
        context: &'static str,
        #[source]
        /// The underlying filesystem error.
        source: io::Error,
    },

    #[error("{0}")]
    /// The upstream validation message.
    Invalid(String),
}

impl PluginStoreError {
    fn io(context: &'static str, source: io::Error) -> Self {
        Self::Io { context, source }
    }

    // Retained for the source Store's marketplace, metadata, and management consumers.
    #[allow(dead_code)]
    pub(crate) fn sub_error_type(&self) -> Option<String> {
        match self {
            Self::Io { context, .. } => Some(error_context_sub_error_type(context)),
            Self::Invalid(_) => None,
        }
    }
}

// Retained for the source Store's marketplace, metadata, and management consumers.
#[allow(dead_code)]
pub(crate) fn error_context_sub_error_type(context: &str) -> String {
    context.to_ascii_lowercase().replace(' ', "_")
}

/// Selects and validates an installable source version, including portable version hashing.
pub fn plugin_version_for_source(source_path: &Path) -> Result<String, PluginStoreError> {
    plugin_version_for_install_manifest(source_path, InstallManifest::OnDisk)
}

// Retained for the source Store's marketplace, metadata, and management consumers.
#[allow(dead_code)]
pub(crate) fn plugin_version_for_source_with_fallback_manifest(
    source_path: &Path,
    manifest_contents: &str,
) -> Result<String, PluginStoreError> {
    let manifest =
        resolve_install_manifest(source_path, InstallManifest::Fallback(manifest_contents));
    plugin_version_for_install_manifest(source_path, manifest)
}

fn resolve_install_manifest<'a>(
    source_path: &Path,
    manifest: InstallManifest<'a>,
) -> InstallManifest<'a> {
    // A real plugin manifest always wins. The fallback only fills the gap for marketplace
    // sources that cannot be changed in place because they may be user-owned directories.
    match manifest {
        InstallManifest::Fallback(_) if find_plugin_manifest_path(source_path).is_some() => {
            InstallManifest::OnDisk
        }
        manifest => manifest,
    }
}

fn plugin_version_for_install_manifest(
    source_path: &Path,
    manifest: InstallManifest<'_>,
) -> Result<String, PluginStoreError> {
    let (plugin_version, is_agent_plugin) =
        plugin_manifest_version_for_source(source_path, manifest)?;
    let plugin_version = plugin_version.unwrap_or_else(|| {
        if is_agent_plugin {
            DEFAULT_AGENT_PLUGIN_VERSION.to_string()
        } else {
            DEFAULT_PLUGIN_VERSION.to_string()
        }
    });
    match validate_plugin_version_segment(&plugin_version) {
        Ok(()) => Ok(plugin_version),
        Err(_) if is_agent_plugin => {
            let digest = Sha256::digest(plugin_version.as_bytes());
            Ok(format!(
                "agent-plugins-{}",
                hex_prefix(&digest, /*count*/ 12)
            ))
        }
        Err(message) => Err(PluginStoreError::Invalid(message)),
    }
}

fn hex_prefix(bytes: &[u8], count: usize) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(count.saturating_mul(2));
    for byte in bytes.iter().take(count) {
        output.push(HEX[usize::from(byte >> 4)] as char);
        output.push(HEX[usize::from(byte & 0x0f)] as char);
    }
    output
}

/// Validates a single version directory segment using the common package contract.
pub fn validate_plugin_version_segment(plugin_version: &str) -> Result<(), String> {
    super::installed::validate_plugin_version_segment(plugin_version)
}

fn plugin_manifest_for_source(
    source_path: &Path,
    manifest: InstallManifest<'_>,
) -> Result<PluginManifest, PluginStoreError> {
    match manifest {
        InstallManifest::OnDisk => load_plugin_manifest(source_path)
            .ok_or_else(|| PluginStoreError::Invalid("missing or invalid plugin.json".to_string())),
        InstallManifest::Fallback(contents) => parse_plugin_manifest(
            source_path,
            &source_path.join(".codex-plugin/plugin.json"),
            contents,
        )
        .map_err(|err| PluginStoreError::Invalid(format!("failed to parse plugin.json: {err}"))),
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawPluginManifestVersion {
    #[serde(default)]
    version: Option<JsonValue>,
}

fn plugin_manifest_version_for_source(
    source_path: &Path,
    manifest: InstallManifest<'_>,
) -> Result<(Option<String>, bool), PluginStoreError> {
    let contents = match manifest {
        InstallManifest::OnDisk => {
            let manifest_path = find_plugin_manifest_path(source_path)
                .ok_or_else(|| PluginStoreError::Invalid("missing plugin.json".to_string()))?;
            fs::read_to_string(&manifest_path)
                .map_err(|err| PluginStoreError::io("failed to read plugin.json", err))?
        }
        InstallManifest::Fallback(contents) => contents.to_string(),
    };
    let is_agent_plugin =
        agent_plugin_schema_status(&contents) == AgentPluginSchemaStatus::Supported;
    let manifest: RawPluginManifestVersion = serde_json::from_str(&contents)
        .map_err(|err| PluginStoreError::Invalid(format!("failed to parse plugin.json: {err}")))?;
    let Some(version) = manifest.version else {
        return Ok((None, is_agent_plugin));
    };
    let Some(version) = version.as_str() else {
        return Err(PluginStoreError::Invalid(
            "invalid plugin version in plugin.json: expected string".to_string(),
        ));
    };
    if is_agent_plugin {
        let version = version.trim();
        return Ok(((!version.is_empty()).then(|| version.to_string()), true));
    }
    let version = version.trim();
    if version.is_empty() {
        return Err(PluginStoreError::Invalid(
            "invalid plugin version in plugin.json: must not be blank".to_string(),
        ));
    }
    Ok((Some(version.to_string()), false))
}

fn plugin_name_for_source(
    source_path: &Path,
    manifest: InstallManifest<'_>,
) -> Result<String, PluginStoreError> {
    let manifest = plugin_manifest_for_source(source_path, manifest)?;

    let plugin_name = manifest.name;
    validate_plugin_segment(&plugin_name, "plugin name")
        .map_err(PluginStoreError::Invalid)
        .map(|()| plugin_name)
}

fn remove_existing_target(path: &Path) -> Result<(), PluginStoreError> {
    if !path.exists() {
        return Ok(());
    }

    if path.is_dir() {
        fs::remove_dir_all(path).map_err(|err| {
            PluginStoreError::io("failed to remove existing plugin cache entry", err)
        })
    } else {
        fs::remove_file(path).map_err(|err| {
            PluginStoreError::io("failed to remove existing plugin cache entry", err)
        })
    }
}

fn replace_plugin_root_atomically(
    source: &Path,
    target_root: &Path,
    plugin_version: &str,
    manifest: InstallManifest<'_>,
) -> Result<(), PluginStoreError> {
    replace_plugin_root_using(
        source,
        target_root,
        plugin_version,
        manifest,
        copy_dir_recursive,
        |source, target| fs::rename(source, target),
        |path| fs::remove_dir_all(path),
    )
}

// Keep source operation ordering while allowing deterministic filesystem failures in external tests.
#[allow(clippy::too_many_lines)]
fn replace_plugin_root_using(
    source: &Path,
    target_root: &Path,
    plugin_version: &str,
    manifest: InstallManifest<'_>,
    mut copy: impl FnMut(&Path, &Path) -> Result<(), PluginStoreError>,
    mut rename: impl FnMut(&Path, &Path) -> io::Result<()>,
    remove_directory: impl FnMut(&Path) -> io::Result<()>,
) -> Result<(), PluginStoreError> {
    let Some(parent) = target_root.parent() else {
        return Err(PluginStoreError::Invalid(format!(
            "plugin cache path has no parent: {}",
            target_root.display()
        )));
    };

    fs::create_dir_all(parent)
        .map_err(|err| PluginStoreError::io("failed to create plugin cache directory", err))?;

    let Some(plugin_dir_name) = target_root.file_name() else {
        return Err(PluginStoreError::Invalid(format!(
            "plugin cache path has no directory name: {}",
            target_root.display()
        )));
    };
    let staged_dir = tempfile::Builder::new()
        .prefix("plugin-install-")
        .tempdir_in(parent)
        .map_err(|err| {
            PluginStoreError::io("failed to create temporary plugin cache directory", err)
        })?;
    let staged_root = staged_dir.path().join(plugin_dir_name);
    let staged_version_root = staged_root.join(plugin_version);
    let (source_manifest_relative_path, source_manifest_contents) = match manifest {
        InstallManifest::OnDisk => {
            let manifest_path = find_plugin_manifest_path(source)
                .ok_or_else(|| PluginStoreError::Invalid("missing plugin.json".to_string()))?;
            let relative_path = manifest_path
                .strip_prefix(source)
                .map_err(|_| {
                    PluginStoreError::Invalid(
                        "plugin manifest is outside the plugin source".to_string(),
                    )
                })?
                .to_path_buf();
            let contents = fs::read(&manifest_path)
                .map_err(|err| PluginStoreError::io("failed to read plugin.json", err))?;
            (relative_path, contents)
        }
        InstallManifest::Fallback(contents) => (
            PathBuf::from(".codex-plugin/plugin.json"),
            contents.as_bytes().to_vec(),
        ),
    };
    copy(source, &staged_version_root)?;
    if let InstallManifest::Fallback(contents) = manifest {
        // Inject the generated manifest into Store's existing atomic copy so install does not
        // mutate the original source or require a second staging directory.
        let manifest_path = staged_version_root.join(".codex-plugin/plugin.json");
        let Some(manifest_parent) = manifest_path.parent() else {
            return Err(PluginStoreError::Invalid(
                "plugin manifest path has no parent".to_string(),
            ));
        };
        fs::create_dir_all(manifest_parent).map_err(|err| {
            PluginStoreError::io("failed to create plugin manifest directory", err)
        })?;
        fs::write(&manifest_path, contents)
            .map_err(|err| PluginStoreError::io("failed to write fallback plugin manifest", err))?;
    }
    let staged_manifest_path =
        find_plugin_manifest_path(&staged_version_root).ok_or_else(|| {
            PluginStoreError::Invalid(
                "plugin manifest is missing after installation staging".to_string(),
            )
        })?;
    if staged_manifest_path != staged_version_root.join(&source_manifest_relative_path) {
        return Err(PluginStoreError::Invalid(
            "plugin manifest changed during installation staging".to_string(),
        ));
    }
    let staged_manifest_contents = fs::read(&staged_manifest_path)
        .map_err(|err| PluginStoreError::io("failed to read staged plugin.json", err))?;
    if staged_manifest_contents != source_manifest_contents {
        return Err(PluginStoreError::Invalid(
            "plugin manifest contents changed during installation staging".to_string(),
        ));
    }
    let is_agent_plugin = fs::read_to_string(staged_version_root.join("plugin.json"))
        .ok()
        .is_some_and(|contents| {
            agent_plugin_schema_status(&contents) == AgentPluginSchemaStatus::Supported
        });
    if !is_agent_plugin && let Err(err) = migrate_plugin_commands(&staged_version_root) {
        tracing::warn!(%err, "failed to migrate plugin commands into skills");
    }

    let target_version_root = target_root.join(plugin_version);
    if target_root.exists() && !target_version_root.exists() {
        rename(&staged_version_root, &target_version_root).map_err(|err| {
            PluginStoreError::io("failed to activate updated plugin cache version", err)
        })?;
        remove_old_plugin_versions(target_root, plugin_version, remove_directory)?;
        return Ok(());
    }

    if target_root.exists() {
        let backup_dir = tempfile::Builder::new()
            .prefix("plugin-backup-")
            .tempdir_in(parent)
            .map_err(|err| {
                PluginStoreError::io("failed to create plugin cache backup directory", err)
            })?;
        let backup_root = backup_dir.path().join(plugin_dir_name);
        rename(target_root, &backup_root)
            .map_err(|err| PluginStoreError::io("failed to back up plugin cache entry", err))?;

        if let Err(err) = rename(&staged_root, target_root) {
            let rollback_result = rename(&backup_root, target_root);
            return match rollback_result {
                Ok(()) => Err(PluginStoreError::io(
                    "failed to activate updated plugin cache entry",
                    err,
                )),
                Err(rollback_err) => {
                    let backup_path = backup_dir.keep().join(plugin_dir_name);
                    Err(PluginStoreError::Invalid(format!(
                        "failed to activate updated plugin cache entry at {}: {err}; failed to restore previous cache entry (left at {}): {rollback_err}",
                        target_root.display(),
                        backup_path.display()
                    )))
                }
            };
        }
    } else {
        rename(&staged_root, target_root)
            .map_err(|err| PluginStoreError::io("failed to activate plugin cache entry", err))?;
    }

    Ok(())
}

fn remove_old_plugin_versions(
    target_root: &Path,
    plugin_version: &str,
    mut remove_directory: impl FnMut(&Path) -> io::Result<()>,
) -> Result<(), PluginStoreError> {
    let Ok(entries) = fs::read_dir(target_root) else {
        return Ok(());
    };

    for entry in entries.filter_map(Result::ok) {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        let Ok(version) = entry.file_name().into_string() else {
            continue;
        };
        if version == plugin_version || validate_plugin_version_segment(&version).is_err() {
            continue;
        }

        if remove_directory(&entry.path()).is_err()
            && old_plugin_version_would_stay_active(&version, plugin_version)
        {
            return Err(PluginStoreError::Invalid(format!(
                "failed to activate updated plugin cache version `{plugin_version}` while `{version}` remains active"
            )));
        }
    }

    Ok(())
}

fn old_plugin_version_would_stay_active(old_version: &str, new_version: &str) -> bool {
    old_version == DEFAULT_PLUGIN_VERSION
        || compare_plugin_versions(old_version, new_version).is_gt()
}

fn compare_plugin_versions(left: &str, right: &str) -> Ordering {
    super::installed::compare_plugin_versions(left, right)
}

fn copy_dir_recursive(source: &Path, target: &Path) -> Result<(), PluginStoreError> {
    fs::create_dir_all(target)
        .map_err(|err| PluginStoreError::io("failed to create plugin target directory", err))?;

    for entry in fs::read_dir(source)
        .map_err(|err| PluginStoreError::io("failed to read plugin source directory", err))?
    {
        let entry =
            entry.map_err(|err| PluginStoreError::io("failed to enumerate plugin source", err))?;
        let source_path = entry.path();
        let target_path = target.join(entry.file_name());
        let file_type = entry
            .file_type()
            .map_err(|err| PluginStoreError::io("failed to inspect plugin source entry", err))?;

        if file_type.is_dir() {
            copy_dir_recursive(&source_path, &target_path)?;
        } else if file_type.is_file() {
            fs::copy(&source_path, &target_path)
                .map_err(|err| PluginStoreError::io("failed to copy plugin file", err))?;
        }
    }

    Ok(())
}

/// Bridges to source-internal Store operations for the external integration-test workspace.
#[cfg(feature = "test-api")]
#[doc(hidden)]
pub mod test_api {
    use super::{
        AbsolutePathBuf, InstallManifest, Path, PluginId, PluginInstallResult, PluginManifest,
        PluginManifestFormat, PluginStore, PluginStoreError, copy_dir_recursive, io,
        old_plugin_version_would_stay_active, plugin_version_for_source_with_fallback_manifest,
        replace_plugin_root_using,
    };

    /// Installs with the source's generated-manifest fallback semantics.
    pub fn install_with_fallback(
        store: &PluginStore,
        source: AbsolutePathBuf,
        id: PluginId,
        contents: &str,
    ) -> Result<PluginInstallResult, PluginStoreError> {
        store.install_with_fallback_manifest(source, id, contents)
    }
    /// Installs an explicit cache version with a fallback manifest.
    pub fn install_version_with_fallback(
        store: &PluginStore,
        source: AbsolutePathBuf,
        id: PluginId,
        version: String,
        contents: &str,
    ) -> Result<PluginInstallResult, PluginStoreError> {
        store.install_with_version_and_fallback_manifest(source, id, version, contents)
    }
    /// Selects a source version with generated-manifest fallback semantics.
    pub fn fallback_version(source: &Path, contents: &str) -> Result<String, PluginStoreError> {
        plugin_version_for_source_with_fallback_manifest(source, contents)
    }
    /// Returns the portable format's persistent data directory.
    #[must_use]
    pub fn agent_data_root(store: &PluginStore, id: &PluginId) -> AbsolutePathBuf {
        store.agent_plugin_data_root(id)
    }
    /// Selects the original MCP data root for a format.
    #[must_use]
    pub fn mcp_data_root(store: &PluginStore, id: &PluginId, portable: bool) -> AbsolutePathBuf {
        store.mcp_data_root(
            id,
            if portable {
                PluginManifestFormat::AgentPlugin
            } else {
                PluginManifestFormat::Legacy
            },
        )
    }
    /// Reads a manifest through the Store's owner-scoped cache.
    #[must_use]
    pub fn load_cached(store: &PluginStore, root: &Path) -> Option<PluginManifest> {
        store
            .manifest_cache
            .load(root)
            .map(|loaded| loaded.manifest)
    }
    /// Reads the explicit, resolved product home.
    #[must_use]
    pub fn home(store: &PluginStore) -> &AbsolutePathBuf {
        store.codex_home()
    }

    /// Returns the selected local installation identity without account metadata.
    #[must_use]
    pub fn active_installation(
        store: &PluginStore,
        id: &PluginId,
    ) -> Option<(PluginId, AbsolutePathBuf)> {
        store
            .active_plugin_installation(id)
            .map(|installation| (installation.plugin_id, installation.root))
    }
    /// Applies the original old-version precedence predicate.
    #[must_use]
    pub fn old_version_stays_active(old: &str, new: &str) -> bool {
        old_plugin_version_would_stay_active(old, new)
    }
    /// Returns the original contextual error classification.
    #[must_use]
    pub fn error_subtype(error: &PluginStoreError) -> Option<String> {
        error.sub_error_type()
    }
    /// Runs the real recursive copier for filesystem fault fixtures.
    pub fn copy_directory(source: &Path, target: &Path) -> Result<(), PluginStoreError> {
        copy_dir_recursive(source, target)
    }
    /// Runs staging and activation with explicitly supplied filesystem operations.
    pub fn replace_with_ops(
        source: &Path,
        target: &Path,
        version: &str,
        fallback: Option<&str>,
        copy: impl FnMut(&Path, &Path) -> Result<(), PluginStoreError>,
        rename: impl FnMut(&Path, &Path) -> io::Result<()>,
        remove_directory: impl FnMut(&Path) -> io::Result<()>,
    ) -> Result<(), PluginStoreError> {
        replace_plugin_root_using(
            source,
            target,
            version,
            fallback.map_or(InstallManifest::OnDisk, InstallManifest::Fallback),
            copy,
            rename,
            remove_directory,
        )
    }
}
