//! Sandbox backends.
//!
//! A backend is gated by its feature, and *selected* by platform: the feature says "build this
//! backend" and [`platform_backend`] says which one can actually confine a process on the machine
//! this build is running on. A default build gets [`seatbelt`] on macOS and [`bwrap`] on Linux,
//! with [`unix_local`] as the baseline present on both.
//!
//! **The platform gate moved off the module and onto the selection** (it used to sit on the `mod`
//! declaration, so `seatbelt` did not compile on Linux and `bwrap` did not compile on macOS). Each
//! backend is two very different things wearing one name: a pure translation from a
//! [`SandboxPolicy`] into a command line, and a handful of calls that only mean something on one
//! kernel. Gating the whole module put the first half out of reach of everyone not sitting on that
//! kernel — the policy a Linux host will run was unbuildable, let alone testable, on the machine it
//! was written on. So the translation compiles everywhere and is tested everywhere; the kernel
//! calls keep their own `cfg`; and selection still refuses to hand a macOS host a Linux backend.
//!
//! **No silent downgrade**: when the requested guarantee cannot be delivered, resolution fails and
//! names what is missing. It falls back to a weaker configuration only when the host said in
//! advance which weaker one it would accept, and then the configuration actually in force is
//! reported with the result — see [`resolve_confinement`] and [`Confinement`].

#[cfg(feature = "bwrap")]
pub mod bwrap;
#[cfg(feature = "docker")]
pub mod docker;
pub mod manifest;
pub mod network;
#[cfg(feature = "seatbelt")]
pub mod seatbelt;
pub mod unix_local;

use std::{
    collections::BTreeSet,
    fmt,
    path::{Path, PathBuf},
    sync::Arc,
};

use ra_core::compat::{SchemaVersion, Unknown};
use serde::{Deserialize, Serialize};

use crate::{
    EXEC_SCHEMA_VERSION,
    sandbox::unix_local::{EnvPolicy, ResourceLimits},
    tmpdir::RunTempDir,
};

const fn default_schema_version() -> SchemaVersion {
    EXEC_SCHEMA_VERSION
}

/// How a host wants its child processes set up, independent of which backend confines them.
///
/// Held by a [`crate::session::ProcessManager`] rather than travelling on a request, because every
/// field here is a decision about what a command is *allowed* to be, and requests are built from
/// model output. A policy a command could name is a policy a command could widen.
///
/// # The `SandboxBackend` trait is now written, and this is its input
///
/// It was deliberately withheld while only one backend existed, because one implementation cannot
/// show which parts of an interface are general — and the two sources this design draws on are
/// shaped differently: codex's `sandboxing` wraps a single spawn in a confinement, while openai's
/// `sandbox/sandboxes` provisions a session with `start`, `stop` and a persistable state. With
/// [`seatbelt`] and [`bwrap`] both landed there are two to look at, and [`SandboxBackend`] is
/// traced around what they have in common: a policy goes in, a command line comes out, and the
/// backend says up front how strong a guarantee it can actually deliver here.
#[derive(Debug, Clone, Default)]
pub struct ExecEnvironment {
    env_policy: EnvPolicy,
    limits: ResourceLimits,
    temp_dir: Option<Arc<RunTempDir>>,
    sandbox: Option<SandboxPolicy>,
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

    /// Asks for every child to be confined by a platform backend.
    ///
    /// Setting a policy cannot fail, but running under it can: the backend is chosen and the
    /// guarantee checked when a command is about to start, so that a host which loses its sandbox
    /// mid-run is refused rather than quietly unconfined. See [`Self::resolve_confinement`].
    #[must_use]
    pub fn with_sandbox(mut self, policy: SandboxPolicy) -> Self {
        self.sandbox = Some(policy);
        self
    }

    /// The confinement policy, when the host asked for one.
    ///
    /// `None` is not "confined by nothing it happens to have configured" — it is the host saying it
    /// wants only the baseline. The distinction is reported as [`SandboxLevel::Unconfined`] rather
    /// than left for a reader to assume.
    #[must_use]
    pub const fn sandbox(&self) -> Option<&SandboxPolicy> {
        self.sandbox.as_ref()
    }

