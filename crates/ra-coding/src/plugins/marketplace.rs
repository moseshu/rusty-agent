// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Read-only local marketplace discovery, catalog metadata, and plugin source resolution.

use super::identity::PluginId;
use super::identity::PluginIdError;
use super::manifest::ManifestCache;
use super::manifest::PluginManifestInterface;
use super::{AbsolutePathBuf, join_absolute_path};
use discovery::get_git_repo_root;
use ra_protocol::server::protocol::PluginAuthPolicy;
use ra_protocol::server::protocol::PluginInstallPolicy;
use ra_protocol::server::protocol::Product;
use serde::Deserialize;
use serde_json::Map as JsonMap;
use serde_json::Value as JsonValue;
use std::fs;
use std::io;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use tracing::warn;

mod discovery;

const MARKETPLACE_MANIFEST_RELATIVE_PATHS: &[&str] = &[
    ".agents/plugins/marketplace.json",
    ".agents/plugins/api_marketplace.json",
    ".claude-plugin/marketplace.json",
    ".cursor-plugin/marketplace.json",
];

#[derive(Debug, Clone, PartialEq, Eq)]
/// A resolved catalog entry, with optional readable or fallback manifest metadata.
pub struct ResolvedMarketplacePlugin {
    /// The validated marketplace-qualified package identity.
    pub plugin_id: PluginId,
    /// The source descriptor; no materialization is performed by catalog resolution.
    pub source: MarketplacePluginSource,
    /// The declared policy metadata.
    pub policy: MarketplacePluginPolicy,
    /// Optional parsed presentation metadata.
    pub interface: Option<PluginManifestInterface>,
    /// The readable local or generated metadata-only manifest, if available.
    pub manifest: Option<crate::plugins::manifest::PluginManifest>,
    /// Generated fallback metadata, independently retained from a real manifest.
    pub manifest_fallback: MarketplacePluginManifestFallback,
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// One readable marketplace and its independently resolved plugin entries.
pub struct Marketplace {
    /// The declared marketplace or plugin name.
    pub name: String,
    /// The native source or manifest path.
    pub path: AbsolutePathBuf,
    /// Optional parsed presentation metadata.
    pub interface: Option<MarketplaceInterface>,
    /// Resolved catalog entries in their declared order.
    pub plugins: Vec<MarketplacePlugin>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// A marketplace read failure retained alongside successful catalogs.
pub struct MarketplaceListError {
    /// The native source or manifest path.
    pub path: AbsolutePathBuf,
    /// The original read or validation diagnostic.
    pub message: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
/// Ordered catalogs and accumulated per-marketplace read failures.
pub struct MarketplaceListOutcome {
    /// Successful catalogs, preserving discovery order.
    pub marketplaces: Vec<Marketplace>,
    /// Read failures for otherwise discovered marketplaces.
    pub errors: Vec<MarketplaceListError>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Optional marketplace presentation metadata.
pub struct MarketplaceInterface {
    /// The optional display name, preserving source string semantics.
    pub display_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Generated manifest contents used when a real source manifest is absent.
pub struct MarketplacePluginManifestFallback {
    contents: String,
    has_metadata: bool,
}

impl MarketplacePluginManifestFallback {
    /// Returns the generated manifest JSON without writing it to the source.
    #[must_use]
    pub fn contents(&self) -> &str {
        &self.contents
    }

    pub(crate) fn contents_if_has_metadata(&self) -> Option<&str> {
        self.has_metadata.then_some(self.contents())
    }

    pub(crate) fn parse_for_plugin_root(
        &self,
        plugin_root: &Path,
    ) -> Option<crate::plugins::manifest::PluginManifest> {
        crate::plugins::manifest::parse_plugin_manifest(
            plugin_root,
            &fallback_plugin_manifest_path(plugin_root),
            &self.contents,
        )
        .ok()
    }

