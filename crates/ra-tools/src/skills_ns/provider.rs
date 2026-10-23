// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Provider routing and bounded reads through an explicitly granted local filesystem.

use std::{
    collections::BTreeMap,
    future::Future,
    io::Read,
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
};

use ra_exec::fs::{RootedFileSystem, Workspace};

use super::catalog::{
    SkillAuthority, SkillCatalog, SkillPackageId, SkillProviderError, SkillProviderResult,
    SkillReadResult, SkillResourceId, SkillSourceKind,
};

/// Maximum size of a streamed executor resource and of a cached read snapshot.
pub const MAX_SKILL_RESOURCE_CONTENT_BYTES: usize = 1024 * 1024;
/// The local coding executor's environment identity.
pub const LOCAL_ENVIRONMENT_ID: &str = "local";
/// Authority identity required for cloud entries in the Codex skills tool contract.
pub const CLOUD_AUTHORITY_ID: &str = "codex_apps";

/// Per-call list context and explicitly selected local executor roots.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct SkillListQuery {
    /// Run identity supplied by the callback; no framework thread storage is required.
    pub turn_id: String,
    /// Selected authority ids and their granted local paths. Empty for Host and Cloud queries.
    /// This is the local adaptation of the upstream selected/resolved executor roots.
    pub executor_roots: BTreeMap<String, PathBuf>,
}

/// A resource read with its authority and callback access.
#[derive(Clone)]
pub struct SkillReadRequest<'a> {
    /// Source authority that owns the package.
    pub authority: SkillAuthority,
    /// Opaque package identifier.
    pub package: SkillPackageId,
    /// Resource and optional executor path binding.
    pub resource: SkillResourceId,
    /// Access selected for this callback.
    pub context: SkillReadContext<'a>,
}

/// Executor access is a host-selected directory capability; cloud providers own their transport.
#[non_exhaustive]
#[derive(Clone)]
pub enum SkillReadContext<'a> {
    /// Read within this root, which is the host's explicit grant.
    Executor {
        /// The granted workspace capability.
        fs: &'a Workspace,
    },
    /// A host-supplied cloud adapter. No `OpenAI` account backend is installed.
    Cloud,
}

/// Sendable asynchronous provider result.
pub type SkillProviderFuture<'a, T> =
    Pin<Box<dyn Future<Output = SkillProviderResult<T>> + Send + 'a>>;

/// Source-specific catalog and resource access.
///
/// Implementations must preserve authority boundaries and return exactly the requested resource.
/// This port consumes `list` and `read`; the upstream unused search API is deferred.
pub trait SkillProvider: Send + Sync {
    /// Lists the current catalog, preserving provider order and warnings.
    fn list(&self, query: SkillListQuery) -> SkillProviderFuture<'_, SkillCatalog>;

    /// Reads through the callback's access, or reports a provider failure.
    fn read<'a>(
        &'a self,
        request: SkillReadRequest<'a>,
    ) -> SkillProviderFuture<'a, SkillReadResult>;
}

/// A named provider registered for one source kind.
#[derive(Clone)]
pub struct SkillProviderSource {
    kind: SkillSourceKind,
    label: String,
    provider: Arc<dyn SkillProvider>,
}

impl std::fmt::Debug for SkillProviderSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SkillProviderSource")
            .field("kind", &self.kind)
            .field("label", &self.label)
            .finish_non_exhaustive()
    }
}

impl SkillProviderSource {
    /// Registers a provider under its source kind and diagnostic label.
    pub fn new(
        kind: SkillSourceKind,
        label: impl Into<String>,
        provider: Arc<dyn SkillProvider>,
    ) -> Self {
        Self {
            kind,
            label: label.into(),
            provider,
        }
    }
}

/// Ordered providers, routed by kind with first-success read fallback.
#[derive(Clone, Default, Debug)]
pub struct SkillProviders {
    sources: Vec<SkillProviderSource>,
}

