//! The sandbox half of an agent declaration.
//!
//! The reference makes a sandbox agent a subclass of its agent class with four more fields. Here
//! the four fields are a [`SandboxAgentConfig`] an [`AgentSpec`](crate::agent::AgentSpec) carries,
//! and an agent that carries one is a sandbox agent. Composition rather than a second agent type,
//! because every consumer of an agent — the registry, handoffs, the loop — would otherwise have to
//! learn a second type that differs only in these fields.
//!
//! What a run needs to *give* such an agent a workspace — a client, a live session, a state to
//! resume — is not here. As on the reference, that belongs to the run configuration: the same agent
//! can run against a local directory in a test and a container in production without being
//! redeclared.
//!
//! This module also holds the manifest processing the runtime performs before a session is created
//! or resumed, because it reads private parts of a [`Manifest`] that only this crate can reach.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::agent::AgentInstructions;
use crate::capability::Capability;
use crate::error::{Error, Result};

use super::manifest::Manifest;
use super::mount_security::{
    manifest_has_configured_mount_authority, replace_protected_mount_error,
    validate_manifest_mount_provenance,
};
use super::session::SandboxResult;
use super::types::User;

/// What makes an agent a sandbox agent.
///
/// # The default capabilities
///
/// The reference gives every sandbox agent a filesystem, a shell and compaction unless it names its
/// own. Those are implementations with tools and IO, which this crate does not hold, so a
/// configuration here tells apart the two cases the reference's default makes: capabilities left
/// **unspecified** ([`Self::new`], [`Self::default`]), which stand for the default set, and
/// capabilities **named** — [`Self::with_capability`], [`Self::with_capabilities`], or none at all
/// with [`Self::empty`].
///
/// The run fills in an unspecified set from the default its configuration supplies, and refuses an
/// agent whose set is unspecified when the run supplies none, before any session is made: an agent
/// that silently ran without the tools the reference would have given it is the failure this
/// avoids.
#[must_use]
pub struct SandboxAgentConfig {
    default_manifest: Option<Manifest>,
    base_instructions: Option<AgentInstructions>,
    /// `None` when unspecified, which stands for the default set.
    capabilities: Option<Vec<Arc<dyn Capability>>>,
    run_as: Option<User>,
    /// Runs currently using this configuration, which the reference allows to be at most one.
    active_runs: Arc<AtomicUsize>,
}