    pub(crate) fn parse_for_listing(&self) -> Option<crate::plugins::manifest::PluginManifest> {
        // Materialized sources have no plugin root before install. Parse against a host-native
        // synthetic absolute root, then discard path-bearing fields so listings expose metadata only.
        let plugin_root = Path::new(if cfg!(windows) { r"C:\" } else { "/" });
        let mut manifest = crate::plugins::manifest::parse_plugin_manifest(
            plugin_root,
            &fallback_plugin_manifest_path(plugin_root),
            &self.contents,
        )
        .ok()?;
        manifest.paths = crate::plugins::manifest::PluginManifestPaths {
            onboarding_skill: None,
            skills: Vec::new(),
            mcp_servers: None,
            apps: None,
            hooks: None,
        };
        if let Some(interface) = manifest.interface.as_mut() {
            interface.composer_icon = None;
            interface.logo = None;
            interface.logo_dark = None;
            interface.screenshots.clear();
        }
        Some(manifest)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// One catalog plugin descriptor; this does not imply installation or enablement.
pub struct MarketplacePlugin {
    /// The declared marketplace or plugin name.
    pub name: String,
    /// The parsed manifest version, including metadata-only remote fallback versions.
    pub local_version: Option<String>,
    /// The source descriptor; no materialization is performed by catalog resolution.
    pub source: MarketplacePluginSource,
    /// The declared policy metadata.
    pub policy: MarketplacePluginPolicy,
    /// Optional parsed presentation metadata.
    pub interface: Option<PluginManifestInterface>,
    /// The declared manifest keywords.
    pub keywords: Vec<String>,
    /// Generated fallback metadata, independently retained from a real manifest.
    pub manifest_fallback: Option<MarketplacePluginManifestFallback>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
/// A local path or an unmaterialized Git/npm source descriptor.
pub enum MarketplacePluginSource {
    /// A local source resolved relative to the marketplace root.
    Local {
        /// The native source or manifest path.
        path: AbsolutePathBuf,
    },
    /// A Git descriptor without cloning or filesystem materialization.
    Git {
        /// The normalized Git source URL or local repository spelling.
        url: String,
        /// The optional repository subdirectory.
        path: Option<String>,
        /// The optional trimmed Git reference.
        ref_name: Option<String>,
        /// The optional trimmed commit selector; resolution does not fetch or verify it.
        sha: Option<String>,
    },
    /// An npm registry descriptor without invoking npm.
    Npm {
        /// The normalized npm registry package name.
        package: String,
        /// The optional npm registry version selector.
        version: Option<String>,
        /// The optional validated HTTPS registry URL.
        registry: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NpmPackageScope {
    Scoped,
    Unscoped,
}

impl MarketplacePluginSource {
    // Consumed by source materialization and the later plugin manager.
    #[allow(dead_code)]
    pub(crate) fn is_install_materialized(&self) -> bool {
        matches!(self, Self::Git { .. } | Self::Npm { .. })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Declared installation, authentication timing, and optional product restrictions.
pub struct MarketplacePluginPolicy {
    /// The installation availability declaration.
    pub installation: MarketplacePluginInstallPolicy,
    /// The authentication timing declaration, without an authentication implementation.
    pub authentication: MarketplacePluginAuthPolicy,
    // TODO: Surface or enforce product gating at the Codex/plugin consumer boundary instead of
    // only carrying it through core marketplace metadata.
    /// Optional product restrictions; an explicit empty list rejects installation selection.
    pub products: Option<Vec<Product>>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
/// Marketplace installation policy with the upstream `Available` default.
pub enum MarketplacePluginInstallPolicy {
    #[serde(rename = "NOT_AVAILABLE")]
    /// The package is unavailable for installation.
    NotAvailable,
    #[default]
    #[serde(rename = "AVAILABLE")]
    /// The package may be selected for installation.
    Available,
    #[serde(rename = "INSTALLED_BY_DEFAULT")]
    /// The source declares default installation.
    InstalledByDefault,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
/// Marketplace authentication timing with the upstream `OnInstall` default.
pub enum MarketplacePluginAuthPolicy {
    #[default]
    #[serde(rename = "ON_INSTALL")]
    /// Authentication is declared at installation time.
    OnInstall,
    #[serde(rename = "ON_USE")]
    /// Authentication is declared when the package is used.
    OnUse,
}

impl From<MarketplacePluginInstallPolicy> for PluginInstallPolicy {
    fn from(value: MarketplacePluginInstallPolicy) -> Self {
        match value {
            MarketplacePluginInstallPolicy::NotAvailable => Self::NotAvailable,
            MarketplacePluginInstallPolicy::Available => Self::Available,
            MarketplacePluginInstallPolicy::InstalledByDefault => Self::InstalledByDefault,
        }
    }
}

impl From<MarketplacePluginAuthPolicy> for PluginAuthPolicy {
    fn from(value: MarketplacePluginAuthPolicy) -> Self {
        match value {
            MarketplacePluginAuthPolicy::OnInstall => Self::OnInstall,
            MarketplacePluginAuthPolicy::OnUse => Self::OnUse,
        }
    }
}

#[derive(Debug, thiserror::Error)]
/// A contextual catalog read, layout, identity, or policy-selection error.
pub enum MarketplaceError {
    #[error("{context}: {source}")]
    /// A contextual filesystem read failure.
    Io {
        /// The original filesystem operation context.
        context: &'static str,
        #[source]
        /// The underlying filesystem error.
        source: io::Error,
    },

    #[error("marketplace file `{path}` does not exist")]
    /// The selected marketplace manifest is missing.
    MarketplaceNotFound {
        /// The native source or manifest path.
        path: PathBuf,
    },

    #[error("invalid marketplace file `{path}`: {message}")]
    /// The selected manifest has an invalid layout, shape, or source value.
    InvalidMarketplaceFile {
        /// The native source or manifest path.
        path: PathBuf,
        /// The original read or validation diagnostic.
        message: String,
    },

    #[error("plugin `{plugin_name}` was not found in marketplace `{marketplace_name}`")]
    /// No matching resolvable plugin entry was found.
    PluginNotFound {
        /// The requested plugin name.
        plugin_name: String,
        /// The declared marketplace name.
        marketplace_name: String,
    },

    #[error(
        "plugin `{plugin_name}` is not available for install in marketplace `{marketplace_name}`"
    )]
    /// The matching package is excluded by installation or product policy.
    PluginNotAvailable {
        /// The requested plugin name.
        plugin_name: String,
        /// The declared marketplace name.
        marketplace_name: String,
    },

    #[error("plugins feature is disabled")]
    /// A future configured consumer has disabled plugin support.
    PluginsDisabled,

    #[error("{0}")]
    /// A plugin or marketplace identity failed validation.
    InvalidPlugin(String),
}

impl MarketplaceError {
    fn io(context: &'static str, source: io::Error) -> Self {
        Self::Io { context, source }
    }
}

// TODO: thread an owner-provided manifest cache through standalone install/read workflows.
/// Finds the first matching resolvable entry without applying install availability.
pub fn find_marketplace_plugin(
    marketplace_path: &AbsolutePathBuf,
    plugin_name: &str,
) -> Result<ResolvedMarketplacePlugin, MarketplaceError> {
    let marketplace = load_raw_marketplace_manifest(marketplace_path)?;
    let marketplace_name = marketplace.name;
    let marketplace_name_for_not_found = marketplace_name.clone();
    for plugin in marketplace.plugins {
        if plugin.name != plugin_name {
            continue;
        }

        if let Some(plugin) = resolve_marketplace_plugin_entry(
            marketplace_path,
            &marketplace_name,
            plugin,
            &ManifestCache::disabled(),
        )? {
            return Ok(plugin);
        }
    }

    Err(MarketplaceError::PluginNotFound {
        plugin_name: plugin_name.to_string(),
        marketplace_name: marketplace_name_for_not_found,
    })
}

/// Finds an entry and applies installation and explicit product restrictions.
pub fn find_installable_marketplace_plugin(
    marketplace_path: &AbsolutePathBuf,
    plugin_name: &str,
    restriction_product: Option<Product>,
) -> Result<ResolvedMarketplacePlugin, MarketplaceError> {
    let resolved = find_marketplace_plugin(marketplace_path, plugin_name)?;
    let product_allowed = match resolved.policy.products.as_deref() {
        None => true,
        Some([]) => false,
        Some(products) => {
            restriction_product.is_some_and(|product| product.matches_product_restriction(products))
        }
    };
    if resolved.policy.installation == MarketplacePluginInstallPolicy::NotAvailable
        || !product_allowed
    {
        return Err(MarketplaceError::PluginNotAvailable {
            plugin_name: resolved.plugin_id.plugin_name,
            marketplace_name: resolved.plugin_id.marketplace_name,
        });
    }

    Ok(resolved)
}

/// Lists supported local catalogs from home and selected roots without installation or network I/O.
pub fn list_marketplaces(
    additional_roots: &[AbsolutePathBuf],
) -> Result<MarketplaceListOutcome, MarketplaceError> {
    list_marketplaces_with_home(additional_roots, home_dir().as_deref())
}

pub(crate) fn home_dir() -> Option<PathBuf> {
    ["HOME", "USERPROFILE"]
        .into_iter()
        .filter_map(std::env::var_os)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .find(|path| path.is_absolute())
        .or_else(dirs::home_dir)
}

/// Reads a supported catalog under a root and returns its declared marketplace name.
pub fn validate_marketplace_root(root: &Path) -> Result<String, MarketplaceError> {
    let Some(path) = find_marketplace_manifest_path(root) else {
        return Err(MarketplaceError::InvalidMarketplaceFile {
            path: root.to_path_buf(),
            message: "marketplace root does not contain a supported manifest".to_string(),
        });
    };
    let marketplace = load_marketplace(&path)?;
    Ok(marketplace.name)
}

/// Chooses the first supported manifest layout under a root.
#[must_use]
pub fn find_marketplace_manifest_path(root: &Path) -> Option<AbsolutePathBuf> {
    MARKETPLACE_MANIFEST_RELATIVE_PATHS
        .iter()
        .find_map(|relative_path| {
            let path = root.join(relative_path);
            if !path.is_file() {
                return None;
            }
            AbsolutePathBuf::try_from(path).ok()
        })
}

fn supported_marketplace_manifest_path(path: &Path) -> Option<AbsolutePathBuf> {
    if !path.is_file() {
        return None;
    }
    if !MARKETPLACE_MANIFEST_RELATIVE_PATHS
        .iter()
        .any(|relative_path| marketplace_root_from_layout(path, relative_path).is_some())
    {
        return None;
    }
    AbsolutePathBuf::try_from(path.to_path_buf()).ok()
}

fn invalid_marketplace_layout_error(path: &AbsolutePathBuf) -> MarketplaceError {
    MarketplaceError::InvalidMarketplaceFile {
        path: path.to_path_buf(),
        message: "marketplace file is not in a supported location".to_string(),
    }
}

fn marketplace_root_from_layout(marketplace_path: &Path, relative_path: &str) -> Option<PathBuf> {
    let mut current = marketplace_path;
    for component in Path::new(relative_path).components().rev() {
        let Component::Normal(expected) = component else {
            return None;
        };
        if current.file_name() != Some(expected) {
            return None;
        }
        current = current.parent()?;
    }
    Some(current.to_path_buf())
}

// TODO: allow standalone callers to share a cache across marketplace loads.
/// Loads a catalog with independently resolved entries and an uncached manifest owner.
pub fn load_marketplace(path: &AbsolutePathBuf) -> Result<Marketplace, MarketplaceError> {
    load_marketplace_with_cache(path, &ManifestCache::disabled())
}

fn load_marketplace_with_cache(
    path: &AbsolutePathBuf,
    manifest_cache: &ManifestCache,
) -> Result<Marketplace, MarketplaceError> {
    let marketplace = load_raw_marketplace_manifest(path)?;
    let mut plugins = Vec::new();

    for plugin in marketplace.plugins {
        let plugin =
            match resolve_marketplace_plugin_entry(path, &marketplace.name, plugin, manifest_cache)
            {
                Ok(Some(plugin)) => plugin,
                Ok(None) => continue,
                Err(MarketplaceError::InvalidPlugin(message)) => {
                    warn!(
                        path = %path.display(),
                        marketplace = %marketplace.name,
                        error = %message,
                        "skipping invalid marketplace plugin"
                    );
                    continue;
                }
                Err(err) => return Err(err),
            };

        let manifest_fallback = plugin
            .manifest_fallback
            .contents_if_has_metadata()
            .map(|_| plugin.manifest_fallback.clone());
        let local_version = plugin
            .manifest
            .as_ref()
            .and_then(|manifest| manifest.version.clone());
        let keywords = plugin
            .manifest
            .map(|manifest| manifest.keywords)
            .unwrap_or_default();

        plugins.push(MarketplacePlugin {
            name: plugin.plugin_id.plugin_name,
            local_version,
            source: plugin.source,
            policy: plugin.policy,
            interface: plugin.interface,
            keywords,
            manifest_fallback,
        });
    }

    Ok(Marketplace {
        name: marketplace.name,
        path: path.clone(),
        interface: resolve_marketplace_interface(marketplace.interface),
        plugins,
    })
}

// Standalone consumers use an uncached owner; configured consumers can share the owner's cache.
#[doc(hidden)]
/// Lists selected roots with an explicit optional home, preserving source discovery order.
pub fn list_marketplaces_with_home(
    additional_roots: &[AbsolutePathBuf],
    home_dir: Option<&Path>,
) -> Result<MarketplaceListOutcome, MarketplaceError> {
    list_marketplaces_with_cache(additional_roots, home_dir, &ManifestCache::disabled())
}

// Keep the upstream fallible surface shared with standalone and configured consumers.
#[allow(clippy::unnecessary_wraps)]
pub(crate) fn list_marketplaces_with_cache(
    additional_roots: &[AbsolutePathBuf],
    home_dir: Option<&Path>,
    manifest_cache: &ManifestCache,
) -> Result<MarketplaceListOutcome, MarketplaceError> {
    let mut outcome = MarketplaceListOutcome::default();

    for marketplace_path in discover_marketplace_paths_from_roots(additional_roots, home_dir) {
        match load_marketplace_with_cache(&marketplace_path, manifest_cache) {
            Ok(marketplace) => outcome.marketplaces.push(marketplace),
            Err(err) => {
                warn!(
                    path = %marketplace_path.display(),
                    error = %err,
                    "skipping marketplace that failed to load"
                );
                outcome.errors.push(MarketplaceListError {
                    path: marketplace_path,
                    message: err.to_string(),
                });
            }
        }
    }

    Ok(outcome)
}

fn discover_marketplace_paths_from_roots(
    additional_roots: &[AbsolutePathBuf],
    home_dir: Option<&Path>,
) -> Vec<AbsolutePathBuf> {
    let mut paths = Vec::new();

    if let Some(home) = home_dir
        && let Some(path) = find_marketplace_manifest_path(home)
    {
        paths.push(path);
    }

    for root in additional_roots {
        if let Some(path) = supported_marketplace_manifest_path(root.as_path())
            && !paths.contains(&path)
        {
            paths.push(path);
            continue;
        }
        // Curated marketplaces can now come from an HTTP-downloaded directory that is not a git
        // checkout, so check the root directly before falling back to repo-root discovery.
        if let Some(path) = find_marketplace_manifest_path(root.as_path())
            && !paths.contains(&path)
        {
            paths.push(path);
            continue;
        }
        if let Some(repo_root) = get_git_repo_root(root.as_path())
            && let Ok(repo_root) = AbsolutePathBuf::try_from(repo_root)
            && let Some(path) = find_marketplace_manifest_path(repo_root.as_path())
            && !paths.contains(&path)
        {
            paths.push(path);
        }
    }

    paths
}

fn load_raw_marketplace_manifest(
    path: &AbsolutePathBuf,
) -> Result<RawMarketplaceManifest, MarketplaceError> {
    let contents = fs::read_to_string(path.as_path()).map_err(|err| {
        if err.kind() == io::ErrorKind::NotFound {
            MarketplaceError::MarketplaceNotFound {
                path: path.to_path_buf(),
            }
        } else {
            MarketplaceError::io("failed to read marketplace file", err)
        }
    })?;
    serde_json::from_str(&contents).map_err(|err| MarketplaceError::InvalidMarketplaceFile {
        path: path.to_path_buf(),
        message: err.to_string(),
    })
}

fn resolve_marketplace_plugin_entry(
    marketplace_path: &AbsolutePathBuf,
    marketplace_name: &str,
    plugin: RawMarketplaceManifestPlugin,
    manifest_cache: &ManifestCache,
) -> Result<Option<ResolvedMarketplacePlugin>, MarketplaceError> {
    let RawMarketplaceManifestPlugin {
        name,
        source,
        policy,
        category,
        manifest_fields,
    } = plugin;
    let Some(source) = resolve_supported_plugin_source(marketplace_path, &name, source) else {
        return Ok(None);
    };
    let manifest_fallback =
        marketplace_plugin_manifest_fallback(&name, category.as_deref(), &manifest_fields);

    let manifest = match &source {
        MarketplacePluginSource::Local { path } => {
            if super::discovery::find_plugin_manifest_path(path.as_path()).is_some() {
                manifest_cache
                    .load(path.as_path())
                    .map(|loaded| loaded.manifest)
            } else if manifest_fallback.has_metadata {
                manifest_fallback.parse_for_plugin_root(path.as_path())
            } else {
                None
            }
        }
        MarketplacePluginSource::Git { .. } | MarketplacePluginSource::Npm { .. }
            if manifest_fallback.has_metadata =>
        {
            manifest_fallback.parse_for_listing()
        }
        MarketplacePluginSource::Git { .. } | MarketplacePluginSource::Npm { .. } => None,
    };
    let interface = plugin_interface_with_marketplace_category(
        manifest
            .as_ref()
            .and_then(|manifest| manifest.interface.clone()),
        category,
    );

    Ok(Some(ResolvedMarketplacePlugin {
        plugin_id: PluginId::new(name, marketplace_name.to_string()).map_err(|err| match err {
            PluginIdError::Invalid(message) => MarketplaceError::InvalidPlugin(message),
        })?,
        source,
        policy: MarketplacePluginPolicy {
            installation: policy.installation,
            authentication: policy.authentication,
            products: policy.products,
        },
        interface,
        manifest,
        manifest_fallback,
    }))
}

fn resolve_supported_plugin_source(
    marketplace_path: &AbsolutePathBuf,
    plugin_name: &str,
    source: RawMarketplaceManifestPluginSource,
) -> Option<MarketplacePluginSource> {
    match source {
        RawMarketplaceManifestPluginSource::Unsupported(_) => {
            warn!(
                path = %marketplace_path.display(),
                plugin = plugin_name,
                "skipping marketplace plugin with unsupported source"
            );
            None
        }
        source => match resolve_plugin_source(marketplace_path, source) {
            Ok(source) => Some(source),
            Err(err) => {
                warn!(
                    path = %marketplace_path.display(),
                    plugin = plugin_name,
                    error = %err,
                    "skipping marketplace plugin that failed to resolve"
                );
                None
            }
        },
    }
}

fn resolve_plugin_source(
    marketplace_path: &AbsolutePathBuf,
    source: RawMarketplaceManifestPluginSource,
) -> Result<MarketplacePluginSource, MarketplaceError> {
    match source {
        RawMarketplaceManifestPluginSource::Path(path)
        | RawMarketplaceManifestPluginSource::Object(
            RawMarketplaceManifestPluginSourceObject::Local { path },
        ) => Ok(MarketplacePluginSource::Local {
            path: resolve_local_plugin_source_path(marketplace_path, &path)?,
        }),
        RawMarketplaceManifestPluginSource::Object(
            RawMarketplaceManifestPluginSourceObject::Url {
                url,
                path,
                ref_name,
                sha,
            },
        ) => Ok(MarketplacePluginSource::Git {
            url: normalize_git_plugin_source_url(marketplace_path, &url)?,
            path: path
                .as_deref()
                .map(|path| normalize_remote_plugin_subdir(marketplace_path, path))
                .transpose()?,
            ref_name: normalize_optional_git_selector(&ref_name),
            sha: normalize_optional_git_selector(&sha),
        }),
        RawMarketplaceManifestPluginSource::Object(
            RawMarketplaceManifestPluginSourceObject::GitSubdir {
                url,
                path,
                ref_name,
                sha,
            },
        ) => Ok(MarketplacePluginSource::Git {
            url: normalize_git_plugin_source_url(marketplace_path, &url)?,
            path: Some(normalize_remote_plugin_subdir(marketplace_path, &path)?),
            ref_name: normalize_optional_git_selector(&ref_name),
            sha: normalize_optional_git_selector(&sha),
        }),
        RawMarketplaceManifestPluginSource::Object(
            RawMarketplaceManifestPluginSourceObject::Npm {
                package,
                version,
                registry,
            },
        ) => Ok(MarketplacePluginSource::Npm {
            package: normalize_npm_package(marketplace_path, &package)?,
            version: normalize_optional_npm_version(marketplace_path, version)?,
            registry: normalize_optional_npm_registry(marketplace_path, registry)?,
        }),
        RawMarketplaceManifestPluginSource::Unsupported(_) => {
            unreachable!("unsupported plugin sources should be filtered before resolution")
        }
    }
}

fn resolve_local_plugin_source_path(
    marketplace_path: &AbsolutePathBuf,
    path: &str,
) -> Result<AbsolutePathBuf, MarketplaceError> {
    match path {
        "" => {
            return Err(MarketplaceError::InvalidMarketplaceFile {
                path: marketplace_path.to_path_buf(),
                message: "local plugin source path must not be empty".to_string(),
            });
        }
        "." | "./" => return marketplace_root_dir(marketplace_path),
        _ => {}
    }

    // Non-root local sources must keep the explicit `./` prefix and remain normalized.
    let relative_path = path.strip_prefix("./").or_else(|| {
        marketplace_path
            .as_path()
            .ends_with(".cursor-plugin/marketplace.json")
            .then_some(path)
    });
    let Some(relative_path) = relative_path else {
        return Err(MarketplaceError::InvalidMarketplaceFile {
            path: marketplace_path.to_path_buf(),
            message: "local plugin source path must start with `./`".to_string(),
        });
    };

    let relative_source_path = Path::new(relative_path);
    if relative_source_path
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(MarketplaceError::InvalidMarketplaceFile {
            path: marketplace_path.to_path_buf(),
            message: "local plugin source path must stay within the marketplace root".to_string(),
        });
    }

    // `marketplace.json` lives under a supported marketplace layout beneath `<root>`,
    // but local plugin paths are resolved relative to `<root>`.
    Ok(join_absolute_path(
        &marketplace_root_dir(marketplace_path)?,
        relative_source_path,
    ))
}

fn normalize_remote_plugin_subdir(
    marketplace_path: &AbsolutePathBuf,
    path: &str,
) -> Result<String, MarketplaceError> {
    let path = path.trim();
    let path = path.strip_prefix("./").unwrap_or(path);
    if path.is_empty() {
        return Err(MarketplaceError::InvalidMarketplaceFile {
            path: marketplace_path.to_path_buf(),
            message: "git plugin source path must not be empty".to_string(),
        });
    }
    let relative_path = Path::new(path);
    if relative_path
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(MarketplaceError::InvalidMarketplaceFile {
            path: marketplace_path.to_path_buf(),
            message: "git plugin source path must stay within the repository root".to_string(),
        });
    }
    Ok(path.to_string())
}

fn normalize_git_plugin_source_url(
    marketplace_path: &AbsolutePathBuf,
    url: &str,
) -> Result<String, MarketplaceError> {
    let url = url.trim();
    if url.is_empty() {
        return Err(MarketplaceError::InvalidMarketplaceFile {
            path: marketplace_path.to_path_buf(),
            message: "git plugin source url must not be empty".to_string(),
        });
    }
    if url.starts_with("http://") || url.starts_with("https://") {
        return Ok(normalize_github_git_url(url));
    }
    if url.starts_with("./")
        || url.starts_with("../")
        || url.starts_with(".\\")
        || url.starts_with("..\\")
    {
        return normalize_relative_git_plugin_source_url(marketplace_path, url);
    }
    if url.starts_with("file://") || url.starts_with('/') {
        return Ok(url.to_string());
    }
    if url.starts_with("ssh://") || url.starts_with("git@") && url.contains(':') {
        return Ok(url.to_string());
    }
    if let Some(url) = normalize_github_shorthand_url(url) {
        return Ok(url);
    }

    Err(MarketplaceError::InvalidMarketplaceFile {
        path: marketplace_path.to_path_buf(),
        message: format!("invalid git plugin source url: {url}"),
    })
}

fn normalize_relative_git_plugin_source_url(
    marketplace_path: &AbsolutePathBuf,
    url: &str,
) -> Result<String, MarketplaceError> {
    let mut normalized = marketplace_root_dir(marketplace_path)?
        .as_path()
        .to_path_buf();
    for segment in url.split(['/', '\\']) {
        match segment {
            "" | "." => {}
            ".." => {
                return Err(MarketplaceError::InvalidMarketplaceFile {
                    path: marketplace_path.to_path_buf(),
                    message: "relative git plugin source url must stay within the marketplace root"
                        .to_string(),
                });
            }
            segment => normalized.push(segment),
        }
    }
    Ok(normalized.display().to_string())
}

// Keep the upstream parser helper's borrowed input and normalization order.
#[allow(clippy::ref_option)]
fn normalize_optional_git_selector(value: &Option<String>) -> Option<String> {
    value
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn normalize_npm_package(
    marketplace_path: &AbsolutePathBuf,
    package: &str,
) -> Result<String, MarketplaceError> {
    let package = package.trim();
    let package_scope = if package.starts_with('@') {
        NpmPackageScope::Scoped
    } else {
        NpmPackageScope::Unscoped
    };
    let segments = if let Some(scoped_package) = package.strip_prefix('@') {
        scoped_package.split('/').collect::<Vec<_>>()
    } else {
        package.split('/').collect::<Vec<_>>()
    };
    let expected_segments = match package_scope {
        NpmPackageScope::Scoped => 2,
        NpmPackageScope::Unscoped => 1,
    };
    if package.is_empty()
        || segments.len() != expected_segments
        || segments
            .iter()
            .any(|segment| !is_valid_npm_package_segment(segment, package_scope))
    {
        return Err(MarketplaceError::InvalidMarketplaceFile {
            path: marketplace_path.to_path_buf(),
            message: format!("invalid npm plugin source package: {package}"),
        });
    }
    Ok(package.to_string())
}

fn is_valid_npm_package_segment(segment: &str, package_scope: NpmPackageScope) -> bool {
    !segment.is_empty()
        && segment != "."
        && segment != ".."
        && (package_scope == NpmPackageScope::Scoped
            || !matches!(segment.chars().next(), Some('.' | '_')))
        && segment
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
}

fn normalize_optional_npm_version(
    marketplace_path: &AbsolutePathBuf,
    version: Option<String>,
) -> Result<Option<String>, MarketplaceError> {
    let Some(version) = normalize_optional_npm_source_field(marketplace_path, version, "version")?
    else {
        return Ok(None);
    };
    if !is_registry_npm_version_selector(&version) {
        return Err(MarketplaceError::InvalidMarketplaceFile {
            path: marketplace_path.to_path_buf(),
            message: format!("npm plugin source version must use the registry: {version}"),
        });
    }
    Ok(Some(version))
}

fn is_registry_npm_version_selector(version: &str) -> bool {
    version != "." && version != ".." && !version.chars().any(|ch| matches!(ch, '/' | '\\' | ':'))
}

fn normalize_optional_npm_registry(
    marketplace_path: &AbsolutePathBuf,
    registry: Option<String>,
) -> Result<Option<String>, MarketplaceError> {
    let Some(registry) =
        normalize_optional_npm_source_field(marketplace_path, registry, "registry")?
    else {
        return Ok(None);
    };
    let parsed =
        url::Url::parse(&registry).map_err(|_| MarketplaceError::InvalidMarketplaceFile {
            path: marketplace_path.to_path_buf(),
            message: format!("invalid npm plugin source registry: {registry}"),
        })?;
    if parsed.scheme() != "https"
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err(MarketplaceError::InvalidMarketplaceFile {
            path: marketplace_path.to_path_buf(),
            message: format!("invalid npm plugin source registry: {registry}"),
        });
    }
    Ok(Some(registry))
}

fn normalize_optional_npm_source_field(
    marketplace_path: &AbsolutePathBuf,
    value: Option<String>,
    field: &str,
) -> Result<Option<String>, MarketplaceError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let value = value.trim();
    if value.is_empty() {
        return Err(MarketplaceError::InvalidMarketplaceFile {
            path: marketplace_path.to_path_buf(),
            message: format!("npm plugin source {field} must not be empty"),
        });
    }
    Ok(Some(value.to_string()))
}

// The upstream URL suffix is case-sensitive; this is not a filesystem extension check.
#[allow(clippy::case_sensitive_file_extension_comparisons)]
fn normalize_github_git_url(url: &str) -> String {
    if url.starts_with("https://github.com/") && !url.ends_with(".git") {
        format!("{url}.git")
    } else {
        url.to_string()
    }
}

fn normalize_github_shorthand_url(source: &str) -> Option<String> {
    if !looks_like_github_shorthand(source) {
        return None;
    }
    let mut segments = source.split('/');
    let owner = segments.next()?;
    let repo = segments.next()?;
    let repo = repo.strip_suffix(".git").unwrap_or(repo);
    if repo.is_empty() {
        return None;
    }
    Some(format!("https://github.com/{owner}/{repo}.git"))
}

fn looks_like_github_shorthand(source: &str) -> bool {
    let mut segments = source.split('/');
    let owner = segments.next();
    let repo = segments.next();
    let extra = segments.next();
    owner.is_some_and(is_github_shorthand_segment)
        && repo.is_some_and(is_github_shorthand_segment)
        && extra.is_none()
}

fn is_github_shorthand_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
}

/// Applies source marketplace taxonomy over a manifest interface category.
#[must_use]
pub fn plugin_interface_with_marketplace_category(
    mut interface: Option<PluginManifestInterface>,
    category: Option<String>,
) -> Option<PluginManifestInterface> {
    if let Some(category) = category {
        // Marketplace taxonomy wins when both sources provide a category.
        interface
            .get_or_insert_with(PluginManifestInterface::default)
            .category = Some(category);
    }
    interface
}

#[doc(hidden)]
/// Recovers a marketplace root from an accepted manifest layout.
pub fn marketplace_root_dir(
    marketplace_path: &AbsolutePathBuf,
) -> Result<AbsolutePathBuf, MarketplaceError> {
    for relative_path in MARKETPLACE_MANIFEST_RELATIVE_PATHS {
        if let Some(marketplace_root) =
            marketplace_root_from_layout(marketplace_path.as_path(), relative_path)
        {
            return AbsolutePathBuf::try_from(marketplace_root)
                .map_err(|_| invalid_marketplace_layout_error(marketplace_path));
        }
    }

    Err(invalid_marketplace_layout_error(marketplace_path))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawMarketplaceManifest {
    name: String,
    #[serde(default)]
    interface: Option<RawMarketplaceManifestInterface>,
    plugins: Vec<RawMarketplaceManifestPlugin>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawMarketplaceManifestInterface {
    #[serde(default)]
    display_name: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawMarketplaceManifestPlugin {
    name: String,
    source: RawMarketplaceManifestPluginSource,
    #[serde(default)]
    policy: RawMarketplaceManifestPluginPolicy,
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    #[serde(flatten)]
    manifest_fields: JsonMap<String, JsonValue>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawMarketplaceManifestPluginPolicy {
    #[serde(default)]
    installation: MarketplacePluginInstallPolicy,
    #[serde(default)]
    authentication: MarketplacePluginAuthPolicy,
    products: Option<Vec<Product>>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawMarketplaceManifestPluginSource {
    Path(String),
    Object(RawMarketplaceManifestPluginSourceObject),
    #[allow(dead_code)]
    Unsupported(JsonValue),
}

#[derive(Debug, Deserialize)]
#[serde(tag = "source", rename_all = "lowercase")]
enum RawMarketplaceManifestPluginSourceObject {
    Local {
        path: String,
    },
    Url {
        url: String,
        path: Option<String>,
        #[serde(rename = "ref")]
        ref_name: Option<String>,
        sha: Option<String>,
    },
    #[serde(rename = "git-subdir")]
    GitSubdir {
        url: String,
        path: String,
        #[serde(rename = "ref")]
        ref_name: Option<String>,
        sha: Option<String>,
    },
    Npm {
        package: String,
        version: Option<String>,
        registry: Option<String>,
    },
}

fn resolve_marketplace_interface(
    interface: Option<RawMarketplaceManifestInterface>,
) -> Option<MarketplaceInterface> {
    let interface = interface?;
    if interface.display_name.is_some() {
        Some(MarketplaceInterface {
            display_name: interface.display_name,
        })
    } else {
        None
    }
}

fn fallback_plugin_manifest_path(plugin_root: &Path) -> PathBuf {
    plugin_root.join(".codex-plugin/plugin.json")
}

fn marketplace_plugin_manifest_fallback(
    name: &str,
    category: Option<&str>,
    manifest_fields: &JsonMap<String, JsonValue>,
) -> MarketplacePluginManifestFallback {
    let mut manifest = manifest_fields.clone();
    manifest.insert("name".to_string(), JsonValue::String(name.to_string()));
    if let Some(category) = category {
        manifest.insert(
            "category".to_string(),
            JsonValue::String(category.to_string()),
        );
    }
    if let Some(interface) = plugin_manifest_interface(manifest_fields, category) {
        manifest.insert("interface".to_string(), interface);
    }

    let contents = serde_json::to_string_pretty(&JsonValue::Object(manifest))
        .unwrap_or_else(|_| format!(r#"{{"name":"{name}"}}"#));
    MarketplacePluginManifestFallback {
        contents,
        has_metadata: !manifest_fields.is_empty() || category.is_some(),
    }
}

fn plugin_manifest_interface(
    fields: &JsonMap<String, JsonValue>,
    category: Option<&str>,
) -> Option<JsonValue> {
    let mut interface = fields
        .get("interface")
        .and_then(JsonValue::as_object)
        .cloned()
        .unwrap_or_default();

    if !interface.contains_key("displayName")
        && let Some(display_name) = fields.get("displayName").and_then(JsonValue::as_str)
    {
        interface.insert(
            "displayName".to_string(),
            JsonValue::String(display_name.to_string()),
        );
    }
    if !interface.contains_key("developerName")
        && let Some(author_name) = fields
            .get("author")
            .and_then(|author| author.get("name"))
            .and_then(JsonValue::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
    {
        interface.insert(
            "developerName".to_string(),
            JsonValue::String(author_name.to_string()),
        );
    }
    if !interface.contains_key("websiteUrl")
        && !interface.contains_key("websiteURL")
        && let Some(homepage) = fields.get("homepage").and_then(JsonValue::as_str)
    {
        interface.insert(
            "websiteUrl".to_string(),
            JsonValue::String(homepage.to_string()),
        );
    }
    if let Some(category) = category.map(str::trim).filter(|value| !value.is_empty()) {
        interface.insert(
            "category".to_string(),
            JsonValue::String(category.to_string()),
        );
    }

    (!interface.is_empty()).then_some(JsonValue::Object(interface))
}

/// Bridges to source-internal metadata and owner cache operations for external tests.
#[cfg(feature = "test-api")]
#[doc(hidden)]
pub mod test_api {
    use super::{MarketplaceError, MarketplaceListOutcome, MarketplacePluginManifestFallback};
    use crate::plugins::{AbsolutePathBuf, store::PluginStore};
    use std::path::Path;

    /// Constructs the private source record from explicit expected fixture fields.
    #[must_use]
    pub fn fallback_fixture(
        contents: String,
        has_metadata: bool,
    ) -> MarketplacePluginManifestFallback {
        MarketplacePluginManifestFallback {
            contents,
            has_metadata,
        }
    }
    /// Reads the source record's metadata-presence flag.
    #[must_use]
    pub fn has_metadata(fallback: &MarketplacePluginManifestFallback) -> bool {
        fallback.has_metadata
    }
    /// Calls relative Git URL resolution without altering its original diagnostics.
    pub fn normalize_relative_git_plugin_source_url(
        path: &AbsolutePathBuf,
        url: &str,
    ) -> Result<String, MarketplaceError> {
        super::normalize_relative_git_plugin_source_url(path, url)
    }
    /// Lists through the Store's existing manifest cache ownership.
    pub fn list_with_store_cache(
        roots: &[AbsolutePathBuf],
        home: Option<&Path>,
        store: &PluginStore,
    ) -> Result<MarketplaceListOutcome, MarketplaceError> {
        super::list_marketplaces_with_cache(roots, home, &store.manifest_cache)
    }
}
