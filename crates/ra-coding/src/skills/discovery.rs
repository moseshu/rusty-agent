// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Host skill discovery. Catalog selection never changes filesystem or execution permissions.

use std::{
    collections::{BTreeSet, HashMap, HashSet, VecDeque},
    fs, io,
    path::{Path, PathBuf},
};

use crate::plugins::{
    AbsolutePathBuf, discovery::DISCOVERABLE_PLUGIN_MANIFEST_PATHS, manifest::host_basename,
};
use ra_core::error::{Error, Result};
use ra_tools::skills_ns::{
    SkillProvider,
    catalog::{
        HostSkillPromptKind, SkillAuthority, SkillCatalog, SkillCatalogEntry, SkillPackageId,
        SkillResourceId, SkillScope, SkillSourceKind,
    },
    provider::{SkillListQuery, SkillProviderFuture, SkillReadRequest},
};
use tokio_util::sync::CancellationToken;

use super::{
    config::SkillConfigRules,
    metadata::{SkillMetadataFile, SkillProduct},
    parser::parse_skill_frontmatter_metadata,
};

const MAX_SCAN_DEPTH: usize = 6;
const MAX_SKILLS_DIRS_PER_ROOT: usize = 2_000;
const MAX_SKILLS_ENTRIES_PER_ROOT: usize = 20_000;

/// Whether a plugin root discovers every nested skill or only immediate child skills.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum SkillDiscoveryMode {
    /// Recursively discover skills using the upstream depth and traversal limits.
    #[default]
    Recursive,
    /// Discover only `root/<skill>/SKILL.md`, as in Codex Agent Plugins.
    DirectChildren,
}

/// A plugin skill root explicitly selected by the host, without installing or executing a plugin.
#[derive(Clone, Debug)]
pub struct PluginSkillRoot {
    path: PathBuf,
    plugin_root: PathBuf,
    namespace: String,
    mode: SkillDiscoveryMode,
}

impl PluginSkillRoot {
    /// Selects a skill directory, its owning plugin root and its authoritative namespace.
    /// Paths must be absolute. The namespace follows the owning plugin, as in Codex; it is not
    /// sanitized or replaced with the directory name. Qualified names are validated on load.
    pub fn new(
        path: impl Into<PathBuf>,
        plugin_root: impl Into<PathBuf>,
        namespace: impl Into<String>,
    ) -> Result<Self> {
        let (path, plugin_root) = (path.into(), plugin_root.into());
        if !path.is_absolute() || !plugin_root.is_absolute() {
            return Err(Error::config(
                "plugin skill and owner roots must be absolute",
            ));
        }
        Ok(Self {
            path,
            plugin_root,
            namespace: namespace.into(),
            mode: SkillDiscoveryMode::Recursive,
        })
    }

    /// Chooses recursive or direct-child discovery, independently of plugin authorization.
    #[must_use]
    pub const fn with_discovery_mode(mut self, mode: SkillDiscoveryMode) -> Self {
        self.mode = mode;
        self
    }
}

/// Explicit Host discovery configuration. Defaults to the project's `.rusty-agent/skills/`.
///
/// An allowlist is a context filter, not a sandbox. Omitted files remain on disk and can still
/// be read or executed with ordinary tools under their existing permissions. Selecting a plugin
/// root does not install, trust, execute, or authorize its scripts, MCP servers, or hooks.
#[derive(Clone, Debug)]
pub struct SkillDiscoveryConfig {
    project_skills: bool,
    plugins: Vec<PluginSkillRoot>,
    config_rules: SkillConfigRules,
    allowlist: Option<BTreeSet<String>>,
    restriction_product: Option<SkillProduct>,
}

impl Default for SkillDiscoveryConfig {
    fn default() -> Self {
        Self {
            project_skills: true,
            plugins: Vec::new(),
            config_rules: SkillConfigRules::default(),
            allowlist: None,
            restriction_product: None,
        }
    }
}

impl SkillDiscoveryConfig {
    /// Discovers project skills and accepts every discovered name unless an allowlist is set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Disables the project root, retaining explicitly selected plugin roots.
    #[must_use]
    pub const fn without_project_skills(mut self) -> Self {
        self.project_skills = false;
        self
    }

    /// Adds a plugin root after the project root. The first canonical skill path wins.
    #[must_use]
    pub fn with_plugin_root(mut self, root: PluginSkillRoot) -> Self {
        self.plugins.push(root);
        self
    }

    /// Applies ordered name/path enablement rules after discovery, qualification and path deduplication.
    /// Disabled skills remain in the catalog with `enabled = false` and are omitted from prompts.
    /// These rules only affect discovered Host entries, not Executor or Cloud providers.
    #[must_use]
    pub fn with_config_rules(mut self, rules: SkillConfigRules) -> Self {
        self.config_rules = rules;
        self
    }

