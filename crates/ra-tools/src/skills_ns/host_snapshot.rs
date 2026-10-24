// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Immutable membership for the core Host resources loaded by one run.

use super::{
    catalog::{SkillCatalog, SkillSourceKind},
    host_selection::native_path,
};
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

/// The local Host loader's immutable resource identities, separate from extension catalogs.
/// This is the resource membership portion of Codex's `HostSkillsSnapshot`; metadata remains in the
/// run catalog and bodies stay lazy. An external Host entry does not become a core loader resource
/// unless its adapter explicitly selects the core Host prompt lifecycle.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HostSkillsSnapshot {
    resources: BTreeSet<PathBuf>,
}
impl HostSkillsSnapshot {
    /// Captures only entries produced by core-compatible Host discovery, including disabled entries.
    #[must_use]
    pub fn from_catalog(catalog: &SkillCatalog) -> Self {
        Self {
            resources: catalog
                .entries
                .iter()
                .filter(|entry| {
                    entry.authority.kind == SkillSourceKind::Host
                        && entry.host_prompt_kind.is_some()
                })
                .filter_map(|entry| native_path(Path::new(entry.main_prompt.as_str())))
                .collect(),
        }
    }
    /// Whether this resource belongs to the loaded core Host snapshot.
    #[must_use]
    pub fn contains(&self, resource: &str) -> bool {
        self.resource_path(resource).is_some()
    }

    /// Resolves to the stored loader path, so reading never reinterprets the caller's spelling.
    #[must_use]
    pub fn resource_path(&self, resource: &str) -> Option<&Path> {
        self.resources
            .get(&native_path(Path::new(resource))?)
            .map(PathBuf::as_path)
    }

    /// Reads the stored native Host path, retaining the underlying I/O error for Core warnings.
    ///
    /// # Errors
    /// Returns an I/O error if the resource was not loaded or its contents cannot be read.
    pub async fn read_skill_text(&self, resource: &str) -> std::io::Result<String> {
        let path = self.resource_path(resource).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("host skill resource is not loaded: {resource}"),
            )
        })?;
        tokio::fs::read_to_string(path).await
    }
}