    /// Chooses the backend for this environment's policy and works out what it will deliver.
    ///
    /// The run's scratch directory is added to the writable set here rather than by the host: a
    /// command is told about it through `RUSTY_AGENT_TMPDIR` and is expected to use it, so a sandbox
    /// that did not include it would hand every command a directory it cannot write to.
    ///
    /// # Errors
    ///
    /// Returns [`SandboxError`] when the requested guarantee cannot be delivered and no weaker one
    /// was authorised in advance.
    pub fn resolve_confinement(&self) -> Result<Confinement, SandboxError> {
        let Some(policy) = &self.sandbox else {
            return Ok(Confinement::baseline());
        };
        resolve_confinement(policy, self.temp_dir.as_ref().map(|dir| dir.path()))
    }
}

/// A named confinement strength, ordered from weakest to strongest.
///
/// # Why named levels rather than a bag of switches
///
/// A host has to be able to say what it wants in one word and have the answer come back in the same
/// word. A policy assembled out of independent switches has no answer to "did I get what I asked
/// for" — every backend would satisfy a different subset, and the report would be a diff. So the
/// filesystem guarantee is a ladder, each rung is spelled out below in terms of what a command can
/// actually reach, and [`Confinement::level`] names the rung in force.
///
/// **The ladder is Rusty's own.** Neither reference publishes one: codex composes a policy out of
/// readable roots, writable roots and a platform-defaults flag, and nothing there is sorted by
/// strength. The rungs here are chosen so that each is strictly weaker than the one above it on
/// every axis it covers, which is what makes "fall back one rung" a statement a host can reason
/// about instead of a trade.
///
/// **Network is deliberately not on this ladder.** The most ordinary real configuration — write the
/// workspace, reach nothing on the network — would be unreachable if the two were fused, so
/// [`NetworkAccess`] is its own axis. It is also never downgraded: see [`SandboxPolicy::network`].
#[non_exhaustive]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SandboxLevel {
    /// Nothing is confined beyond the baseline: the environment a child is given, its resource
    /// ceilings, and the directory it starts in.
    ///
    /// This is the [`unix_local`] case, and it is a level rather than the absence of one so that a
    /// result can say so out loud. A reader should never have to infer from a missing field that a
    /// command ran unconfined.
    #[default]
    Unconfined,
    /// Reads are unconfined; writes reach only the declared writable roots.
    ///
    /// The level a coding agent normally runs at. Reading the whole filesystem is what keeps
    /// arbitrary toolchains working — a compiler reads its own installation, a package manager
    /// reads a cache in the home directory — and the boundary that matters for a command written by
    /// a model is what it can change.
    WorkspaceWrite,
    /// Reads reach only the declared readable roots plus the platform's own runtime; writes reach
    /// only the declared writable roots.
    ///
    /// "The platform's own runtime" is not a courtesy: with no access to the loader, the system
    /// libraries and the standard devices, no program starts at all, so the set is the floor that
    /// makes the level usable rather than a widening of it. What it does cost is every toolchain
    /// installed outside that set — a language runtime under a home directory is unreadable here
    /// unless the host names it as a readable root.
    Isolated,
}

impl SandboxLevel {
    /// The level's name in reports and configuration.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unconfined => "unconfined",
            Self::WorkspaceWrite => "workspace-write",
            Self::Isolated => "isolated",
        }
    }

    /// Whether this level confines anything at all, and therefore needs a backend.
    #[must_use]
    pub const fn needs_backend(self) -> bool {
        !matches!(self, Self::Unconfined)
    }
}

impl fmt::Display for SandboxLevel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Whether a confined command may reach the network.
///
/// This is the backend's own on/off switch, and it is all a backend can enforce: a mount namespace
/// or a seatbelt profile can say "no sockets", not "only this host". Per-domain allowlists are a
/// layer above and belong to the network policy task, which sits on top of this rather than
/// replacing it.
#[non_exhaustive]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NetworkAccess {
    /// No sockets to anywhere.
    #[default]
    Denied,
    /// The host's network, as the parent process sees it.
    Allowed,
}

impl NetworkAccess {
    /// The access's name in reports and configuration.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Denied => "denied",
            Self::Allowed => "allowed",
        }
    }

    /// Whether the network is reachable.
    #[must_use]
    pub const fn is_allowed(self) -> bool {
        matches!(self, Self::Allowed)
    }
}

