//! Baseline backend: initial working directory, environment scrubbing, resource ceilings.
//!
//! # What this is not
//!
//! **It is not isolation, and nothing here should be read as a path restriction.** The working
//! directory a process starts in is a starting point, not a boundary: the first `cd ..` leaves it,
//! and so does any absolute path the command names. [`crate::fs::Workspace`] does confine paths,
//! but it confines *the file tools that go through it* — a spawned process never does. A real path
//! boundary is the business of the platform backends, and on a host that has none, construction
//! fails rather than quietly landing here (see the module docs one level up).
//!
//! In codex's vocabulary this is the `SandboxType::None` case: the execution did not go through a
//! platform sandbox layer. It does not say the machine has no sandbox available.
//!
//! # What it does give
//!
//! Two things, both worth having on their own:
//!
//! - an environment the child inherits **on purpose** rather than by default, with explicit filters
//!   and a best-effort credential-name heuristic; this is not a secret-exfiltration guarantee;
//! - ceilings that bound what a single runaway command costs, so a fork-free infinite loop or a
//!   log that never stops growing hits a limit instead of the machine.

use std::{collections::BTreeMap, fmt};

use ra_core::compat::{SchemaVersion, Unknown};
use serde::{Deserialize, Serialize};

use crate::EXEC_SCHEMA_VERSION;

const fn default_schema_version() -> SchemaVersion {
    EXEC_SCHEMA_VERSION
}

const fn default_true() -> bool {
    true
}

/// Variables a child starts with under [`EnvInherit::Core`].
///
/// The list is deliberately small and boring: what a program needs to find its interpreter, its
/// home, and its locale. Anything a specific toolchain wants — `CARGO_HOME`, `NVM_DIR`, a proxy,
/// an ssh agent socket — is the host's to add, because only the host knows which toolchains its
/// product supports.
const CORE_ENV_VARS: &[&str] = &[
    "HOME", "LANG", "LC_ALL", "LC_CTYPE", "LOGNAME", "PATH", "SHELL", "TMPDIR", "USER",
];

/// Name fragments that make a variable look like a credential.
///
/// Substring matching on these fragments is a heuristic and is documented as one: it catches
/// `AWS_SECRET_ACCESS_KEY` and `GITHUB_TOKEN`, and it does not catch `DATABASE_URL` with a password
/// in it. It is a floor, not a guarantee — a host that knows its own secrets names them in
/// [`EnvPolicy::exclude`].
const CREDENTIAL_LIKE_FRAGMENTS: &[&str] = &["KEY", "SECRET", "TOKEN", "PASSWORD", "CREDENTIAL"];

/// Where a child's environment starts before the policy's filters run.
#[non_exhaustive]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvInherit {
    /// Start from this process's full environment.
    ///
    /// The default, and the one that keeps arbitrary toolchains working. It is paired with
    /// [`EnvPolicy::exclude_credential_like`] being on, so "inherit everything" still does not mean
    /// "hand over every API key in the shell that launched us".
    #[default]
    All,
    /// Start from [`CORE_ENV_VARS`] only.
    Core,
    /// Start from nothing.
    ///
    /// Note that a shell with no `PATH` finds almost no programs, so this is for hosts that then
    /// populate [`EnvPolicy::set`] themselves.
    None,
}

/// A case-insensitive environment-variable name pattern.
///
/// `*` matches any run of characters, including an empty one; every other character matches
/// itself. That is the whole language — it is enough for `AWS_*` and `*_TOKEN`, and a pattern
/// dialect with escapes and character classes would be a second thing to learn for no case anyone
/// has yet had.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EnvPattern(String);

impl EnvPattern {
    /// Creates a pattern from its source text.
    #[must_use]
    pub fn new(pattern: impl Into<String>) -> Self {
        Self(pattern.into())
    }

    /// The pattern's source text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether `name` matches, comparing without case.
    #[must_use]
    pub fn matches(&self, name: &str) -> bool {
        let pattern = self.0.to_ascii_uppercase();
        let name = name.to_ascii_uppercase();
        glob_matches(&pattern, &name)
    }
}