    /// Selects complete, case-sensitive names after plugin namespace qualification and config rules.
    /// `None` by default permits all; an explicitly empty collection selects none. This filter
    /// only affects discovered Host catalog entries, not separately configured namespace providers.
    /// Allowlisted skills retain their rule-based enablement; the filter cannot reenable them.
    #[must_use]
    pub fn with_allowlist(mut self, names: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.allowlist = Some(names.into_iter().map(Into::into).collect());
        self
    }

    /// Selects an upstream product for skills with `policy.products` restrictions.
    /// With no product selected, only skills without product restrictions are included.
    #[must_use]
    pub const fn with_restriction_product(mut self, product: SkillProduct) -> Self {
        self.restriction_product = Some(product);
        self
    }

    /// Discovers metadata without retaining instruction bodies or changing any permission grants.
    /// Missing roots are empty; malformed skills and traversal errors are returned as warnings.
    pub async fn discover(&self, workspace_root: impl AsRef<Path>) -> Result<SkillCatalog> {
        let workspace_root = workspace_root.as_ref().to_path_buf();
        if !workspace_root.is_absolute() {
            return Err(Error::config("skill workspace root must be absolute"));
        }
        let config = self.clone();
        let cancelled = CancellationToken::new();
        let _cancel_on_drop = cancelled.clone().drop_guard();
        tokio::task::spawn_blocking(move || config.discover_sync(&workspace_root, &cancelled))
            .await
            .map_err(|error| Error::config(format!("skill discovery task failed: {error}")))
    }

    fn discover_sync(&self, workspace_root: &Path, cancelled: &CancellationToken) -> SkillCatalog {
        let mut roots = Vec::new();
        if self.project_skills {
            roots.push((workspace_root.join(".rusty-agent/skills"), None));
        }
        roots.extend(
            self.plugins
                .iter()
                .map(|plugin| (plugin.path.clone(), Some(plugin))),
        );
        let mut catalog = SkillCatalog::default();
        let mut loaded = Vec::new();
        let mut seen = HashSet::new();
        for (order, (root, plugin)) in roots.into_iter().enumerate() {
            if cancelled.is_cancelled() {
                break;
            }
            let root = fs::canonicalize(&root).unwrap_or(root);
            let mut skills = load_root(
                &root,
                plugin,
                order,
                self.restriction_product,
                &mut catalog.warnings,
                cancelled,
            );
            // Deduplicate before filtering: overlapping roots cannot change the first owner's name.
            skills.retain(|(path, _)| seen.insert(path.clone()));
            loaded.extend(skills);
        }
        loaded.sort_by(|(left_path, left), (right_path, right)| {
            scope_rank(left)
                .cmp(&scope_rank(right))
                .then_with(|| left.name.cmp(&right.name))
                .then_with(|| left_path.cmp(right_path))
        });
        let disabled_paths = self.config_rules.resolve_disabled_paths(
            loaded
                .iter()
                .map(|(path, entry)| (entry.name.as_str(), path)),
        );
        catalog.entries = loaded
            .into_iter()
            .map(|(path, entry)| {
                if disabled_paths.contains(&path) {
                    entry.disabled()
                } else {
                    entry
                }
            })
            .collect();
        if let Some(allowlist) = &self.allowlist {
            catalog
                .entries
                .retain(|entry| allowlist.contains(&entry.name));
        }
        catalog
    }
}

fn scope_rank(entry: &SkillCatalogEntry) -> u8 {
    // Discovery merges repository skills before user/plugin skills. Prompt ordering is separate.
    u8::from(entry.prompt_scope() != Some(SkillScope::Repo))
}