impl fmt::Display for NetworkAccess {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// What a host asks a backend to guarantee.
///
/// Held by the [`ExecEnvironment`], never by a request: every field here is a bound on what a
/// command may do, and requests are built from model output.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxPolicy {
    #[serde(default = "default_schema_version")]
    schema_version: SchemaVersion,
    #[serde(default)]
    level: SandboxLevel,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    accept_level: Option<SandboxLevel>,
    #[serde(default)]
    network: NetworkAccess,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    writable_roots: Vec<PathBuf>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    readable_roots: Vec<PathBuf>,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl Default for SandboxPolicy {
    fn default() -> Self {
        Self::new(SandboxLevel::default())
    }
}

impl SandboxPolicy {
    /// Asks for `level`, with no downgrade permitted and no network.
    ///
    /// Denying the network by default is the one place this type answers a product question, and it
    /// answers it the safe way: a host that wants a confined command to reach the internet has to
    /// have written that down. [`SandboxLevel::Unconfined`] is the exception — it confines nothing,
    /// so pairing it with a denied network would be a promise nothing keeps, and
    /// [`resolve_confinement`] refuses that combination rather than pretending.
    #[must_use]
    pub fn new(level: SandboxLevel) -> Self {
        Self {
            schema_version: EXEC_SCHEMA_VERSION,
            level,
            accept_level: None,
            network: if level.needs_backend() {
                NetworkAccess::Denied
            } else {
                NetworkAccess::Allowed
            },
            writable_roots: Vec::new(),
            readable_roots: Vec::new(),
            unknown: Unknown::new(),
        }
    }

    /// Names the weakest level this host will accept if the requested one cannot be delivered.
    ///
    /// **Without this, there is no downgrade at all**: a backend that cannot keep the promise makes
    /// the command fail. With it, the strongest deliverable level at or above `level` is used and
    /// reported by [`Confinement::downgraded_from`]. Passing the requested level back is the same as
    /// not calling this.
    #[must_use]
    pub const fn accepting_down_to(mut self, level: SandboxLevel) -> Self {
        self.accept_level = Some(level);
        self
    }

    /// Sets whether a confined command may reach the network.
    #[must_use]
    pub const fn with_network(mut self, network: NetworkAccess) -> Self {
        self.network = network;
        self
    }

    /// Adds a root the command may write to, along with everything beneath it.
    #[must_use]
    pub fn with_writable_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.writable_roots.push(root.into());
        self
    }

    /// Adds a root the command may read, along with everything beneath it.
    ///
    /// Only [`SandboxLevel::Isolated`] confines reads, so this is inert at the weaker levels. It is
    /// accepted there anyway so that a host can lower the level of a policy it already wrote
    /// without its readable roots silently becoming an error.
    #[must_use]
    pub fn with_readable_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.readable_roots.push(root.into());
        self
    }

    /// The requested level.
    #[must_use]
    pub const fn level(&self) -> SandboxLevel {
        self.level
    }

    /// The weakest acceptable level, when the host authorised a downgrade.
    #[must_use]
    pub const fn accept_level(&self) -> Option<SandboxLevel> {
        self.accept_level
    }

    /// Whether a confined command may reach the network.
    ///
    /// **This is never downgraded.** A ladder of network guarantees would have one rung — either a
    /// command can open a socket or it cannot — so there is nothing for a host to authorise in
    /// advance and no weaker configuration that still honours the request. A backend that cannot
    /// deny the network to a policy that asked for it makes the command fail, and so does a
    /// downgrade to a level that has no way to deny it.
    #[must_use]
    pub const fn network(&self) -> NetworkAccess {
        self.network
    }

    /// Roots the command may write to.
    #[must_use]
    pub fn writable_roots(&self) -> &[PathBuf] {
        &self.writable_roots
    }

    /// Roots the command may read.
    #[must_use]
    pub fn readable_roots(&self) -> &[PathBuf] {
        &self.readable_roots
    }

    /// Unknown fields preserved during forward-compatible deserialization.
    #[must_use]
    pub const fn unknown(&self) -> &Unknown {
        &self.unknown
    }
}

/// A program and the arguments it is given, before or after a backend wraps it.
///
/// The wrapping is the whole of what a backend does to a command: `sandbox-exec` and `bwrap` both
/// take a policy of their own and then the command line to run under it. Keeping that a value —
/// rather than a half-built [`std::process::Command`] — is what lets the translation be tested by
/// reading it.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxCommand {
    program: PathBuf,
    args: Vec<String>,
    #[cfg(all(feature = "bwrap", target_os = "linux"))]
    seccomp: Option<Vec<u8>>,
}

