// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Source identities and catalog records for authority-routed skill access.

use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Source authority that owns a skill package and must be used to read it.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum SkillSourceKind {
    /// Host-owned skills, including bundled, user, repo, plugin-installed,
    /// and downloaded/materialized remote skills.
    Host,
    /// Skills owned by an execution environment.
    Executor,
    /// Skills owned by the cloud rather than an execution environment.
    Cloud,
    /// Extension-private source kind for future providers that do not fit an
    /// existing transport category.
    Custom(String),
}

impl SkillSourceKind {
    /// Creates an extension-private source kind.
    #[must_use]
    pub fn custom(kind: impl Into<String>) -> Self {
        Self::Custom(kind.into())
    }

    fn as_str(&self) -> &str {
        match self {
            Self::Host => "host",
            Self::Executor => "executor",
            Self::Cloud => "cloud",
            Self::Custom(kind) => kind,
        }
    }
}

impl std::fmt::Display for SkillSourceKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.as_str().fmt(formatter)
    }
}

/// Opaque authority identity for list/read routing.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SkillAuthority {
    /// Transport category used for provider routing.
    pub kind: SkillSourceKind,
    /// Opaque identifier owned by the source.
    pub id: String,
}

impl SkillAuthority {
    /// Creates an opaque authority under one source kind.
    #[must_use]
    pub fn new(kind: SkillSourceKind, id: impl Into<String>) -> Self {
        Self {
            kind,
            id: id.into(),
        }
    }
}

/// Opaque package id. Callers should not parse local paths out of this value.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SkillPackageId(
    /// Opaque package handle.
    pub String,
);

impl SkillPackageId {
    /// Returns a nonempty package-relative suffix without empty, dot or parent segments.
    #[must_use]
    pub fn relative_resource_path<'a>(&self, resource: &'a str) -> Option<&'a str> {
        let relative = resource
            .strip_prefix(self.0.trim_end_matches('/'))?
            .strip_prefix('/')?;
        (!relative.is_empty()
            && relative
                .split('/')
                .all(|segment| !matches!(segment, "" | "." | "..")))
        .then_some(relative)
    }
}

/// Opaque resource id inside a skill package, optionally bound to the
/// environment path that owns its contents.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SkillResourceId {
    id: String,
    environment_path: Option<EnvironmentSkillResource>,
}

impl SkillResourceId {
    /// Creates an unbound opaque resource identifier.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            environment_path: None,
        }
    }

    /// Binds a main resource to an absolute executor path; its parent is the package root.
    #[must_use]
    pub fn environment(
        id: impl Into<String>,
        environment_id: impl Into<String>,
        path: PathBuf,
    ) -> Self {
        let package_root = path.parent().unwrap_or(&path).to_path_buf();
        Self {
            id: id.into(),
            environment_path: Some(EnvironmentSkillResource {
                environment_id: environment_id.into(),
                package_root,
                path,
                contents: None,
            }),
        }
    }

    /// Binds an absolute executor path with already-read contents.
    #[must_use]
    pub fn environment_with_contents(
        id: impl Into<String>,
        environment_id: impl Into<String>,
        path: PathBuf,
        contents: String,
    ) -> Self {
        let package_root = path.parent().unwrap_or(&path).to_path_buf();
        Self {
            id: id.into(),
            environment_path: Some(EnvironmentSkillResource {
                environment_id: environment_id.into(),
                package_root,
                path,
                contents: Some(contents.into()),
            }),
        }
    }

    /// The opaque resource identifier.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.id
    }

    /// Binds another resource within this resource's package root.
    #[must_use]
    pub fn bind_environment_package_resource(
        &self,
        package: &SkillPackageId,
        resource: impl Into<String>,
    ) -> Option<Self> {
        let resource = resource.into();
        let relative = package.relative_resource_path(&resource)?;
        let environment = self.environment_path.as_ref()?;
        if std::path::Path::new(relative)
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
        {
            return None;
        }
        let path = environment.package_root.join(relative);
        path.starts_with(&environment.package_root).then(|| Self {
            id: resource,
            environment_path: Some(EnvironmentSkillResource {
                environment_id: environment.environment_id.clone(),
                package_root: environment.package_root.clone(),
                path,
                contents: None,
            }),
        })
    }

    /// The environment identity and native bound path, when present.
    #[must_use]
    pub fn environment_path(&self) -> Option<(&str, &Path)> {
        self.environment_path
            .as_ref()
            .map(|resource| (resource.environment_id.as_str(), resource.path.as_path()))
    }

    /// The contents retained during discovery, when present.
    #[must_use]
    pub fn environment_contents(&self) -> Option<&str> {
        self.environment_path
            .as_ref()
            .and_then(|resource| resource.contents.as_deref())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct EnvironmentSkillResource {
    environment_id: String,
    package_root: PathBuf,
    path: PathBuf,
    contents: Option<Arc<str>>,
}

/// Origin precedence used by Codex Host catalogs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SkillScope {
    /// Repository skills.
    Repo,
    /// User-installed skills, including plugins.
    User,
    /// Bundled system skills.
    System,
    /// Administrator skills.
    Admin,
}