fn load_root(
    root: &Path,
    plugin: Option<&PluginSkillRoot>,
    order: usize,
    restriction_product: Option<SkillProduct>,
    warnings: &mut Vec<String>,
    cancelled: &CancellationToken,
) -> Vec<(PathBuf, SkillCatalogEntry)> {
    let mode = plugin.map_or(SkillDiscoveryMode::Recursive, |plugin| plugin.mode);
    let plugin_root = plugin.map(|plugin| {
        fs::canonicalize(&plugin.plugin_root).unwrap_or_else(|_| plugin.plugin_root.clone())
    });
    let mut namespace_cache = HashMap::new();
    let mut entries = Vec::new();
    for discovered in discover_files(root, mode, warnings, cancelled) {
        if cancelled.is_cancelled() {
            break;
        }
        let path = match fs::canonicalize(&discovered) {
            Ok(path) => path,
            Err(error) if mode == SkillDiscoveryMode::DirectChildren => {
                warnings.push(format!(
                    "failed to resolve Agent Plugin skill path {}: {error}",
                    discovered.display()
                ));
                continue;
            }
            Err(_) => discovered.clone(),
        };
        if mode == SkillDiscoveryMode::DirectChildren
            && plugin_root
                .as_ref()
                .is_none_or(|owner| !path.starts_with(owner))
        {
            warnings.push(format!(
                "Agent Plugin skill path {} resolves outside plugin root",
                path.display()
            ));
            continue;
        }
        let parsed = fs::read_to_string(&path)
            .map_err(|error| format!("failed to read file: {error}"))
            .and_then(|contents| {
                parse_skill_frontmatter_metadata(&contents, || default_name(&path))
                    .map_err(|error| error.to_string())
            });
        let parsed = match parsed {
            Ok(parsed) => parsed,
            Err(error) => {
                warnings.push(format!(
                    "Failed to load host skill at {}: {error}",
                    path.display()
                ));
                continue;
            }
        };
        let namespace = plugin
            .map(|plugin| plugin.namespace.clone())
            .or_else(|| nearest_namespace(&path, &mut namespace_cache));
        let name = namespace.map_or_else(
            || parsed.name.clone(),
            |namespace| format!("{namespace}:{}", parsed.name),
        );
        if name.is_empty() || name.chars().count() > 129 {
            warnings.push(format!("Failed to load host skill at {}: invalid qualified name: exceeds maximum length of 129 characters", path.display()));
            continue;
        }
        let policy = optional_policy(&path);
        if policy
            .as_ref()
            .is_some_and(|policy| !policy.matches_product(restriction_product))
        {
            continue;
        }
        let display = path.to_string_lossy().to_string();
        let mut entry = SkillCatalogEntry::new(
            SkillPackageId(display.clone()),
            SkillAuthority::new(SkillSourceKind::Host, "host"),
            name,
            parsed.description,
            SkillResourceId::new(display.clone()),
        )
        .with_host_prompt_kind(if mode == SkillDiscoveryMode::DirectChildren {
            HostSkillPromptKind::AgentPlugin
        } else {
            HostSkillPromptKind::Core
        })
        .with_display_path(discovered.to_string_lossy().replace('\\', "/"))
        .with_short_description(parsed.short_description)
        .with_prompt_scope(if plugin.is_some() {
            SkillScope::User
        } else {
            SkillScope::Repo
        })
        .with_alias_root(root.to_string_lossy().replace('\\', "/"))
        .with_alias_root_order(order);
        if policy
            .as_ref()
            .and_then(|policy| policy.allow_implicit_invocation)
            == Some(false)
        {
            entry = entry.hidden_from_prompt();
        }
        entries.push((path, entry));
    }
    entries
}

fn optional_policy(path: &Path) -> Option<super::metadata::SkillPolicy> {
    let metadata_path = path.parent()?.join("agents/openai.yaml");
    let contents = match fs::read_to_string(&metadata_path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return None,
        Err(error) => {
            tracing::warn!(path = %metadata_path.display(), "ignoring optional skill metadata: {error}");
            return None;
        }
    };
    match serde_yaml::from_str::<SkillMetadataFile>(&contents) {
        Ok(parsed) => parsed.policy,
        Err(error) => {
            tracing::warn!(path = %metadata_path.display(), "ignoring invalid optional skill metadata: {error}");
            None
        }
    }
}

fn default_name(path: &Path) -> String {
    path.parent()
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .map(|name| name.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "skill".into())
}

fn nearest_namespace(path: &Path, cache: &mut HashMap<PathBuf, Option<String>>) -> Option<String> {
    for ancestor in path.parent()?.ancestors() {
        let namespace = cache
            .entry(ancestor.to_path_buf())
            .or_insert_with(|| namespace_at(ancestor));
        if namespace.is_some() {
            return namespace.clone();
        }
    }
    None
}

fn namespace_at(root: &Path) -> Option<String> {
    let path = DISCOVERABLE_PLUGIN_MANIFEST_PATHS
        .iter()
        .map(|path| root.join(path))
        .find(|path| path.is_file())?;
    let parsed: serde_json::Value = serde_json::from_str(&fs::read_to_string(path).ok()?).ok()?;
    // As upstream, missing or blank names fall back to the plugin directory; invalid types fail.
    let name = match parsed.get("name") {
        None => "",
        Some(value) => value.as_str()?,
    };
    Some(if name.trim().is_empty() {
        AbsolutePathBuf::try_from(root)
            .ok()
            .and_then(|root| host_basename(&root))
            .unwrap_or_else(|| name.into())
    } else {
        name.into()
    })
}

fn read_directory(path: &Path, cancelled: &CancellationToken) -> io::Result<Vec<fs::DirEntry>> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(path)? {
        if cancelled.is_cancelled() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "skill discovery cancelled",
            ));
        }
        entries.push(entry?);
    }
    entries.sort_by_key(fs::DirEntry::file_name);
    Ok(entries)
}

