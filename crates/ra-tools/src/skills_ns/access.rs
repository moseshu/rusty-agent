// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Captured executor access and its permission-sensitive continuation identity.

use std::{
    fs::File,
    path::Path,
    sync::{Arc, Weak},
};

use ra_core::{context::RunContext, error::Result};
use ra_exec::fs::{RootedFileSystem, Workspace};

/// A product's immutable filesystem permissions for one callback.
pub trait SkillFilePermissions: std::any::Any + Send + Sync {
    /// Compares the complete captured permission and path context by value.
    fn same_permissions(&self, other: &dyn SkillFilePermissions) -> bool;

    /// Checks the opened descriptor before any contents are read.
    fn check_read(&self, file: &File, requested_path: &Path) -> std::io::Result<()>;
}

/// Combines the callback's step settings with current session and originating-turn grants.
pub trait SkillExecutorAccessPolicy: Send + Sync {
    /// Captures one callback's access. An absent policy retains the explicitly selected roots.
    fn capture(&self, context: &RunContext) -> Result<Option<Arc<dyn SkillFilePermissions>>>;
}

/// A selected directory and the permissions captured for this callback.
#[derive(Clone)]
#[non_exhaustive]
pub struct SkillExecutorAccess {
    pub(super) workspace: Workspace,
    permissions: Option<Arc<dyn SkillFilePermissions>>,
}

impl SkillExecutorAccess {
    /// Reads bounded UTF-8 text through this access, preserving descriptor-scoped authorization.
    pub async fn read_text(
        &self,
        path: &Path,
        resource: &str,
    ) -> super::catalog::SkillProviderResult<String> {
        let access = self.clone();
        let path = path.to_path_buf();
        let resource = resource.to_string();
        tokio::task::spawn_blocking(move || {
            super::provider::read_bounded_text(&access, &path, &resource)
        })
        .await
        .map_err(|error| super::catalog::SkillProviderError::new(error.to_string()))?
    }

    /// Creates access to a selected root, optionally constrained by immutable callback permissions.
    #[must_use]
    pub fn new(workspace: Workspace, permissions: Option<Arc<dyn SkillFilePermissions>>) -> Self {
        Self {
            workspace,
            permissions,
        }
    }

    pub(super) fn cache_key(&self) -> SkillExecutorAccessKey {
        SkillExecutorAccessKey {
            filesystem: Arc::downgrade(self.workspace.filesystem()),
            permissions: self.permissions.clone(),
        }
    }

    pub(super) fn check_read(&self, file: &File, path: &Path) -> std::io::Result<()> {
        match &self.permissions {
            Some(permissions) => permissions.check_read(file, path),
            None => Ok(()),
        }
    }
}

#[derive(Clone)]
pub(super) struct SkillExecutorAccessKey {
    filesystem: Weak<RootedFileSystem>,
    permissions: Option<Arc<dyn SkillFilePermissions>>,
}

impl PartialEq for SkillExecutorAccessKey {
    fn eq(&self, other: &Self) -> bool {
        self.filesystem.ptr_eq(&other.filesystem)
            && match (&self.permissions, &other.permissions) {
                (None, None) => true,
                (Some(left), Some(right)) => left.same_permissions(right.as_ref()),
                _ => false,
            }
    }
}
impl Eq for SkillExecutorAccessKey {}