/// Metadata shown in the always-visible skills catalog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkillCatalogEntry {
    /// Opaque identifier owned by the source.
    pub id: SkillPackageId,
    /// Authority that must own resource reads.
    pub authority: SkillAuthority,
    /// Model-visible skill name.
    pub name: String,
    /// Model-visible skill purpose.
    pub description: String,
    /// Optional concise purpose preferred by the namespace catalog prompt.
    pub short_description: Option<String>,
    /// Main instruction resource, usually SKILL.md.
    pub main_prompt: SkillResourceId,
    /// Optional presentation path; not a grant.
    pub display_path: Option<String>,
    alias_root: Option<String>,
    alias_root_order: Option<usize>,
    prompt_scope: Option<SkillScope>,
    /// Whether the skill may be listed or read.
    pub enabled: bool,
    /// Whether enabled metadata appears in model-visible discovery.
    pub prompt_visible: bool,
}

impl SkillCatalogEntry {
    /// Creates an enabled, prompt-visible catalog entry.
    #[must_use]
    pub fn new(
        id: SkillPackageId,
        authority: SkillAuthority,
        name: impl Into<String>,
        description: impl Into<String>,
        main_prompt: SkillResourceId,
    ) -> Self {
        Self {
            id,
            authority,
            name: name.into(),
            description: description.into(),
            short_description: None,
            main_prompt,
            display_path: None,
            alias_root: None,
            alias_root_order: None,
            prompt_scope: None,
            enabled: true,
            prompt_visible: true,
        }
    }

    /// Sets the optional presentation locator without changing read routing.
    #[must_use]
    pub fn with_display_path(mut self, display_path: impl Into<String>) -> Self {
        self.display_path = Some(display_path.into());
        self
    }

    /// Sets the concise purpose used by extension-compatible catalog prompts.
    #[must_use]
    pub fn with_short_description(mut self, short_description: Option<String>) -> Self {
        self.short_description = short_description;
        self
    }

    /// Sets the shared locator prefix that may be compacted in model-visible skill catalogs.
    #[must_use]
    pub fn with_alias_root(mut self, alias_root: impl Into<String>) -> Self {
        self.alias_root = Some(alias_root.into());
        self
    }

    /// Sets stable alias ordering supplied by the host catalog.
    #[must_use]
    pub fn with_alias_root_order(mut self, alias_root_order: usize) -> Self {
        self.alias_root_order = Some(alias_root_order);
        self
    }

    /// Sets the Host scope used for catalog ordering.
    #[must_use]
    pub fn with_prompt_scope(mut self, scope: SkillScope) -> Self {
        self.prompt_scope = Some(scope);
        self
    }

    /// The Host scope selected for catalog ordering, if supplied.
    #[must_use]
    pub fn prompt_scope(&self) -> Option<SkillScope> {
        self.prompt_scope
    }

    /// Marks the entry unavailable for listing and reading.
    #[must_use]
    pub fn disabled(mut self) -> Self {
        self.enabled = false;
        self
    }

    /// Hides the entry from listing while retaining direct package reads.
    #[must_use]
    pub fn hidden_from_prompt(mut self) -> Self {
        self.prompt_visible = false;
        self
    }

    pub(crate) fn is_model_visible(&self) -> bool {
        self.enabled && self.prompt_visible
    }

    pub(crate) fn rendered_path(&self) -> &str {
        self.display_path
            .as_deref()
            .unwrap_or_else(|| self.main_prompt.as_str())
    }

    pub(crate) fn alias_root(&self) -> Option<&str> {
        self.alias_root.as_deref()
    }

    pub(crate) fn alias_root_order(&self) -> Option<usize> {
        self.alias_root_order
    }
}

/// Merged catalog for one turn.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SkillCatalog {
    /// Ordered catalog entries.
    pub entries: Vec<SkillCatalogEntry>,
    /// Discovery warnings in provider order.
    pub warnings: Vec<String>,
}

impl SkillCatalog {
    /// Merges entries in order, keeping the first authority/package pair and every warning.
    pub fn extend(&mut self, other: SkillCatalog) {
        for entry in other.entries {
            self.push_entry(entry);
        }
        self.warnings.extend(other.warnings);
    }

    /// Appends an entry unless its authority/package pair is already present.
    pub fn push_entry(&mut self, entry: SkillCatalogEntry) {
        if self
            .entries
            .iter()
            .any(|existing| existing.authority == entry.authority && existing.id == entry.id)
        {
            return;
        }

        self.entries.push(entry);
    }
}

/// Contents returned after resolving a skill resource through its owner.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkillReadResult {
    /// The exact resource identifier and binding returned by the provider.
    pub resource: SkillResourceId,
    /// Complete unpaginated UTF-8 resource contents.
    pub contents: String,
}

/// A failure reported by the provider that owns a skill resource.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkillProviderError {
    /// Provider diagnostic, logged without being exposed by skills.read.
    pub message: String,
}

impl SkillProviderError {
    /// Creates a provider error with the source's diagnostic message.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for SkillProviderError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.message.fmt(formatter)
    }
}

impl std::error::Error for SkillProviderError {}

/// Result of an authority-routed provider operation.
pub type SkillProviderResult<T> = Result<T, SkillProviderError>;