fn is_scan_root(root: &Path, warnings: &mut Vec<String>) -> bool {
    match fs::metadata(root) {
        Ok(metadata) => metadata.is_dir(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => {
            warnings.push(format!(
                "failed to walk skills root {}: {error}",
                root.display()
            ));
            false
        }
    }
}

fn discover_files(
    root: &Path,
    mode: SkillDiscoveryMode,
    warnings: &mut Vec<String>,
    cancelled: &CancellationToken,
) -> Vec<PathBuf> {
    if !is_scan_root(root, warnings) {
        return Vec::new();
    }
    let max_depth = if mode == SkillDiscoveryMode::Recursive {
        MAX_SCAN_DEPTH
    } else {
        2
    };
    let mut queue = VecDeque::from([(root.to_path_buf(), 0usize)]);
    let mut visited = HashSet::from([root.to_path_buf()]);
    let (mut directories, mut entry_count) = (1usize, 0usize);
    let mut skills = Vec::new();
    let mut truncated = false;
    while let Some((directory, depth)) = queue.pop_front() {
        if cancelled.is_cancelled() {
            break;
        }
        let children = match read_directory(&directory, cancelled) {
            Ok(entries) => entries,
            Err(error) => {
                warnings.push(format!(
                    "failed to scan skill path {}: {error}",
                    directory.display()
                ));
                continue;
            }
        };
        for child in children {
            if cancelled.is_cancelled() {
                return skills;
            }
            if entry_count == MAX_SKILLS_ENTRIES_PER_ROOT {
                truncated = true;
                break;
            }
            entry_count += 1;
            let path = child.path();
            let metadata = match fs::metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) => {
                    warnings.push(format!(
                        "failed to scan skill path {}: {error}",
                        path.display()
                    ));
                    continue;
                }
            };
            let is_symlink = child.file_type().is_ok_and(|kind| kind.is_symlink());
            if is_symlink && !metadata.is_dir() {
                continue;
            }
            if metadata.is_file()
                && child.file_name() == "SKILL.md"
                && (mode == SkillDiscoveryMode::Recursive
                    || path.parent().and_then(Path::parent) == Some(root))
            {
                skills.push(path);
            } else if metadata.is_dir()
                && depth < max_depth
                && !child.file_name().to_string_lossy().starts_with('.')
            {
                let identity = match fs::canonicalize(&path) {
                    Ok(identity) => identity,
                    Err(error) => {
                        warnings.push(format!(
                            "failed to scan skill path {}: {error}",
                            path.display()
                        ));
                        continue;
                    }
                };
                if !visited.insert(identity) {
                    continue;
                }
                if directories == MAX_SKILLS_DIRS_PER_ROOT {
                    truncated = true;
                } else {
                    directories += 1;
                    queue.push_back((path, depth + 1));
                }
            }
        }
        if entry_count == MAX_SKILLS_ENTRIES_PER_ROOT && truncated {
            break;
        }
    }
    if truncated {
        warnings.push(format!(
            "skills scan reached its traversal limit (root: {})",
            root.display()
        ));
    }
    skills
}

pub(crate) struct HostSkillProvider {
    pub(crate) config: SkillDiscoveryConfig,
    pub(crate) workspace_root: PathBuf,
}

impl SkillProvider for HostSkillProvider {
    fn list(&self, _: SkillListQuery) -> SkillProviderFuture<'_, SkillCatalog> {
        Box::pin(async move {
            self.config
                .discover(&self.workspace_root)
                .await
                .map_err(|error| {
                    ra_tools::skills_ns::catalog::SkillProviderError::new(error.to_string())
                })
        })
    }
    fn read<'a>(
        &'a self,
        request: SkillReadRequest<'a>,
    ) -> SkillProviderFuture<'a, ra_tools::skills_ns::catalog::SkillReadResult> {
        Box::pin(async move {
            use ra_tools::skills_ns::{
                catalog::{SkillProviderError, SkillReadResult},
                provider::SkillReadContext,
            };
            let SkillReadContext::Host {
                snapshot: Some(snapshot),
            } = request.context
            else {
                return Err(SkillProviderError::new(
                    "host skill provider requires a host skills snapshot",
                ));
            };
            let Some(resource_path) = snapshot.resource_path(request.resource.as_str()) else {
                return Err(SkillProviderError::new(format!(
                    "host skill resource is not loaded: {}",
                    request.resource.as_str()
                )));
            };
            // The run catalog supplies the discovered locator. Selection grants no filesystem
            // permissions; Host reads retain the host's ambient access, as in Codex's snapshot.
            let contents = tokio::fs::read_to_string(resource_path)
                .await
                .map_err(|error| {
                    SkillProviderError::new(format!(
                        "failed to read host skill resource {}: {error}",
                        request.resource.as_str()
                    ))
                })?;
            Ok(SkillReadResult {
                resource: request.resource,
                contents,
            })
        })
    }
}
