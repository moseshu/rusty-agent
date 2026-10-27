// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Authority-bound plugin descriptors and resource locations.

#![allow(
    clippy::result_large_err,
    reason = "Preserve the upstream error fields and public result type."
)]

use super::model::PluginManifest;
use ra_paths::PathUri;
use thiserror::Error;

/// A plugin resource paired with the environment that owns its filesystem.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PluginResourceLocator {
    /// A resource owned by the named execution environment.
    Environment {
        /// Environment whose filesystem owns the resource.
        environment_id: String,
        /// Resource URI within that filesystem.
        path: PathUri,
    },
}

/// Authority-bound location of a resolved plugin package.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResolvedPluginLocation {
    /// A package owned by the named execution environment.
    Environment {
        /// Environment whose filesystem owns the package.
        environment_id: String,
        /// Package root URI within that filesystem.
        root: PathUri,
    },
}

/// An inert plugin descriptor whose resources retain their source authority.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedPlugin {
    selected_root_id: String,
    location: ResolvedPluginLocation,
    manifest_path: PluginResourceLocator,
    manifest: PluginManifest<PluginResourceLocator>,
}

/// Failure to construct a resolved plugin with internally consistent resources.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum ResolvedPluginError {
    /// A manifest or declared resource lies outside its package root.
    #[error("plugin resource path `{path}` is outside package root `{root}`")]
    ResourceOutsideRoot {
        /// The package root used to bind all resources.
        root: PathUri,
        /// The resource that failed the containment check.
        path: PathUri,
    },
}

impl ResolvedPlugin {
    /// Creates an environment-owned descriptor from a validated plugin manifest.
    pub fn from_environment(
        selected_root_id: String,
        environment_id: String,
        root: PathUri,
        manifest_path: PathUri,
        manifest: PluginManifest<PathUri>,
    ) -> Result<Self, ResolvedPluginError> {
        let manifest_path = environment_resource(&environment_id, &root, manifest_path)?;
        let manifest = manifest
            .try_map_resources(|path| environment_resource(&environment_id, &root, path))?;
        Ok(Self {
            selected_root_id,
            location: ResolvedPluginLocation::Environment {
                environment_id,
                root,
            },
            manifest_path,
            manifest,
        })
    }

    /// Returns the opaque ID supplied for the selected capability root.
    #[must_use]
    pub fn selected_root_id(&self) -> &str {
        &self.selected_root_id
    }

    /// Returns the authority-bound package location.
    #[must_use]
    pub fn location(&self) -> &ResolvedPluginLocation {
        &self.location
    }

    /// Returns the manifest resource used to resolve this package.
    #[must_use]
    pub fn manifest_path(&self) -> &PluginResourceLocator {
        &self.manifest_path
    }

    /// Returns package metadata whose resource fields retain their source authority.
    #[must_use]
    pub fn manifest(&self) -> &PluginManifest<PluginResourceLocator> {
        &self.manifest
    }
}

fn environment_resource(
    environment_id: &str,
    root: &PathUri,
    path: PathUri,
) -> Result<PluginResourceLocator, ResolvedPluginError> {
    if !path.starts_with(root) {
        return Err(ResolvedPluginError::ResourceOutsideRoot {
            root: root.clone(),
            path,
        });
    }
    Ok(PluginResourceLocator::Environment {
        environment_id: environment_id.to_string(),
        path,
    })
}