impl SandboxAgentConfig {
    /// A sandbox agent with no manifest of its own, the default base prompt, the default
    /// capabilities and the session's own user — the reference's `SandboxAgent` with nothing named.
    ///
    /// The capabilities are left unspecified: the run installs the default set its configuration
    /// supplies, and refuses the agent when it supplies none.
    pub fn new() -> Self {
        Self {
            default_manifest: None,
            base_instructions: None,
            capabilities: None,
            run_as: None,
            active_runs: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// As [`Self::new`], but with no capabilities at all rather than the default ones — the
    /// reference's `capabilities=[]`.
    pub fn empty() -> Self {
        Self {
            capabilities: Some(Vec::new()),
            ..Self::new()
        }
    }

    /// The manifest a new session is created with when the run configuration names none.
    pub fn with_default_manifest(mut self, manifest: Manifest) -> Self {
        self.default_manifest = Some(manifest);
        self
    }

    /// Replaces the base sandbox prompt.
    ///
    /// Most callers want the agent's own instructions instead: those are added beneath the base
    /// prompt, while this replaces the prompt that explains the workspace in the first place.
    pub fn with_base_instructions(mut self, instructions: AgentInstructions) -> Self {
        self.base_instructions = Some(instructions);
        self
    }

    /// Installs one capability, after those already installed.
    ///
    /// On a configuration whose capabilities were unspecified this names them, and the default set
    /// no longer applies: as on the reference, naming capabilities replaces the default rather than
    /// adding to it.
    pub fn with_capability(mut self, capability: Arc<dyn Capability>) -> Self {
        self.capabilities
            .get_or_insert_with(Vec::new)
            .push(capability);
        self
    }

    /// Installs several capabilities, in order, after those already installed.
    ///
    /// Like [`Self::with_capability`], this names the capabilities, even when `capabilities` is
    /// empty.
    pub fn with_capabilities(
        mut self,
        capabilities: impl IntoIterator<Item = Arc<dyn Capability>>,
    ) -> Self {
        self.capabilities
            .get_or_insert_with(Vec::new)
            .extend(capabilities);
        self
    }

    /// Runs model-facing sandbox tools as `user`.
    pub fn with_run_as(mut self, user: User) -> Self {
        self.run_as = Some(user);
        self
    }

    /// The manifest a new session is created with when the run configuration names none.
    #[must_use]
    pub const fn default_manifest(&self) -> Option<&Manifest> {
        self.default_manifest.as_ref()
    }

    /// The base sandbox prompt, or `None` for the built-in one.
    #[must_use]
    pub const fn base_instructions(&self) -> Option<&AgentInstructions> {
        self.base_instructions.as_ref()
    }

    /// The named capabilities, in installation order; empty when they are unspecified.
    ///
    /// What a run installs is [`Self::resolve_capabilities`]'s answer, not this one: an unspecified
    /// set stands for the default.
    #[must_use]
    pub fn capabilities(&self) -> &[Arc<dyn Capability>] {
        self.capabilities.as_deref().unwrap_or_default()
    }

    /// Whether the capabilities were named — possibly as none — rather than left to the default.
    #[must_use]
    pub const fn capabilities_specified(&self) -> bool {
        self.capabilities.is_some()
    }

    /// This configuration with its capabilities settled: the named ones, or, when they were left
    /// unspecified, what `defaults` produces.
    ///
    /// The copy shares this configuration's run claim, so claiming either claims both.
    pub fn resolve_capabilities(
        &self,
        defaults: impl FnOnce() -> Vec<Arc<dyn Capability>>,
    ) -> Self {
        Self {
            default_manifest: self.default_manifest.clone(),
            base_instructions: self.base_instructions.clone(),
            capabilities: Some(self.capabilities.clone().unwrap_or_else(defaults)),
            run_as: self.run_as.clone(),
            active_runs: Arc::clone(&self.active_runs),
        }
    }

    /// Who model-facing sandbox tools run as, or `None` for the session's own user.
    #[must_use]
    pub const fn run_as(&self) -> Option<&User> {
        self.run_as.as_ref()
    }

    /// Claims this configuration for one run.
    ///
    /// The claim lasts until the returned lease is dropped. The reference refuses a second
    /// concurrent claim on the same agent, and so does this: capabilities bound to one run's
    /// session and a second run preparing the same agent would each be describing a workspace the
    /// other is changing.
    ///
    /// # Errors
    ///
    /// Returns a caller error when another run holds a claim.
    pub fn acquire_run(&self, agent_name: &str) -> Result<SandboxAgentRunLease> {
        let claimed = self
            .active_runs
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire);
        if claimed.is_err() {
            return Err(Error::caller(format!(
                "SandboxAgent {agent_name:?} cannot be reused concurrently across runs"
            )));
        }
        Ok(SandboxAgentRunLease {
            active_runs: Arc::clone(&self.active_runs),
        })
    }
}

impl Default for SandboxAgentConfig {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for SandboxAgentConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SandboxAgentConfig")
            .field("default_manifest", &self.default_manifest.is_some())
            .field("base_instructions", &self.base_instructions)
            .field(
                "capabilities",
                &self.capabilities.as_ref().map(|capabilities| {
                    capabilities
                        .iter()
                        .map(|capability| capability.kind())
                        .collect::<Vec<_>>()
                }),
            )
            .field("run_as", &self.run_as)
            .finish_non_exhaustive()
    }
}

/// One run's claim on a sandbox agent, released when dropped.
#[must_use]
pub struct SandboxAgentRunLease {
    active_runs: Arc<AtomicUsize>,
}

impl fmt::Debug for SandboxAgentRunLease {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("SandboxAgentRunLease").finish()
    }
}

impl Drop for SandboxAgentRunLease {
    fn drop(&mut self) {
        self.active_runs.store(0, Ordering::Release);
    }
}

/// `manifest` with `user` added to its accounts, unless it already names that user directly or in
/// a group.
#[must_use]
pub fn manifest_with_run_as_user(manifest: Manifest, user: Option<&User>) -> Manifest {
    let Some(user) = user else {
        return manifest;
    };
    let named = manifest
        .users
        .iter()
        .any(|existing| existing.name == user.name)
        || manifest
            .groups
            .iter()
            .flat_map(|group| group.users.iter())
            .any(|existing| existing.name == user.name);
    if named {
        return manifest;
    }
    let mut manifest = manifest;
    manifest.users.push(user.clone());
    manifest
}

/// The manifest a session is created or resumed with, after the agent's capabilities changed it.
///
/// Refuses a manifest whose mounts did not come from this process before anything runs, then adds
/// the run-as user, then hands the copy through each capability in order. Credential exposure the
/// host acknowledged survives every step, including a capability that returned a manifest it built
/// from scratch.
///
/// # Errors
///
/// Returns the provenance refusal, or the first capability's failure — replaced by one that quotes
/// nothing when the manifest carried mount authority, whether the capability was handed it or
/// added it before failing.
pub fn process_manifest(
    capabilities: &[Arc<dyn Capability>],
    manifest: &Manifest,
    run_as: Option<&User>,
) -> SandboxResult<Manifest> {
    validate_manifest_mount_provenance(manifest)?;
    let mut processed = manifest_with_run_as_user(manifest.clone(), run_as);
    for capability in capabilities {
        let handed = processed.clone();
        match capability.process_manifest(&mut processed) {
            Ok(()) => processed.merge_mount_credential_exposure_policy_from(&handed),
            Err(error)
                if manifest_has_configured_mount_authority(&handed)
                    || manifest_has_configured_mount_authority(&processed) =>
            {
                return Err(replace_protected_mount_error(&error));
            }
            Err(error) => return Err(error),
        }
    }
    Ok(processed)
}