impl fmt::Display for EnvPattern {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Matches `name` against a `*`-only glob, both already folded to one case.
///
/// Written as a two-pointer scan with backtracking rather than a regex so the crate does not grow a
/// regex dependency for nine characters of pattern language. The backtrack point is the last `*`
/// seen, which is what makes `A*B*C` work without recursion.
fn glob_matches(pattern: &str, name: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let name: Vec<char> = name.chars().collect();
    let (mut p, mut n) = (0_usize, 0_usize);
    let (mut star, mut resume) = (None, 0_usize);

    while n < name.len() {
        if p < pattern.len() && (pattern[p] == name[n]) {
            p += 1;
            n += 1;
        } else if p < pattern.len() && pattern[p] == '*' {
            star = Some(p);
            resume = n;
            p += 1;
        } else if let Some(star) = star {
            // The last `*` swallows one more character and the scan restarts after it.
            p = star + 1;
            resume += 1;
            n = resume;
        } else {
            return false;
        }
    }

    pattern[p..].iter().all(|c| *c == '*')
}

/// How a child process's environment is derived from this one's.
///
/// The derivation follows codex's `ShellEnvironmentPolicy` (`protocol/src/shell_environment.rs`)
/// step for step, because the shape has been through real toolchains and the order of the steps is
/// the part that is easy to get subtly wrong. Two deliberate differences:
///
/// - **the credential filter is named for what it does and defaults to on.** Upstream spells it
///   `ignore_default_excludes` and defaults it to `true`, so upstream's default environment carries
///   every key in the launching shell. A double negative whose default is "off" is a poor way to
///   describe a safety filter, and a framework that hands arbitrary model-written commands to a
///   shell should not inherit that default.
/// - **[`Self::set`] is applied after [`Self::include_only`]**, not before. Upstream lets an
///   `include_only` list filter out a variable the same policy just set, which reads as a bug
///   waiting for a bug report. Values named here are what the host decided this process should see.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvPolicy {
    #[serde(default = "default_schema_version")]
    schema_version: SchemaVersion,
    #[serde(default)]
    inherit: EnvInherit,
    #[serde(default = "default_true")]
    exclude_credential_like: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    exclude: Vec<EnvPattern>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    include_only: Vec<EnvPattern>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    set: BTreeMap<String, String>,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl Default for EnvPolicy {
    fn default() -> Self {
        Self {
            schema_version: EXEC_SCHEMA_VERSION,
            inherit: EnvInherit::All,
            exclude_credential_like: true,
            exclude: Vec::new(),
            include_only: Vec::new(),
            set: BTreeMap::new(),
            unknown: Unknown::new(),
        }
    }
}

impl EnvPolicy {
    /// Creates the default policy: inherit everything except what looks like a credential.
    ///
    /// This is a mechanism default, not a product ruling. It is chosen so that a host which
    /// configures nothing filters credential-looking names before launching a shell, while
    /// arbitrary toolchains keep working. A product with a tighter answer says so with
    /// [`EnvInherit::Core`] and its own allowlist.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets where the environment starts.
    #[must_use]
    pub const fn with_inherit(mut self, inherit: EnvInherit) -> Self {
        self.inherit = inherit;
        self
    }

    /// Enables or disables the credential-name heuristic.
    #[must_use]
    pub const fn with_exclude_credential_like(mut self, exclude: bool) -> Self {
        self.exclude_credential_like = exclude;
        self
    }

    /// Adds name patterns to drop.
    #[must_use]
    pub fn with_exclude(mut self, patterns: impl IntoIterator<Item = EnvPattern>) -> Self {
        self.exclude.extend(patterns);
        self
    }

    /// Restricts the result to names matching these patterns.
    #[must_use]
    pub fn with_include_only(mut self, patterns: impl IntoIterator<Item = EnvPattern>) -> Self {
        self.include_only.extend(patterns);
        self
    }

    /// Sets a variable the child receives regardless of what was inherited or filtered.
    #[must_use]
    pub fn with_set(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.set.insert(key.into(), value.into());
        self
    }

    /// Where the environment starts.
    #[must_use]
    pub const fn inherit(&self) -> EnvInherit {
        self.inherit
    }

    /// Whether credential-looking names are dropped.
    #[must_use]
    pub const fn exclude_credential_like(&self) -> bool {
        self.exclude_credential_like
    }

    /// Name patterns dropped from the environment.
    #[must_use]
    pub fn exclude(&self) -> &[EnvPattern] {
        &self.exclude
    }

    /// Name patterns the environment is restricted to, when any are set.
    #[must_use]
    pub fn include_only(&self) -> &[EnvPattern] {
        &self.include_only
    }

    /// Variables set unconditionally.
    #[must_use]
    pub const fn set(&self) -> &BTreeMap<String, String> {
        &self.set
    }

    /// Unknown fields preserved during forward-compatible deserialization.
    #[must_use]
    pub const fn unknown(&self) -> &Unknown {
        &self.unknown
    }