impl SandboxCommand {
    /// Creates a command from a program and its arguments.
    #[must_use]
    pub fn new(
        program: impl Into<PathBuf>,
        args: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
            #[cfg(all(feature = "bwrap", target_os = "linux"))]
            seccomp: None,
        }
    }

    /// The program to execute, **for reading, not for spawning**.
    ///
    /// A command built out of this and [`Self::args`] loses whatever the backend attached to it —
    /// on Linux that is the syscall filter, so such a command would run unfiltered while looking
    /// identical. These two exist for reports and for tests that assert on the argument list;
    /// [`Self::prepare`] is the only way to launch one.
    #[must_use]
    pub fn program(&self) -> &Path {
        &self.program
    }

    /// Its arguments, not including the program name. For reading, not for spawning — see
    /// [`Self::program`].
    #[must_use]
    pub fn args(&self) -> &[String] {
        &self.args
    }

    /// Prepares a spawn, including inherited policy descriptors required by the backend.
    ///
    /// Callers must use this rather than spawning `program()` and `args()` directly: those
    /// accessors describe the command but do not transfer the seccomp policy to bubblewrap.
    pub fn prepare(&self) -> std::io::Result<std::process::Command> {
        let mut command = std::process::Command::new(&self.program);
        #[cfg(all(feature = "bwrap", target_os = "linux"))]
        if let Some(filter) = &self.seccomp {
            bwrap::seccomp::attach(&mut command, filter)?;
        }
        command.args(&self.args);
        Ok(command)
    }

    #[cfg(any(feature = "seatbelt", feature = "bwrap"))]
    fn into_parts(self) -> (PathBuf, Vec<String>) {
        (self.program, self.args)
    }
}

/// Everything a backend needs to translate one policy into one command line.
///
/// The scratch directory arrives here already folded into [`Self::writable_roots`]; a backend sees
/// one writable set and has no separate case for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfinementRequest {
    level: SandboxLevel,
    network: NetworkAccess,
    writable_roots: Vec<PathBuf>,
    readable_roots: Vec<PathBuf>,
}

impl ConfinementRequest {
    /// The level the backend is being asked to deliver.
    #[must_use]
    pub const fn level(&self) -> SandboxLevel {
        self.level
    }

    /// Whether the command may reach the network.
    #[must_use]
    pub const fn network(&self) -> NetworkAccess {
        self.network
    }

    /// Roots the command may write to, scratch directory included, deduplicated and in order.
    #[must_use]
    pub fn writable_roots(&self) -> &[PathBuf] {
        &self.writable_roots
    }

    /// Roots the command may read. Writable roots are readable and are not repeated here.
    #[must_use]
    pub fn readable_roots(&self) -> &[PathBuf] {
        &self.readable_roots
    }
}

/// A platform mechanism that can confine a spawned process.
///
/// Two implementations exist, and the trait is the shape they turned out to share rather than a
/// design imposed on them. Both answer two questions: how strong a guarantee can this machine
/// actually deliver, and what does this command line look like once the policy is applied to it.
///
/// **A backend never spawns.** It produces a command line and hands it back; the process is started
/// by the one place in this crate that starts processes, which is what keeps the scrubbed
/// environment, the resource ceilings and the process group from having a second implementation
/// that drifts.
pub trait SandboxBackend: fmt::Debug + Send + Sync {
    /// The name reported alongside the level in force.
    fn name(&self) -> &'static str;

    /// The strongest level this backend can deliver on this machine, or why it can deliver none.
    ///
    /// Answering per level rather than yes/no is what a later backend needs: a mechanism that can
    /// bound writes but not reads tops out at [`SandboxLevel::WorkspaceWrite`], and a host asking
    /// for [`SandboxLevel::Isolated`] has to hear that rather than be handed a weaker thing under
    /// the stronger name.
    ///
    /// # Errors
    ///
    /// Returns the reason the backend is unusable here — no binary, no kernel support, a
    /// misconfiguration — in a form a person can act on.
    fn available_level(&self) -> Result<SandboxLevel, SandboxUnavailable>;

