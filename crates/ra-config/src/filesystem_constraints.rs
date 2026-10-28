// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Convert resolved filesystem denials into portable policy entries.

#![allow(
    missing_docs,
    reason = "Preserve source product configuration and validation records."
)]
use crate::FilesystemConstraints;
use ra_filesystem::permissions::FileSystemAccessMode;
use ra_filesystem::permissions::FileSystemPath;
use ra_filesystem::permissions::FileSystemSandboxEntry;
use ra_filesystem::permissions::FileSystemSandboxPolicy;
use ra_paths::LegacyAppPathString;
use ra_paths::LegacyAppPathStringError;
use ra_paths::PathConvention;
use ra_paths::PathUri;
use std::io;

impl FilesystemConstraints {
    /// Adds denials without changing existing entries or other policy settings.
    /// `convention` must match the executor facts used to resolve these requirements.
    /// Literal paths become URIs; glob spellings and entry order are preserved.
    /// Invalid denials return an error before the policy is changed.
    pub fn apply_to_policy(
        &self,
        policy: &mut FileSystemSandboxPolicy,
        convention: PathConvention,
    ) -> io::Result<()> {
        let entries = self
            .deny_read
            .iter()
            .map(|deny_read| {
                PathUri::validate_config_path_text(deny_read.as_str(), convention)?;
                let path = if deny_read.contains_glob() {
                    FileSystemPath::GlobPattern {
                        pattern: deny_read.as_str().to_string(),
                    }
                } else {
                    let mut path = LegacyAppPathString::from_string(deny_read.as_str())
                        .to_path_uri(convention)?;
                    path.validate_config_path(convention)?;
                    // Match Core's existing URI spelling for UNC share roots.
                    if path.to_url().host_str().is_some() {
                        path = path.join(".")?;
                    }
                    path.into()
                };
                Ok(FileSystemSandboxEntry::new(
                    path,
                    FileSystemAccessMode::Deny,
                ))
            })
            .collect::<Result<Vec<_>, LegacyAppPathStringError>>()
            .map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("invalid permissions.filesystem.deny_read path: {error}"),
                )
            })?;
        for entry in entries {
            if !policy.entries.contains(&entry) {
                policy.entries.push(entry);
            }
        }
        Ok(())
    }
}
