//! macOS `sandbox-exec`: a Seatbelt profile assembled from the policy, and the command line that
//! runs under it.
//!
//! # What the profile is made of
//!
//! Four sections, composed in order, following how codex's `sandboxing/src/seatbelt.rs` builds
//! one — a base policy, a read policy, a write policy and a network policy, each generated
//! separately and joined:
//!
//! - **base**, always present: `(deny default)`, then the handful of operations without which no
//!   program starts — exec, fork, signalling its own sandbox, the standard devices.
//! - **read**, which is the only section the level changes.
//! - **write**, the declared writable roots and nothing else.
//! - **network**, present only when the policy allows the network; denial needs no rule, because
//!   the base already denies everything not named.
//!
//! The paths themselves are passed as `-D` parameters rather than interpolated into the profile
//! text. A path with a `"` or a `\` in it would otherwise end the string it was pasted into, and a
//! policy that a path can rewrite is not a policy.
//!
//! # What was found by running it, not by reading it
//!
//! Three rules here exist because the profile did not work without them, and each is the kind of
//! thing no amount of reading the documentation would have produced:
//!
//! - **`(literal "/")`** — without read access to the root directory *itself*, every binary dies
//!   with `SIGABRT` before `main`, even with every top-level directory granted individually. The
//!   loader resolves through `/`.
//! - **top-level aliases are resolved before the path is used** — Seatbelt matches the path the
//!   kernel resolved to, so a writable root of `/tmp/x` grants nothing: the kernel sees
//!   `/private/tmp/x`. Deeper components are deliberately *not* resolved; see [`normalize_root`].
//! - **`/tmp`, `/etc`, `/var` and `/private` need metadata read** even when the roots beneath them
//!   are granted, or a command that names a path through one of those aliases cannot traverse it.

use std::{
    fmt::Write as _,
    path::{Component, Path, PathBuf},
    sync::OnceLock,
};

use crate::sandbox::{
    ConfinementRequest, SandboxBackend, SandboxCommand, SandboxError, SandboxLevel,
    SandboxUnavailable,
};

/// This backend's name in reports.
pub const BACKEND_NAME: &str = "seatbelt";

/// Only `/usr/bin/sandbox-exec` is ever used, never a `sandbox-exec` found on `PATH`.
///
/// The command about to run is arbitrary text from a model, and `PATH` is derived from the host's
/// own environment policy; looking the sandbox up there would let whatever wrote that `PATH` choose
/// the program that enforces the sandbox. A machine where `/usr/bin/sandbox-exec` itself has been
/// replaced is already lost for reasons this check could not have helped with.
pub const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

/// The platform's own runtime, readable at every level because nothing runs without it.
///
/// This is the macOS equivalent of the `:minimal` platform-default set codex keeps in
/// `seatbelt_read_only_platform_defaults.sbpl`. It is deliberately much shorter: this list is the
/// one this repository has run a shell, `git`, `grep` and a language runtime under, and every entry
/// is a system path rather than anything belonging to a user.
const PLATFORM_READ_ROOTS: &[&str] = &[
    "/bin",
    "/sbin",
    "/usr",
    "/System",
    "/Library/Apple",
    "/private/etc",
    "/private/var/select",
    "/private/var/db/timezone",
    "/opt/homebrew/lib",
    "/usr/local/lib",
];

/// Top-level symlinks a path may be written through, needing metadata read to be traversed.
const SYSTEM_ALIASES: &[&str] = &["/etc", "/tmp", "/var", "/private"];

/// The mach services a name lookup needs before a network client works.
///
/// Only consulted when the policy allows the network. The list is the same shape as the one in
/// codex's `seatbelt_network_policy.sbpl`: resolver configuration and the trust daemon, without
/// which DNS and TLS fail while the sockets themselves are open.
const NETWORK_MACH_SERVICES: &[&str] = &[
    "com.apple.SystemConfiguration.DNSConfiguration",
    "com.apple.SystemConfiguration.configd",
    "com.apple.SecurityServer",
    "com.apple.trustd",
    "com.apple.trustd.agent",
    "com.apple.networkd",
    "com.apple.ocspd",
    "com.apple.bsd.dirhelper",
    "com.apple.system.opendirectoryd.membership",
];

/// The always-present half of the profile.
const BASE_POLICY: &str = r#"(version 1)

; Closed by default: everything below is an exception someone had to name.
(deny default)

; A child inherits this policy, so a shell must be able to start and signal what it starts.
(allow process-exec)
(allow process-fork)
(allow signal (target same-sandbox))
(allow process-info* (target same-sandbox))