    /// Derives the environment a child should receive from this process's own.
    ///
    /// The caller is expected to clear the child's inherited environment and install exactly this
    /// map; handing it to a command that still inherits would defeat every filter below.
    /// Non-Unicode names or values are omitted; this text policy never panics on Unix byte values.
    #[must_use]
    pub fn derive(&self) -> BTreeMap<String, String> {
        self.derive_from(
            std::env::vars_os().filter_map(|(key, value)| {
                Some((key.into_string().ok()?, value.into_string().ok()?))
            }),
        )
    }

    /// Derives a child environment from an explicit set of variables.
    ///
    /// Separate from [`Self::derive`] so the derivation is testable without a process-wide
    /// environment, which no test can mutate safely while its neighbours run.
    #[must_use]
    pub fn derive_from(
        &self,
        vars: impl IntoIterator<Item = (String, String)>,
    ) -> BTreeMap<String, String> {
        // 1. The starting set.
        let mut env: BTreeMap<String, String> = match self.inherit {
            EnvInherit::All => vars.into_iter().collect(),
            EnvInherit::None => BTreeMap::new(),
            EnvInherit::Core => vars
                .into_iter()
                .filter(|(name, _)| {
                    CORE_ENV_VARS
                        .iter()
                        .any(|core| core.eq_ignore_ascii_case(name))
                })
                .collect(),
        };

        // 2. The credential heuristic.
        if self.exclude_credential_like {
            env.retain(|name, _| !is_credential_like(name));
        }

        // 3. The host's own patterns.
        if !self.exclude.is_empty() {
            env.retain(|name, _| !self.exclude.iter().any(|pattern| pattern.matches(name)));
        }

        // 4. The allowlist, when one is configured.
        if !self.include_only.is_empty() {
            env.retain(|name, _| self.include_only.iter().any(|p| p.matches(name)));
        }

        // 5. Explicit values last, so they survive every filter above. See the type docs for why
        //    this runs after the allowlist rather than before it.
        for (key, value) in &self.set {
            env.insert(key.clone(), value.clone());
        }

        env
    }
}

/// Whether a variable name looks like it carries a credential.
fn is_credential_like(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    CREDENTIAL_LIKE_FRAGMENTS
        .iter()
        .any(|fragment| upper.contains(fragment))
}

/// Ceilings applied to a single spawned process.
///
/// # Which limits, and why not the others
///
/// Each one here is per-process, has the same meaning on Linux and macOS, and fails in a way the
/// caller can read: the command dies with a signal or a write returns an error, and the exit status
/// says so.
///
/// Two obvious candidates are deliberately absent:
///
/// - **address space** (`RLIMIT_AS`) is honoured inconsistently. macOS largely ignores it for
///   `mmap`-backed allocation, so the same configuration would bound memory on one platform and be
///   decorative on the other. A ceiling that silently does nothing on half the fleet is worse than
///   no ceiling, because it gets trusted.
/// - **process count** (`RLIMIT_NPROC`) is per-*user*, not per-process tree. Setting it counts
///   every process the host's own user already has, so a low value fails to spawn for reasons that
///   have nothing to do with this command, and a value low enough to stop a fork bomb can leave the
///   user unable to start a shell. Bounding process creation needs a cgroup or a job object, which
///   is a platform backend's job.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceLimits {
    #[serde(default = "default_schema_version")]
    schema_version: SchemaVersion,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cpu_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    file_size_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    open_files: Option<u64>,
    #[serde(
        default = "default_no_core_dumps",
        skip_serializing_if = "Option::is_none"
    )]
    core_dump_bytes: Option<u64>,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

#[allow(clippy::unnecessary_wraps)]
const fn default_no_core_dumps() -> Option<u64> {
    Some(0)
}

impl Default for ResourceLimits {
    fn default() -> Self {
        Self {
            schema_version: EXEC_SCHEMA_VERSION,
            cpu_seconds: None,
            file_size_bytes: None,
            open_files: None,
            core_dump_bytes: default_no_core_dumps(),
            unknown: Unknown::new(),
        }
    }
}

impl ResourceLimits {
    /// Creates the default ceilings: no core dumps, and nothing else bounded.
    ///
    /// Core dumps are off by default to avoid persisting process memory, at the cost of losing
    /// crash dumps for debugging: a
    /// crashing command writes a multi-gigabyte image of its address space into the working
    /// directory, and that image contains whatever the process had in memory. Every other ceiling
    /// needs a number only the host can pick, and a wrong guess here kills legitimate builds.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Bounds CPU time. The process is signalled once it is exceeded.
    #[must_use]
    pub const fn with_cpu_seconds(mut self, seconds: Option<u64>) -> Self {
        self.cpu_seconds = seconds;
        self
    }

