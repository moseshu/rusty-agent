// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Explicit synchronous Git/npm source materialization and temporary-directory ownership.

use super::AbsolutePathBuf;
use super::git_policy::PluginGitMode;
use super::marketplace::MarketplacePluginSource;
use super::npm_source::materialize_npm_plugin_source;
use std::{fs, path::Path};
use tempfile::TempDir;

/// Original internal source helpers exposed only to the external test workspace.
#[cfg(feature = "test-api")]
#[doc(hidden)]
pub mod test_api;

#[derive(Debug)]
/// A resolved source with an owned temporary checkout or extraction lifetime.
pub struct MaterializedMarketplacePluginSource {
    /// The local source root; temporary contents live until this value is dropped.
    pub path: AbsolutePathBuf,
    _tempdir: Option<TempDir>,
}

/// Materializes a catalog source synchronously, defaulting Git operations to manual mode.
///
/// Callers supply a catalog-validated source and retain this result through Store installation.
/// This low-level operation does not apply the product runtime's network or enablement policy.
pub fn materialize_marketplace_plugin_source(
    codex_home: &Path,
    source: &MarketplacePluginSource,
) -> Result<MaterializedMarketplacePluginSource, String> {
    materialize_marketplace_plugin_source_with_mode(codex_home, source, PluginGitMode::Manual)
}

/// Applies the initiating operation's Git trust policy throughout plugin materialization.
pub(crate) fn materialize_marketplace_plugin_source_with_mode(
    codex_home: &Path,
    source: &MarketplacePluginSource,
    mode: PluginGitMode,
) -> Result<MaterializedMarketplacePluginSource, String> {
    match source {
        MarketplacePluginSource::Local { path } => Ok(MaterializedMarketplacePluginSource {
            path: path.clone(),
            _tempdir: None,
        }),
        MarketplacePluginSource::Git {
            url,
            path,
            ref_name,
            sha,
        } => {
            let staging_root = codex_home.join("plugins/.marketplace-plugin-source-staging");
            fs::create_dir_all(&staging_root).map_err(|err| {
                format!(
                    "failed to create marketplace plugin source staging directory {}: {err}",
                    staging_root.display()
                )
            })?;
            let tempdir = tempfile::Builder::new()
                .prefix("marketplace-plugin-source-")
                .tempdir_in(&staging_root)
                .map_err(|err| {
                    format!(
                        "failed to create marketplace plugin source staging directory in {}: {err}",
                        staging_root.display()
                    )
                })?;
            clone_git_plugin_source(
                codex_home,
                url,
                ref_name.as_deref(),
                sha.as_deref(),
                path.as_deref(),
                tempdir.path(),
                mode,
            )?;
            let path = if let Some(path) = path {
                AbsolutePathBuf::try_from(tempdir.path().join(path)).map_err(|err| {
                    format!("failed to resolve materialized plugin source path: {err}")
                })?
            } else {
                AbsolutePathBuf::try_from(tempdir.path().to_path_buf()).map_err(|err| {
                    format!("failed to resolve materialized plugin source path: {err}")
                })?
            };
            Ok(MaterializedMarketplacePluginSource {
                path,
                _tempdir: Some(tempdir),
            })
        }
        MarketplacePluginSource::Npm {
            package,
            version,
            registry,
        } => {
            let (path, tempdir) = materialize_npm_plugin_source(
                codex_home,
                package,
                version.as_deref(),
                registry.as_deref(),
            )?;
            Ok(MaterializedMarketplacePluginSource {
                path,
                _tempdir: Some(tempdir),
            })
        }
    }
}

fn clone_git_plugin_source(
    codex_home: &Path,
    url: &str,
    ref_name: Option<&str>,
    sha: Option<&str>,
    sparse_checkout_path: Option<&str>,
    destination: &Path,
    mode: PluginGitMode,
) -> Result<(), String> {
    let clone_cwd = match mode {
        PluginGitMode::Automatic => Some(codex_home),
        PluginGitMode::Manual => None,
    };
    if let Some(sparse_checkout_path) = sparse_checkout_path {
        run_git(
            &[
                "clone",
                "--filter=blob:none",
                "--sparse",
                "--no-checkout",
                url,
                destination.to_string_lossy().as_ref(),
            ],
            clone_cwd,
            mode,
        )?;
        run_git(
            &[
                "sparse-checkout",
                "set",
                "--no-cone",
                "--",
                sparse_checkout_path,
            ],
            Some(destination),
            mode,
        )?;
    } else {
        run_git(
            &["clone", url, destination.to_string_lossy().as_ref()],
            clone_cwd,
            mode,
        )?;
    }
    if let Some(sha) = sha {
        run_git(&["checkout", sha], Some(destination), mode)?;
        let checked_out_sha = run_git_output(&["rev-parse", "HEAD"], Some(destination), mode)?;
        if !checked_out_sha.eq_ignore_ascii_case(sha) {
            return Err(format!(
                "checked out Git SHA {checked_out_sha} does not match requested SHA {sha}"
            ));
        }
    } else if let Some(ref_name) = ref_name {
        run_git(&["checkout", ref_name], Some(destination), mode)?;
    } else if sparse_checkout_path.is_some() {
        run_git(&["checkout"], Some(destination), mode)?;
    }
    Ok(())
}

fn run_git(args: &[&str], cwd: Option<&Path>, mode: PluginGitMode) -> Result<(), String> {
    run_git_output(args, cwd, mode).map(drop)
}

fn run_git_output(
    args: &[&str],
    cwd: Option<&Path>,
    mode: PluginGitMode,
) -> Result<String, String> {
    let mut command = mode.command(Path::new("git"));
    command.args(args);
    command.env("GIT_TERMINAL_PROMPT", "0");
    let _trusted_repository = if let Some(cwd) = cwd
        && args.first() == Some(&"clone")
    {
        Some(super::git_policy::configure_trusted_git_repository(
            &mut command,
            cwd,
        )?)
    } else {
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
            if matches!(mode, PluginGitMode::Manual) {
                command.env_remove("GIT_DIR");
            }
        }
        None
    };

    let output = command
        .output()
        .map_err(|err| format!("failed to run git {}: {err}", args.join(" ")))?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).trim().to_string());
    }

    Err(format!(
        "git {} failed with status {}\nstdout:\n{}\nstderr:\n{}",
        args.join(" "),
        output.status,
        String::from_utf8_lossy(&output.stdout).trim(),
        String::from_utf8_lossy(&output.stderr).trim()
    ))
}