; Machine facts a language runtime reads before it can run anything.
(allow sysctl-read)
(allow mach-lookup (global-name "com.apple.system.opendirectoryd.libinfo"))

; The standard devices, and the descriptors a piped or interactive command writes through.
(allow file-read* file-write-data file-ioctl file-test-existence
  (literal "/dev/null")
  (literal "/dev/zero")
  (literal "/dev/random")
  (literal "/dev/urandom")
  (literal "/dev/dtracehelper"))
(allow file-read-data file-write-data file-test-existence (subpath "/dev/fd"))
(allow file-read* file-write* file-ioctl
  (literal "/dev/tty")
  (literal "/dev/ptmx")
  (regex #"^/dev/ttys[0-9]+$"))"#;

/// A generated profile and the path parameters it refers to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeatbeltProfile {
    policy: String,
    params: Vec<(String, PathBuf)>,
}

impl SeatbeltProfile {
    /// The profile text, as `sandbox-exec -p` receives it.
    #[must_use]
    pub fn policy(&self) -> &str {
        &self.policy
    }

    /// The `-D` parameters the profile refers to, in the order they are passed.
    #[must_use]
    pub fn params(&self) -> &[(String, PathBuf)] {
        &self.params
    }
}

/// Builds the profile for one confinement request.
///
/// # Errors
///
/// Returns [`SandboxError::UnusableRoot`] for a root that cannot be turned into a path the kernel
/// will match: a relative one, or one reached through a symlinked directory.
pub fn build_profile(request: &ConfinementRequest) -> Result<SeatbeltProfile, SandboxError> {
    let mut policy = String::from(BASE_POLICY);
    let mut params = Vec::new();

    policy.push_str("\n\n; Reads.\n");
    match request.level() {
        SandboxLevel::Isolated => {
            policy.push_str(
                "; The platform's own runtime, plus the roots this policy declared. The root \
                 directory\n; itself is on the list because the loader resolves through it and \
                 nothing starts without it.\n(allow file-read* file-test-existence \
                 file-map-executable\n  (literal \"/\")",
            );
            for root in PLATFORM_READ_ROOTS {
                let _ = write!(policy, "\n  (subpath \"{root}\")");
            }
            policy.push_str(")\n");
            policy.push_str("; Top-level aliases a path may be written through.\n(allow file-read-metadata file-test-existence");
            for alias in SYSTEM_ALIASES {
                let _ = write!(policy, "\n  (literal \"{alias}\")");
            }
            policy.push_str(")\n");
            for (index, root) in request.readable_roots().iter().enumerate() {
                let key = format!("READABLE_ROOT_{index}");
                params.push((key.clone(), normalize_root(root)?));
                let _ = write!(
                    policy,
                    "(allow file-read-metadata file-test-existence (path-ancestors (param \"{key}\")))\n\
                     (allow file-read* file-test-existence file-map-executable (subpath (param \"{key}\")))\n"
                );
            }
        }
        SandboxLevel::WorkspaceWrite => {
            policy.push_str(
                "; Reads are not confined at this level; what a model-written command can change \
                 is.\n(allow file-read* file-test-existence file-map-executable (subpath \"/\"))\n",
            );
        }
        SandboxLevel::Unconfined => {
            return Err(SandboxError::Contradictory {
                detail: "the `unconfined` level does not go through a backend".to_owned(),
            });
        }
    }

    policy.push_str("\n; Writes: only the roots this policy declared.\n");
    for (index, root) in request.writable_roots().iter().enumerate() {
        let key = format!("WRITABLE_ROOT_{index}");
        params.push((key.clone(), normalize_root(root)?));
        // A writable root is readable too, at every level: a command that may change a file it
        // cannot read is a combination no caller has ever wanted and every editor breaks on.
        let _ = write!(
            policy,
            "(allow file-read-metadata file-test-existence (path-ancestors (param \"{key}\")))\n\
             (allow file-read* file-test-existence file-map-executable file-write* (subpath (param \"{key}\")))\n"
        );
    }

    if request.network().is_allowed() {
        policy.push_str("\n; Network, because the policy allows it.\n");
        policy
            .push_str("(allow network-outbound)\n(allow network-inbound)\n(allow system-socket)\n");
        policy.push_str("(allow mach-lookup");
        for service in NETWORK_MACH_SERVICES {
            let _ = write!(policy, "\n  (global-name \"{service}\")");
        }
        policy.push_str(")\n");
    }

    Ok(SeatbeltProfile { policy, params })
}

/// Turns a declared root into the path the kernel will match against.
///
/// **Only the top-level component is resolved.** `/tmp` and `/var` are symlinks on macOS, and a
/// root under either matches nothing unless it is rewritten — that part is forced. Resolving the
/// rest is not: a deeper component can be replaced by a process already running inside the sandbox,
/// so following it would turn a path the host wrote into a path the sandboxed command chose. Codex
/// draws the line in the same place, and refuses a writable root with a symlinked component rather
/// than following it; so does this.
///
/// # Errors
///
/// Returns [`SandboxError::UnusableRoot`] for a relative root or one whose non-top-level ancestors
/// include a symlink.
pub fn normalize_root(root: &Path) -> Result<PathBuf, SandboxError> {
    if !root.is_absolute() {
        return Err(SandboxError::UnusableRoot {
            root: root.to_path_buf(),
            detail: "a sandbox root must be absolute; a relative one would be resolved against \
                     whatever directory the process happened to be in"
                .to_owned(),
        });
    }
    if root
        .components()
        .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
    {
        return Err(SandboxError::UnusableRoot {
            root: root.to_path_buf(),
            detail: "a sandbox root may not contain `.` or `..`: the kernel matches the resolved \
                     path, so the rule would not cover what the caller wrote"
                .to_owned(),
        });
    }

    let mut resolved = PathBuf::from("/");
    for (index, component) in root.components().skip(1).enumerate() {
        resolved.push(component);
        let Ok(metadata) = std::fs::symlink_metadata(&resolved) else {
            // A root that does not exist yet is legitimate — a scratch directory is created inside
            // the command — and nothing beneath a missing path can be a symlink.
            continue;
        };
        if !metadata.file_type().is_symlink() {
            continue;
        }
        if index > 0 {
            return Err(SandboxError::UnusableRoot {
                root: root.to_path_buf(),
                detail: format!(
                    "`{}` is a symlink; following it would grant access to wherever it points, \
                     which a process inside the sandbox can change",
                    resolved.display()
                ),
            });
        }
        resolved = resolved
            .canonicalize()
            .map_err(|error| SandboxError::UnusableRoot {
                root: root.to_path_buf(),
                detail: format!(
                    "cannot resolve the top-level alias `{}`: {error}",
                    resolved.display()
                ),
            })?;
    }
    Ok(resolved)
}

/// The command line that runs `command` under `profile`.
///
/// The profile travels in `-p` rather than in a file: a temporary file would be one more thing to
/// create before every command, to clean up after a crash, and to keep another process from
/// swapping between the write and the read.
#[must_use]
pub fn command_args(profile: &SeatbeltProfile, command: SandboxCommand) -> SandboxCommand {
    let mut args = vec!["-p".to_owned(), profile.policy().to_owned()];
    for (key, value) in profile.params() {
        args.push(format!("-D{key}={}", value.display()));
    }
    args.push("--".to_owned());
    let (program, program_args) = command.into_parts();
    args.push(program.to_string_lossy().into_owned());
    args.extend(program_args);
    SandboxCommand::new(SANDBOX_EXEC, args)
}

/// The macOS backend.
#[derive(Debug, Clone, Copy, Default)]
pub struct SeatbeltBackend;

/// The process-wide instance.
///
/// Availability is probed once: whether `/usr/bin/sandbox-exec` exists does not change while a run
/// is in flight, and probing per command would put a `stat` in front of every spawn.
#[must_use]
pub fn backend() -> &'static SeatbeltBackend {
    &SeatbeltBackend
}

impl SandboxBackend for SeatbeltBackend {
    fn name(&self) -> &'static str {
        BACKEND_NAME
    }

    fn available_level(&self) -> Result<SandboxLevel, SandboxUnavailable> {
        static AVAILABLE: OnceLock<Result<SandboxLevel, SandboxUnavailable>> = OnceLock::new();
        AVAILABLE
            .get_or_init(|| {
                if !cfg!(target_os = "macos") {
                    return Err(SandboxUnavailable::new(
                        BACKEND_NAME,
                        "Seatbelt exists only on macOS; this build can generate its profiles but \
                         cannot enforce them here",
                    ));
                }
                if !Path::new(SANDBOX_EXEC).is_file() {
                    return Err(SandboxUnavailable::new(
                        BACKEND_NAME,
                        format!("`{SANDBOX_EXEC}` is not present on this machine"),
                    ));
                }
                Ok(SandboxLevel::Isolated)
            })
            .clone()
    }

    fn confine(
        &self,
        request: &ConfinementRequest,
        command: SandboxCommand,
    ) -> Result<SandboxCommand, SandboxError> {
        let profile = build_profile(request)?;
        Ok(command_args(&profile, command))
    }
}