    /// Wraps `command` so that running it enforces `request`.
    ///
    /// # Errors
    ///
    /// Returns [`SandboxError`] when the policy cannot be expressed — an unresolvable root, a level
    /// this backend does not implement — rather than emitting a command line that confines less
    /// than it was asked to.
    fn confine(
        &self,
        request: &ConfinementRequest,
        command: SandboxCommand,
    ) -> Result<SandboxCommand, SandboxError>;
}

/// Why a backend cannot be used on this machine.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{backend} is unavailable: {reason}")]
pub struct SandboxUnavailable {
    backend: &'static str,
    reason: String,
}

impl SandboxUnavailable {
    /// Records that `backend` cannot be used, and why.
    #[must_use]
    pub fn new(backend: &'static str, reason: impl Into<String>) -> Self {
        Self {
            backend,
            reason: reason.into(),
        }
    }

    /// The backend that reported itself unusable.
    #[must_use]
    pub const fn backend(&self) -> &'static str {
        self.backend
    }

    /// What is missing, in terms of something a person can fix.
    #[must_use]
    pub fn reason(&self) -> &str {
        &self.reason
    }
}

/// A confinement that could not be set up.
///
/// Every variant is a refusal to start the command. None of them is a warning: a caller that
/// believed it was confined and was not is the failure this whole module exists to prevent.
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    /// This build has no backend that can confine a process on this platform.
    #[error(
        "no sandbox backend for {platform}: {detail}. Build with the platform's backend feature, \
         or ask for the `unconfined` level if this host really wants no confinement"
    )]
    NoBackend {
        /// The platform that has nothing to offer.
        platform: &'static str,
        /// Which backends were compiled in, so the answer is not guesswork.
        detail: String,
    },
    /// The backend exists but cannot run here.
    #[error("sandbox backend cannot run here")]
    Unavailable {
        /// The backend's own account of what is missing.
        #[source]
        source: SandboxUnavailable,
    },
    /// The backend works but cannot deliver the level that was asked for.
    #[error(
        "sandbox backend `{backend}` delivers at most `{available}`, but `{requested}` was asked \
         for and no weaker level was authorised"
    )]
    LevelNotDeliverable {
        /// The backend that fell short.
        backend: &'static str,
        /// What was asked for.
        requested: SandboxLevel,
        /// The strongest thing on offer.
        available: SandboxLevel,
    },
    /// A downgrade was authorised, but not far enough to reach anything deliverable.
    #[error(
        "sandbox backend `{backend}` delivers at most `{available}`, below the `{floor}` this host \
         authorised"
    )]
    BelowAcceptedLevel {
        /// The backend that fell short.
        backend: &'static str,
        /// The strongest thing on offer.
        available: SandboxLevel,
        /// The weakest level the host said it would accept.
        floor: SandboxLevel,
    },
    /// The policy asks for two things that cannot both be true.
    #[error("sandbox policy is self-contradictory: {detail}")]
    Contradictory {
        /// Which pair of requests cannot hold together.
        detail: String,
    },
    /// A root in the policy could not be turned into a path a backend can enforce.
    #[error("sandbox root `{root}` cannot be used: {detail}")]
    UnusableRoot {
        /// The root as the host wrote it.
        root: PathBuf,
        /// Why it cannot be enforced.
        detail: String,
    },
}

/// What is actually in force for a command, once the policy has met the machine.
///
/// A host reads this to answer the only question that matters after asking for a sandbox: did I get
/// it. [`Self::downgraded_from`] is non-`None` exactly when the answer is "not quite, and here is
/// what you have instead".
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct Confinement {
    backend: Option<&'static dyn SandboxBackend>,
    level: SandboxLevel,
    network: NetworkAccess,
    downgraded_from: Option<SandboxLevel>,
    request: Option<ConfinementRequest>,
}

impl Confinement {
    /// The baseline: no backend, nothing confined, network as the host has it.
    #[must_use]
    pub const fn baseline() -> Self {
        Self {
            backend: None,
            level: SandboxLevel::Unconfined,
            network: NetworkAccess::Allowed,
            downgraded_from: None,
            request: None,
        }
    }