impl SkillProviders {
    /// Creates an empty registry; an unconfigured authority lists an empty catalog.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends a provider without changing existing precedence.
    #[must_use]
    pub fn with_provider(mut self, source: SkillProviderSource) -> Self {
        self.sources.push(source);
        self
    }

    /// Appends an executor provider.
    #[must_use]
    pub fn with_executor_provider(self, provider: Arc<dyn SkillProvider>) -> Self {
        self.with_provider(SkillProviderSource::new(
            SkillSourceKind::Executor,
            "executor",
            provider,
        ))
    }

    /// Appends a host-supplied cloud provider without installing an account backend.
    /// Its catalog entries must use [`CLOUD_AUTHORITY_ID`] as their authority id to be exposed
    /// through `skills.list` and `skills.read`, preserving the upstream authority contract.
    #[must_use]
    pub fn with_cloud_provider(self, provider: Arc<dyn SkillProvider>) -> Self {
        self.with_provider(SkillProviderSource::new(
            SkillSourceKind::Cloud,
            "cloud",
            provider,
        ))
    }

    /// Adds a Host catalog source. Host entries contribute paths to the prompt and are read
    /// with ordinary filesystem tools; the namespace does not serve Host resources.
    #[must_use]
    pub fn with_host_provider(self, provider: Arc<dyn SkillProvider>) -> Self {
        self.with_provider(SkillProviderSource::new(
            SkillSourceKind::Host,
            "host",
            provider,
        ))
    }

    /// Whether Host catalog discovery is configured.
    #[must_use]
    pub fn has_host_provider(&self) -> bool {
        self.sources
            .iter()
            .any(|source| source.kind == SkillSourceKind::Host)
    }

    /// Whether an executor source is configured, even if its catalog is currently empty.
    #[must_use]
    pub fn has_executor_provider(&self) -> bool {
        self.sources
            .iter()
            .any(|source| source.kind == SkillSourceKind::Executor)
    }

    /// Whether a cloud source is configured, even if its catalog is currently empty.
    #[must_use]
    pub fn has_cloud_provider(&self) -> bool {
        self.sources
            .iter()
            .any(|source| source.kind == SkillSourceKind::Cloud)
    }

    pub(crate) fn host_catalog_sources(mut self) -> Self {
        self.sources
            .retain(|source| source.kind == SkillSourceKind::Host);
        self
    }

    pub(super) async fn list(&self, kind: &SkillSourceKind, query: SkillListQuery) -> SkillCatalog {
        let mut catalog = SkillCatalog::default();
        for source in self.sources.iter().filter(|source| &source.kind == kind) {
            match source.provider.list(query.clone()).await {
                Ok(other) => catalog.extend(other),
                Err(err) => catalog.warnings.push(format!(
                    "{} skills unavailable: {}",
                    source.label, err.message
                )),
            }
        }
        if *kind == SkillSourceKind::Executor {
            catalog.entries.retain(|entry| {
                entry.authority.kind == SkillSourceKind::Executor
                    && query.executor_roots.contains_key(&entry.authority.id)
            });
        }
        catalog
    }

    /// Reads from matching providers in registration order, retaining the last failure.
    pub async fn read(
        &self,
        request: SkillReadRequest<'_>,
    ) -> SkillProviderResult<SkillReadResult> {
        let mut last_error = None;
        for source in self
            .sources
            .iter()
            .filter(|source| source.kind == request.authority.kind)
        {
            match source.provider.read(request.clone()).await {
                Ok(result) => return Ok(result),
                Err(err) => last_error = Some(err),
            }
        }
        Err(last_error.unwrap_or_else(|| {
            SkillProviderError::new(format!(
                "{} skill provider is not configured",
                request.authority.kind
            ))
        }))
    }
}

/// Reads executor resources from an explicitly supplied catalog; automatic discovery is separate.
#[derive(Clone, Debug)]
pub struct ExecutorSkillProvider {
    catalog: SkillCatalog,
}