    /// Bounds the size of any single file the process writes.
    #[must_use]
    pub const fn with_file_size_bytes(mut self, bytes: Option<u64>) -> Self {
        self.file_size_bytes = bytes;
        self
    }

    /// Bounds how many file descriptors the process may hold open.
    #[must_use]
    pub const fn with_open_files(mut self, count: Option<u64>) -> Self {
        self.open_files = count;
        self
    }

    /// Bounds core dump size; `Some(0)` disables dumps entirely.
    #[must_use]
    pub const fn with_core_dump_bytes(mut self, bytes: Option<u64>) -> Self {
        self.core_dump_bytes = bytes;
        self
    }

    /// CPU-time ceiling in seconds.
    #[must_use]
    pub const fn cpu_seconds(&self) -> Option<u64> {
        self.cpu_seconds
    }

    /// Single-file size ceiling in bytes.
    #[must_use]
    pub const fn file_size_bytes(&self) -> Option<u64> {
        self.file_size_bytes
    }

    /// Open descriptor ceiling.
    #[must_use]
    pub const fn open_files(&self) -> Option<u64> {
        self.open_files
    }

    /// Core dump ceiling in bytes.
    #[must_use]
    pub const fn core_dump_bytes(&self) -> Option<u64> {
        self.core_dump_bytes
    }

    /// Unknown fields preserved during forward-compatible deserialization.
    #[must_use]
    pub const fn unknown(&self) -> &Unknown {
        &self.unknown
    }

    /// Whether any ceiling is set, so a caller can skip installing a no-op hook.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.cpu_seconds.is_none()
            && self.file_size_bytes.is_none()
            && self.open_files.is_none()
            && self.core_dump_bytes.is_none()
    }

    /// Applies every configured ceiling to the calling process.
    ///
    /// Meant to run between `fork` and `exec`, where "the calling process" is the child and nothing
    /// else is affected. Both the soft and the hard limit are set to the requested value, capped by
    /// the hard limit the child inherited — so the value may sit above the inherited *soft* limit,
    /// but never above the inherited hard one.
    ///
    /// Setting the hard limit too is what makes the ceiling a ceiling: an unprivileged process may
    /// lower a hard limit but never raise it, so the child cannot climb back out with `ulimit`. The
    /// price is that a program which lowers a limit and then restores it — a real, if uncommon,
    /// pattern in build tools — finds it cannot restore.
    ///
    /// # Errors
    ///
    /// Returns the first `setrlimit` failure. A caller in a `pre_exec` hook must turn that into a
    /// failed spawn rather than continuing, or the command would run without the ceiling its caller
    /// asked for and nobody would hear about it.
    #[cfg(unix)]
    pub fn apply_to_current_process(&self) -> std::io::Result<()> {
        use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};

        let apply = |resource: Resource, value: u64| -> std::io::Result<()> {
            let current = getrlimit(resource);
            // The configured value is what the child gets, capped by the hard limit already in
            // force. That cap is not decoration: a soft limit above the hard one is refused by the
            // kernel, so taking the minimum turns "asked for more than this machine allows" into
            // "got what it allows" rather than into a failed spawn.
            //
            // Note this *sets* rather than only tightens — a value above the inherited soft limit
            // raises it, up to the hard one, which is what `ulimit -n 1024` does in any shell.
            // What cannot be exceeded is the hard limit, and that is the ceiling being described.
            let maximum = current.maximum;
            let soft = maximum.map_or(value, |hard| value.min(hard));
            setrlimit(
                resource,
                Rlimit {
                    current: Some(soft),
                    maximum: Some(soft),
                },
            )
            .map_err(|errno| std::io::Error::from_raw_os_error(errno.raw_os_error()))
        };

        if let Some(seconds) = self.cpu_seconds {
            apply(Resource::Cpu, seconds)?;
        }
        if let Some(bytes) = self.file_size_bytes {
            apply(Resource::Fsize, bytes)?;
        }
        if let Some(count) = self.open_files {
            apply(Resource::Nofile, count)?;
        }
        if let Some(bytes) = self.core_dump_bytes {
            apply(Resource::Core, bytes)?;
        }
        Ok(())
    }
}