    /// The name of the backend enforcing this, or `None` when nothing is.
    #[must_use]
    pub fn backend(&self) -> Option<&'static str> {
        self.backend.map(SandboxBackend::name)
    }

    /// The level actually in force.
    #[must_use]
    pub const fn level(&self) -> SandboxLevel {
        self.level
    }

    /// Whether the command can reach the network.
    #[must_use]
    pub const fn network(&self) -> NetworkAccess {
        self.network
    }

    /// The level that was asked for, when it is not the one in force.
    #[must_use]
    pub const fn downgraded_from(&self) -> Option<SandboxLevel> {
        self.downgraded_from
    }

    /// What the backend was asked to enforce, or `None` when no backend is involved.
    ///
    /// Exposed because it is the only place the writable set can be read after the scratch
    /// directory has been folded into it, and "is the directory every command is pointed at
    /// actually writable" is a question worth being able to ask.
    #[must_use]
    pub const fn request(&self) -> Option<&ConfinementRequest> {
        self.request.as_ref()
    }

    /// Wraps a command so that running it enforces this confinement.
    ///
    /// At [`SandboxLevel::Unconfined`] the command is returned untouched, which is the honest
    /// translation of a level that confines nothing.
    ///
    /// # Errors
    ///
    /// Returns [`SandboxError`] when the backend cannot express the policy.
    pub fn apply(&self, command: SandboxCommand) -> Result<SandboxCommand, SandboxError> {
        let (Some(backend), Some(request)) = (self.backend, self.request.as_ref()) else {
            return Ok(command);
        };
        backend.confine(request, command)
    }
}

/// The backend this build can use on this platform, if any.
///
/// Selection is by platform, not by preference: a build with every feature on still has exactly one
/// mechanism that confines a process on the kernel it is running on. The other backends are
/// compiled — their policy translation is testable anywhere — but are never chosen here.
#[must_use]
pub fn platform_backend() -> Option<&'static dyn SandboxBackend> {
    #[cfg(all(feature = "seatbelt", target_os = "macos"))]
    {
        return Some(seatbelt::backend());
    }
    #[cfg(all(feature = "bwrap", target_os = "linux"))]
    {
        return Some(bwrap::backend());
    }
    #[allow(unreachable_code)]
    None
}

/// The backends this build contains, whether or not this platform can select one of them.
///
/// Public because the answer is not derivable outside this crate: the features belong to `ra-exec`,
/// so a caller asking `cfg!(feature = "seatbelt")` from its own crate would be reading its own
/// feature table and would quietly get `false` every time.
#[must_use]
pub fn compiled_backend_names() -> Vec<&'static str> {
    [
        cfg!(feature = "seatbelt").then_some("seatbelt"),
        cfg!(feature = "bwrap").then_some("bwrap"),
        cfg!(feature = "docker").then_some("docker"),
    ]
    .into_iter()
    .flatten()
    .collect()
}

/// Which backends this build contains, for an error that would otherwise be a guess.
fn compiled_backends() -> String {
    let names = compiled_backend_names();
    if names.is_empty() {
        "no backend features are enabled".to_owned()
    } else {
        format!("compiled backends: {}", names.join(", "))
    }
}

/// Chooses a backend for `policy` and works out what it will deliver.
///
/// `scratch_dir` is the run's own directory, added to the writable set because every confined
/// command is told to use it.
///
/// # Errors
///
/// Returns [`SandboxError`] when the requested guarantee cannot be delivered and no weaker one was
/// authorised. It never returns a weaker confinement than the caller asked for without saying so in
/// [`Confinement::downgraded_from`].
pub fn resolve_confinement(
    policy: &SandboxPolicy,
    scratch_dir: Option<&Path>,
) -> Result<Confinement, SandboxError> {
    resolve_confinement_with(platform_backend(), policy, scratch_dir)
}