impl ExecutorSkillProvider {
    /// Uses host-supplied metadata without discovering or granting any filesystem paths.
    #[must_use]
    pub const fn new(catalog: SkillCatalog) -> Self {
        Self { catalog }
    }
}

impl SkillProvider for ExecutorSkillProvider {
    fn list(&self, query: SkillListQuery) -> SkillProviderFuture<'_, SkillCatalog> {
        Box::pin(async move {
            let mut catalog = self.catalog.clone();
            catalog
                .entries
                .retain(|entry| query.executor_roots.contains_key(&entry.authority.id));
            Ok(catalog)
        })
    }

    fn read<'a>(
        &'a self,
        request: SkillReadRequest<'a>,
    ) -> SkillProviderFuture<'a, SkillReadResult> {
        Box::pin(async move {
            if request.authority.kind != SkillSourceKind::Executor {
                return Err(SkillProviderError::new(format!(
                    "executor skill provider cannot read {} resources",
                    request.authority.kind
                )));
            }
            let expected_package_prefix = format!("skill://{}/", request.authority.id);
            if !request.package.0.starts_with(&expected_package_prefix)
                || request
                    .package
                    .relative_resource_path(request.resource.as_str())
                    .is_none()
            {
                return Err(SkillProviderError::new(
                    "executor skill resource does not match its package",
                ));
            }
            let SkillReadContext::Executor { fs } = request.context else {
                return Err(SkillProviderError::new(
                    "executor skill reads require filesystem access",
                ));
            };
            if let Some(contents) = request.resource.environment_contents() {
                return Ok(SkillReadResult {
                    resource: request.resource.clone(),
                    contents: contents.to_string(),
                });
            }
            let Some((_, resource_path)) = request.resource.environment_path() else {
                return Err(SkillProviderError::new(
                    "executor skill resource is not bound to an environment",
                ));
            };
            // Environment paths are absolute, as upstream PathUri requires. Keep native path
            // components through the capability open instead of resolving a lossy string.
            let relative = resource_path
                .strip_prefix(fs.root())
                .map_err(|err| {
                    SkillProviderError::new(format!(
                        "failed to read executor skill resource {}: {err}",
                        request.resource.as_str()
                    ))
                })?
                .to_path_buf();
            let filesystem = Arc::clone(fs.filesystem());
            let resource = request.resource.clone();
            let contents = tokio::task::spawn_blocking(move || {
                read_bounded_text(&filesystem, &relative, resource.as_str())
            })
            .await
            .map_err(|err| SkillProviderError::new(err.to_string()))??;
            Ok(SkillReadResult {
                resource: request.resource,
                contents,
            })
        })
    }
}

fn read_bounded_text(
    filesystem: &RootedFileSystem,
    path: &Path,
    resource: &str,
) -> SkillProviderResult<String> {
    let read_error = |error: String| {
        SkillProviderError::new(format!(
            "failed to read executor skill resource {resource}: {error}"
        ))
    };
    let file = filesystem
        .open_read(path)
        .map_err(|err| read_error(err.to_string()))?;
    let mut contents = Vec::new();
    // Read at most one sentinel byte beyond the limit, never an unbounded resource.
    file.take((MAX_SKILL_RESOURCE_CONTENT_BYTES + 1) as u64)
        .read_to_end(&mut contents)
        .map_err(|err| read_error(err.to_string()))?;
    if contents.len() > MAX_SKILL_RESOURCE_CONTENT_BYTES {
        return Err(SkillProviderError::new(format!(
            "executor skill resource {resource} exceeds {MAX_SKILL_RESOURCE_CONTENT_BYTES} bytes"
        )));
    }
    String::from_utf8(contents).map_err(|_| {
        SkillProviderError::new(format!(
            "executor skill resource {resource} is not valid UTF-8"
        ))
    })
}
