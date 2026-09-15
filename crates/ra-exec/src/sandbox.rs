//! Sandbox backends.
//!
//! A backend is gated twice, by feature and by platform: the feature says "I want this backend"
//! and `target_os` says "it only exists on this platform". A default build gets [`seatbelt`] on
//! macOS and [`bwrap`] on Linux, with [`unix_local`] as the baseline present on both.
//!
//! **No silent downgrade**: when a platform has no real sandbox backend available, construction
//! must fail and say what is missing. It must not quietly fall back to [`unix_local`] while the
//! caller believes it is isolated.

#[cfg(all(feature = "bwrap", target_os = "linux"))]
pub mod bwrap;
#[cfg(feature = "docker")]
pub mod docker;
pub mod manifest;
pub mod network;
#[cfg(all(feature = "seatbelt", target_os = "macos"))]
pub mod seatbelt;
pub mod unix_local;

use std::sync::Arc;

use crate::{
    sandbox::unix_local::{EnvPolicy, ResourceLimits},
    tmpdir::RunTempDir,
};

/// How a host wants its child processes set up, independent of which backend confines them.
///
/// Held by a [`crate::session::ProcessManager`] rather than travelling on a request, because every
/// field here is a decision about what a command is *allowed* to be, and requests are built from
/// model output. A policy a command could name is a policy a command could widen.
///
/// # Why there is no `SandboxBackend` trait yet
///
/// One implementation cannot show which parts of an interface are general. The two sources this
/// design draws on are shaped differently — codex's `sandboxing` wraps a single spawn in a
/// confinement, while openai's `sandbox/sandboxes` provisions a session with `start`, `stop` and a
/// persistable state — and a trait written now would be traced around whichever one happened to be
/// implemented first. The shared trait is written when there
/// are two of them to look at, and this type is what they will take as input.
#[derive(Debug, Clone, Default)]
pub struct ExecEnvironment {
    env_policy: EnvPolicy,
    limits: ResourceLimits,
    temp_dir: Option<Arc<RunTempDir>>,
}

impl ExecEnvironment {
    /// Creates the default environment: the default environment policy and no ceilings but core
    /// dumps.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets how a child's environment is derived.
    #[must_use]
    pub fn with_env_policy(mut self, policy: EnvPolicy) -> Self {
        self.env_policy = policy;
        self
    }

    /// Sets the per-process resource ceilings.
    #[must_use]
    pub fn with_resource_limits(mut self, limits: ResourceLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Attaches the run's scratch directory, which children are told about and hold open.
    #[must_use]
    pub fn with_temp_dir(mut self, temp_dir: Arc<RunTempDir>) -> Self {
        self.temp_dir = Some(temp_dir);
        self
    }

    /// How a child's environment is derived.
    #[must_use]
    pub const fn env_policy(&self) -> &EnvPolicy {
        &self.env_policy
    }

    /// The per-process resource ceilings.
    #[must_use]
    pub const fn resource_limits(&self) -> &ResourceLimits {
        &self.limits
    }

    /// The run's scratch directory, when one is attached.
    #[must_use]
    pub fn temp_dir(&self) -> Option<&Arc<RunTempDir>> {
        self.temp_dir.as_ref()
    }
}
