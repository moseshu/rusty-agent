// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Executor-local paths and trusted startup settings used when preparing execution.

#![allow(
    clippy::needless_pass_by_value,
    reason = "Preserve the source ownership and public argument contract."
)]
#![allow(
    missing_docs,
    reason = "Internal records retain upstream field names and documentation; serde defines their wire contract."
)]
use std::path::PathBuf;

use ra_paths::AbsolutePathBuf;

/// Paths and sandbox settings initialized when creating an executor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecServerRuntimeOptions {
    /// Stable path to the Codex executable used to launch hidden helper modes.
    pub codex_self_exe: AbsolutePathBuf,
    /// Path to the Linux sandbox helper alias used when the platform sandbox
    /// needs to re-enter Codex by argv0.
    pub codex_linux_sandbox_exe: Option<AbsolutePathBuf>,
    /// Trusted startup policy; requests and repository config cannot change PID isolation.
    pub linux_sandbox_pid_namespace: LinuxSandboxPidNamespace,
    /// Trusted startup routing; request policy still controls destination access.
    pub proxy_private_ips_via_upstream: bool,
    /// Only the sandbox preference from CLI flags, never other startup config or secrets.
    pub prefer_mxc: Option<bool>,
    /// User-config opt-out of writable-root symlink checks beneath this host's home.
    #[cfg(target_os = "macos")]
    pub allowed_symlinked_codex_home: Option<AbsolutePathBuf>,
}

impl ExecServerRuntimeOptions {
    pub fn from_optional_paths(
        codex_self_exe: Option<PathBuf>,
        codex_linux_sandbox_exe: Option<PathBuf>,
    ) -> std::io::Result<Self> {
        let codex_self_exe = codex_self_exe.ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "Codex executable path is not configured",
            )
        })?;
        Self::new(codex_self_exe, codex_linux_sandbox_exe)
    }

    pub fn new(
        codex_self_exe: PathBuf,
        codex_linux_sandbox_exe: Option<PathBuf>,
    ) -> std::io::Result<Self> {
        Ok(Self {
            linux_sandbox_pid_namespace: LinuxSandboxPidNamespace::default(),
            proxy_private_ips_via_upstream: false,
            prefer_mxc: None,
            codex_self_exe: absolute_path(codex_self_exe)?,
            codex_linux_sandbox_exe: codex_linux_sandbox_exe.map(absolute_path).transpose()?,
            #[cfg(target_os = "macos")]
            allowed_symlinked_codex_home: None,
        })
    }

    /// Applies the PID namespace policy chosen by trusted executor provisioning.
    #[must_use]
    pub fn with_linux_sandbox_pid_namespace(mut self, mode: LinuxSandboxPidNamespace) -> Self {
        self.linux_sandbox_pid_namespace = mode;
        self
    }

    /// Routes permitted private IPs through the executor's configured upstream proxy.
    #[must_use]
    pub fn with_proxy_private_ips_via_upstream(mut self, enabled: bool) -> Self {
        self.proxy_private_ips_via_upstream = enabled;
        self
    }

    /// Applies the symlink opt-in resolved by the execution host's config loader.
    #[cfg(target_os = "macos")]
    #[must_use]
    pub fn with_allowed_symlinked_codex_home(
        mut self,
        allowed_symlinked_codex_home: Option<AbsolutePathBuf>,
    ) -> Self {
        self.allowed_symlinked_codex_home = allowed_symlinked_codex_home;
        self
    }
}

fn absolute_path(path: PathBuf) -> std::io::Result<AbsolutePathBuf> {
    AbsolutePathBuf::from_absolute_path(path.as_path())
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidInput, err))
}

// Startup-owned Linux PID isolation policy, independent of request permissions.

/// Selects the PID namespace policy for Linux bubblewrap sandboxes.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum LinuxSandboxPidNamespace {
    /// Create a new PID namespace, retaining the legacy `/proc` mount fallback.
    #[default]
    Isolate,
    /// Reuse the caller's PID namespace and `/proc`, allowing signals to other same-UID processes.
    Inherit,
}

impl std::str::FromStr for LinuxSandboxPidNamespace {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "isolate" => Ok(Self::Isolate),
            "inherit" => Ok(Self::Inherit),
            _ => Err("expected 'isolate' or 'inherit'".to_string()),
        }
    }
}