/// The same, against a backend named by the caller instead of the platform's.
///
/// **This exists so the selection rules can be tested at all.** Every rule here — refuse rather
/// than downgrade, refuse a downgrade past the authorised floor, never restore the network on the
/// way down — is about a backend that cannot deliver what was asked for, and the backend on the
/// machine running the test is the one that can. Tested only through [`resolve_confinement`], the
/// rules would be exercised on exactly one shape of machine: whichever the test happened to run on.
pub fn resolve_confinement_with(
    backend: Option<&'static dyn SandboxBackend>,
    policy: &SandboxPolicy,
    scratch_dir: Option<&Path>,
) -> Result<Confinement, SandboxError> {
    if !policy.level().needs_backend() {
        if !policy.network().is_allowed() {
            return Err(SandboxError::Contradictory {
                detail: "the `unconfined` level confines nothing, so it cannot deny the network"
                    .to_owned(),
            });
        }
        return Ok(Confinement::baseline());
    }

    let Some(backend) = backend else {
        return Err(SandboxError::NoBackend {
            platform: std::env::consts::OS,
            detail: compiled_backends(),
        });
    };

    let available = backend
        .available_level()
        .map_err(|source| SandboxError::Unavailable { source })?;

    let level = if available >= policy.level() {
        policy.level()
    } else {
        let floor = policy
            .accept_level()
            .ok_or(SandboxError::LevelNotDeliverable {
                backend: backend.name(),
                requested: policy.level(),
                available,
            })?;
        if available < floor {
            return Err(SandboxError::BelowAcceptedLevel {
                backend: backend.name(),
                available,
                floor,
            });
        }
        available
    };

    // A downgrade may not quietly restore the network. `Unconfined` is the only level with no way
    // to deny it, so a policy that asked for denial and was offered that rung is refused even
    // though the host authorised the rung itself — the authorisation was about the filesystem.
    if !level.needs_backend() && !policy.network().is_allowed() {
        return Err(SandboxError::BelowAcceptedLevel {
            backend: backend.name(),
            available: level,
            floor: SandboxLevel::WorkspaceWrite,
        });
    }

    Ok(Confinement {
        backend: Some(backend),
        level,
        network: policy.network(),
        downgraded_from: (level != policy.level()).then_some(policy.level()),
        request: Some(ConfinementRequest {
            level,
            network: policy.network(),
            writable_roots: collect_roots(policy.writable_roots(), scratch_dir),
            readable_roots: collect_roots(policy.readable_roots(), None),
        }),
    })
}

/// Deduplicates roots while keeping the order the host wrote them in.
///
/// Order is kept because a generated policy is something people read and diff; a set would sort the
/// workspace root somewhere below a scratch directory for no reason anyone could see.
fn collect_roots(roots: &[PathBuf], extra: Option<&Path>) -> Vec<PathBuf> {
    let mut seen = BTreeSet::new();
    roots
        .iter()
        .map(PathBuf::as_path)
        .chain(extra)
        .filter(|root| seen.insert(root.to_path_buf()))
        .map(Path::to_path_buf)
        .collect()
}

/// What confined one command, recorded where the command's result is recorded.
///
/// # Why this travels with the result rather than only in a log
///
/// A downgrade is not an event, it is a property of the answer: a host reading "the command wrote
/// nothing outside the workspace" needs to know in the same breath whether anything was enforcing
/// that. Left in a log line, the two would have to be correlated by timestamp by whoever noticed to
/// ask — and nobody asks, which is how an unconfined run gets read as a confined one.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxReport {
    #[serde(default = "default_schema_version")]
    schema_version: SchemaVersion,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    backend: Option<String>,
    #[serde(default)]
    level: SandboxLevel,
    #[serde(default)]
    network: NetworkAccess,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    downgraded_from: Option<SandboxLevel>,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl SandboxReport {
    /// The backend that enforced this, or `None` when nothing did.
    #[must_use]
    pub fn backend(&self) -> Option<&str> {
        self.backend.as_deref()
    }

    /// The level in force.
    #[must_use]
    pub const fn level(&self) -> SandboxLevel {
        self.level
    }

    /// Whether the command could reach the network.
    #[must_use]
    pub const fn network(&self) -> NetworkAccess {
        self.network
    }

    /// The level that was asked for, when it is not the one that was delivered.
    #[must_use]
    pub const fn downgraded_from(&self) -> Option<SandboxLevel> {
        self.downgraded_from
    }

    /// Whether this command ran more weakly confined than its host asked for.
    #[must_use]
    pub const fn is_downgraded(&self) -> bool {
        self.downgraded_from.is_some()
    }

    /// Schema version of this record.
    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    /// Unknown fields preserved during forward-compatible deserialization.
    #[must_use]
    pub const fn unknown(&self) -> &Unknown {
        &self.unknown
    }
}

impl Confinement {
    /// The record of this confinement, as it is kept with a command's result.
    #[must_use]
    pub fn report(&self) -> SandboxReport {
        SandboxReport {
            schema_version: EXEC_SCHEMA_VERSION,
            backend: self.backend().map(str::to_owned),
            level: self.level,
            network: self.network,
            downgraded_from: self.downgraded_from,
            unknown: Unknown::new(),
        }
    }
}
