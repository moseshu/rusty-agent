// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Test-only access to original source helpers, without policy or result emulation.

use super::super::AbsolutePathBuf;
pub use super::super::git_policy::PluginGitMode;
pub use super::super::plugin_bundle_archive::{PluginBundlePackError, PluginBundleUnpackError};
use std::{ffi::OsStr, path::Path, process::Command};
use tempfile::TempDir;

/// Creates the original background-process command.
pub fn background_command(program: impl AsRef<OsStr>) -> Command {
    super::super::process::background_command(program)
}
/// Creates the original Git command without executing it.
#[must_use]
pub fn git_command(mode: PluginGitMode, binary: &Path) -> Command {
    mode.command(binary)
}
/// Returns the original ambient repository variable list.
#[must_use]
pub fn repository_local_git_variables() -> &'static [&'static str] {
    super::super::git_policy::REPOSITORY_LOCAL_GIT_ENVIRONMENT_VARIABLES
}
/// Configures the original trusted bare-repository guard.
pub fn configure_trusted_repository(command: &mut Command, home: &Path) -> Result<TempDir, String> {
    super::super::git_policy::configure_trusted_git_repository(command, home)
}
/// Materializes with an explicit initiating-operation mode.
pub fn materialize_with_mode(
    home: &Path,
    source: &super::super::marketplace::MarketplacePluginSource,
    mode: PluginGitMode,
) -> Result<super::MaterializedMarketplacePluginSource, String> {
    super::materialize_marketplace_plugin_source_with_mode(home, source, mode)
}
/// Runs the original Git command helper for local repository fixtures.
pub fn run_git(args: &[&str], cwd: Option<&Path>, mode: PluginGitMode) -> Result<(), String> {
    super::run_git(args, cwd, mode)
}
/// Reads the original Git command output for local repository fixtures.
pub fn run_git_output(
    args: &[&str],
    cwd: Option<&Path>,
    mode: PluginGitMode,
) -> Result<String, String> {
    super::run_git_output(args, cwd, mode)
}
/// Runs the original npm source path with an explicit fixture command.
pub fn materialize_npm_with_command(
    home: &Path,
    package: &str,
    version: Option<&str>,
    registry: Option<&str>,
    command: &OsStr,
) -> Result<(AbsolutePathBuf, TempDir), String> {
    super::super::npm_source::materialize_npm_plugin_source_with_command(
        home, package, version, registry, command,
    )
}
/// Packs through the original local bundle writer.
pub fn pack(path: &Path, limit: usize) -> Result<Vec<u8>, PluginBundlePackError> {
    super::super::plugin_bundle_archive::pack_plugin_bundle_tar_gz(path, limit)
}
/// Unpacks through the original bounded extraction path.
pub fn unpack(bytes: &[u8], destination: &Path, limit: u64) -> Result<(), PluginBundleUnpackError> {
    super::super::plugin_bundle_archive::unpack_plugin_bundle_tar_gz(bytes, destination, limit)
}
/// Checks the original cumulative-size overflow branch without allocating a huge archive.
pub fn enforce_size(size: u64, total: &mut u64, limit: u64) -> Result<(), PluginBundleUnpackError> {
    super::super::plugin_bundle_archive::enforce_total_extracted_size(size, total, limit)
}
/// Inspects the original archive-size admission check.
pub fn read_npm_archive(path: &Path) -> Result<Vec<u8>, String> {
    super::super::npm_source::read_npm_package_archive(path)
}
