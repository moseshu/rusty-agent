//! Linux bubblewrap: a mount namespace built from the policy, and the command line that runs in it.
//!
//! # The boundary is the namespace, not a list of paths
//!
//! Seatbelt answers "may this process touch that path"; bubblewrap answers "does that path exist at
//! all". Everything the policy did not grant is simply not in the mount namespace the command runs
//! in, which is why the arguments below read as a filesystem being assembled rather than as rules:
//! bind the world read-only, then bind the writable roots over it, and what was never bound is
//! unreachable by any name.
//!
//! `--unshare-net` isolates the IP stack and abstract Unix socket namespace. Pathname Unix
//! sockets remain reachable through bind mounts, so network denial also requires a syscall filter.
//!
//! # Seccomp complements namespaces
//!
//! A network namespace does not hide pathname Unix sockets in bind-mounted directories.
//! Every launch therefore carries a seccomp filter through bubblewrap's `--seccomp` descriptor.
//! With network denied it rejects socket creation and communication, including pathname sockets.
//! At both network settings it rejects ptrace, cross-process memory access and `io_uring`, so those
//! alternate interfaces cannot bypass the syscall restrictions. Like the upstream Linux sandbox,
//! this is a syscall denylist. A strict syscall allowlist is not implemented here.
//! Filter creation, transport or installation failure prevents the requested command from running.
//!
//! # What compiles where
//!
//! Translating a policy into arguments is portable and is compiled and tested everywhere. Finding
//! the binary and probing whether this kernel will grant a user namespace only mean something on
//! Linux, and on any other platform the backend reports itself unavailable — so
//! [`crate::sandbox::platform_backend`] never hands it to a host that could not be confined by it.

#[cfg(any(target_os = "linux", all(unix, feature = "test-api")))]
use std::collections::BTreeSet;
use std::{
    path::{Component, Path, PathBuf},
    sync::OnceLock,
};

#[cfg(any(target_os = "linux", all(unix, feature = "test-api")))]
mod daemon_mounts;
#[cfg(target_os = "linux")]
pub(crate) mod seccomp;

use crate::sandbox::{
    ConfinementRequest, SandboxBackend, SandboxCommand, SandboxError, SandboxLevel,
    SandboxUnavailable,
};

/// This backend's name in reports.
pub const BACKEND_NAME: &str = "bwrap";

/// The program looked up on `PATH`.
pub const BWRAP_PROGRAM: &str = "bwrap";

/// The platform's own runtime, bound read-only at every level because nothing runs without it.
///
/// The list is codex's `LINUX_PLATFORM_DEFAULT_READ_ROOTS` (`linux-sandbox/src/bwrap.rs`), for the
/// reason its comment gives: system paths only, plus the Nix store roots, so that asking for the
/// platform defaults cannot quietly widen access to anyone's data. Entries are bound with
/// `--ro-bind-try`, since a given distribution has only some of them.
const PLATFORM_READ_ROOTS: &[&str] = &[
    "/bin",
    "/sbin",
    "/usr",
    "/etc",
    "/lib",
    "/lib64",
    "/nix/store",
    "/run/current-system/sw",
];

/// How long the user-namespace probe is given before it is treated as inconclusive.
#[cfg(target_os = "linux")]
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);

/// Builds the bubblewrap arguments for one confinement request.
///
/// The order is the order bubblewrap applies them in, and it is load-bearing: the read-only world
/// goes down first, then the kernel filesystems over it, then a private `/tmp`, and only then the
/// writable roots — each later mount covering the earlier one at that path. A writable root listed
/// before `--ro-bind / /` would be buried by it.
///
/// # Errors
///
/// Returns [`SandboxError::UnusableRoot`] for a root that is not an absolute, literal path.
pub fn build_args(request: &ConfinementRequest) -> Result<Vec<String>, SandboxError> {
    fn push(args: &mut Vec<String>, values: &[&str]) {
        args.extend(values.iter().map(|value| (*value).to_owned()));
    }

    let mut args: Vec<String> = Vec::new();

    // The sandbox dies with the process that started it, and the command gets its own session so
    // that it cannot push characters back into a terminal it inherited. The session also means the
    // tracked process group is bubblewrap's own; nothing inside can leave the PID namespace, which
    // is the escape the baseline backend documents as out of its reach.
    push(&mut args, &["--die-with-parent", "--new-session"]);
    push(
        &mut args,
        &[
            "--unshare-pid",
            "--unshare-ipc",
            "--unshare-uts",
            "--unshare-cgroup-try",
        ],
    );
    // Left to bubblewrap: it unshares the user namespace itself when it is not installed setuid,
    // and forcing the flag here would break the setuid installations that some distributions still
    // ship.
    if !request.network().is_allowed() {
        push(&mut args, &["--unshare-net"]);
    }

    match request.level() {
        SandboxLevel::WorkspaceWrite => push(&mut args, &["--ro-bind", "/", "/"]),
        SandboxLevel::Isolated => {
            for root in PLATFORM_READ_ROOTS {
                args.extend([
                    "--ro-bind-try".to_owned(),
                    (*root).to_owned(),
                    (*root).to_owned(),
                ]);
            }
            for root in request.readable_roots() {
                let root = literal_root(root)?;
                args.extend(["--ro-bind".to_owned(), root.clone(), root]);
            }
        }
        SandboxLevel::Unconfined => {
            return Err(SandboxError::Contradictory {
                detail: "the `unconfined` level does not go through a backend".to_owned(),
            });
        }
    }

    // A fresh `/proc` shows only this namespace's processes; `/dev` is the small device set a
    // program expects; `/tmp` is private and dies with the sandbox, so nothing a command leaves
    // there outlives it or is visible to the next one.
    push(
        &mut args,
        &["--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp"],
    );

    for root in request.writable_roots() {
        let root = literal_root(root)?;
        args.extend(["--bind".to_owned(), root.clone(), root]);
    }

    args.push("--".to_owned());
    Ok(args)
}

/// Checks that a root is an absolute literal path and returns it as a string.
///
/// Unlike the Seatbelt side, nothing is resolved: a bind mount names a path in *this* namespace and
/// the kernel resolves it once, at mount time, before the sandboxed command exists. What matters is
/// only that the host wrote something unambiguous.
fn literal_root(root: &Path) -> Result<String, SandboxError> {
    if !root.is_absolute() {
        return Err(SandboxError::UnusableRoot {
            root: root.to_path_buf(),
            detail: "a sandbox root must be absolute; bubblewrap resolves a bind source against \
                     its own working directory, not the host's intent"
                .to_owned(),
        });
    }
    if root
        .components()
        .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
    {
        return Err(SandboxError::UnusableRoot {
            root: root.to_path_buf(),
            detail: "a sandbox root may not contain `.` or `..`".to_owned(),
        });
    }
    root.to_str()
        .map(str::to_owned)
        .ok_or_else(|| SandboxError::UnusableRoot {
            root: root.to_path_buf(),
            detail: "a sandbox root must be valid UTF-8 to be passed as an argument".to_owned(),
        })
}

/// The command line that runs `command` inside the sandbox described by `args`.
#[must_use]
pub fn command_args(
    bwrap: &Path,
    mut args: Vec<String>,
    command: SandboxCommand,
) -> SandboxCommand {
    let (program, program_args) = command.into_parts();
    args.push(program.to_string_lossy().into_owned());
    args.extend(program_args);
    SandboxCommand::new(bwrap, args)
}

/// The first executable named `program` on `path`, if any.
///
/// Written out rather than pulled in as a dependency: this is the only lookup this crate needs, and
/// the behaviour that matters — first match wins, non-executables skipped — is a handful of lines.
///
/// # Relative entries are skipped, not resolved
///
/// A relative `PATH` entry names a different directory for every working directory, and the two
/// that matter here are not the same one: availability is probed from wherever this process happens
/// to be, while the command is launched with the working directory *the request asked for* — which
/// is the workspace a model has been writing into. A `PATH` containing `bin` would therefore probe
/// a trusted `bwrap` and then execute `<workspace>/bin/bwrap`: the sandbox would be whatever the
/// sandboxed side put there. Verified as a real sequence, not a theory.
///
/// Resolving them against the current directory instead would only move the problem, since nothing
/// promises that directory is the same at both moments. They are dropped, and a `PATH` that names
/// its directories relatively is a misconfiguration everywhere else too.
fn find_on_path(program: &str, path: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    std::env::split_paths(path?)
        .filter(|directory| directory.is_absolute())
        .find_map(|directory| {
            let candidate = directory.join(program);
            is_executable_file(&candidate).then_some(candidate)
        })
}

/// Whether `path` is a file this process could execute.
fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Runs bubblewrap's own smallest sandbox to see whether this kernel will grant one.
///
/// Presence on `PATH` is not the question — a container without `CAP_SYS_ADMIN`, a kernel with
/// unprivileged user namespaces disabled, and WSL1 all have the binary and none of them can use it.
/// A probe that cannot be concluded in [`PROBE_TIMEOUT`] is treated as a failure, because the
/// alternative is a host that hangs before its first command instead of being told what is wrong.
#[cfg(target_os = "linux")]
fn probe_user_namespace(bwrap: &Path) -> Result<(), String> {
    use std::process::{Command, Stdio};

    let mut probe = Command::new(bwrap);
    let filter = seccomp::compile(crate::sandbox::NetworkAccess::Denied)
        .map_err(|error| format!("cannot compile seccomp policy: {error}"))?;
    seccomp::attach(&mut probe, &filter)
        .map_err(|error| format!("cannot transfer seccomp policy: {error}"))?;
    let mut child = probe
        .args([
            "--unshare-net",
            "--unshare-pid",
            "--ro-bind",
            "/",
            "/",
            "/bin/true",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .stdin(Stdio::null())
        .spawn()
        .map_err(|error| format!("`{}` could not be started: {error}", bwrap.display()))?;

    let deadline = std::time::Instant::now() + PROBE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(_)) => {
                let mut stderr = String::new();
                if let Some(mut pipe) = child.stderr.take() {
                    use std::io::Read as _;
                    let _ = pipe.read_to_string(&mut stderr);
                }
                return Err(format!(
                    "bubblewrap could not create a namespace here; it usually means unprivileged \
                     user namespaces are disabled or this is a container without the capability \
                     to nest one. Its own words: {}",
                    stderr.trim()
                ));
            }
            Ok(None) if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(
                    "bubblewrap did not answer a trivial sandbox within half a second".to_owned(),
                );
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(20)),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("could not wait for the bubblewrap probe: {error}"));
            }
        }
    }
}

/// The Linux backend.
#[derive(Debug, Clone, Copy, Default)]
pub struct BwrapBackend;

/// The process-wide instance.
///
/// Availability is probed once. The probe starts a process, and doing that before every command
/// would double the cost of running a short one.
#[must_use]
pub fn backend() -> &'static BwrapBackend {
    &BwrapBackend
}

/// Where bubblewrap was found, for a report that can name it.
///
/// **Resolved once, and the same answer is used to probe and to launch.** Looking it up again per
/// command would let the two disagree — a `PATH` that changed in between, or an entry that resolves
/// differently from a different working directory — and the whole value of the probe is that it
/// tested the program that will actually run.
#[must_use]
pub fn located_program() -> Option<PathBuf> {
    static LOCATED: OnceLock<Option<PathBuf>> = OnceLock::new();
    LOCATED
        .get_or_init(|| find_on_path(BWRAP_PROGRAM, std::env::var_os("PATH").as_deref()))
        .clone()
}

impl SandboxBackend for BwrapBackend {
    fn name(&self) -> &'static str {
        BACKEND_NAME
    }

    fn available_level(&self) -> Result<SandboxLevel, SandboxUnavailable> {
        static AVAILABLE: OnceLock<Result<SandboxLevel, SandboxUnavailable>> = OnceLock::new();
        AVAILABLE
            .get_or_init(|| {
                if !cfg!(target_os = "linux") {
                    return Err(SandboxUnavailable::new(
                        BACKEND_NAME,
                        "bubblewrap runs only on Linux; this build can generate its arguments but \
                         cannot enforce them here",
                    ));
                }
                let Some(program) = located_program() else {
                    return Err(SandboxUnavailable::new(
                        BACKEND_NAME,
                        "`bwrap` is not on PATH; install bubblewrap with the system package \
                         manager",
                    ));
                };
                #[cfg(target_os = "linux")]
                if let Err(reason) = probe_user_namespace(&program) {
                    return Err(SandboxUnavailable::new(BACKEND_NAME, reason));
                }
                let _ = &program;
                Ok(SandboxLevel::Isolated)
            })
            .clone()
    }

    fn confine(
        &self,
        request: &ConfinementRequest,
        command: SandboxCommand,
    ) -> Result<SandboxCommand, SandboxError> {
        let program = located_program().ok_or_else(|| SandboxError::Unavailable {
            source: SandboxUnavailable::new(BACKEND_NAME, "`bwrap` is no longer on PATH"),
        })?;
        confine_with(&program, request, command)
    }
}

/// The whole of [`SandboxBackend::confine`] except finding the program.
///
/// Split out so the argument list and the attached filter can be asserted on a machine that has no
/// bubblewrap installed. Without the seam, the only test able to reach this code would be one that
/// needs the binary present, and the line that attaches the syscall policy — the line whose removal
/// would silently unfilter every command — would be covered nowhere.
fn confine_with(
    program: &Path,
    request: &ConfinementRequest,
    command: SandboxCommand,
) -> Result<SandboxCommand, SandboxError> {
    #[cfg(target_os = "linux")]
    let args = protected_args(request, control_socket_directories)?;
    #[cfg(not(target_os = "linux"))]
    let args = build_args(request)?;
    let wrapped = command_args(program, args, command);
    #[cfg(target_os = "linux")]
    {
        let mut wrapped = wrapped;
        wrapped.seccomp = Some(seccomp::compile(request.network()).map_err(|error| {
            SandboxError::Unavailable {
                source: SandboxUnavailable::new(BACKEND_NAME, format!("seccomp: {error}")),
            }
        })?);
        Ok(wrapped)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = wrapped;
        Err(SandboxError::Unavailable {
            source: SandboxUnavailable::new(BACKEND_NAME, "seccomp requires Linux"),
        })
    }
}

/// Inspection entry points for the external integration-test workspace.
///
/// The program lookup is portable and so is its test: which `PATH` entries this backend is willing
/// to trust is a decision, not a platform detail, and it is the kind that has to keep holding on
/// the machine someone is reading it on. The filter entries below are Linux-only because a BPF
/// program and the descriptor carrying it do not exist anywhere else.
#[cfg(feature = "test-api")]
pub mod test_api {
    use std::{ffi::OsStr, path::PathBuf};

    #[cfg(target_os = "linux")]
    use super::{ConfinementRequest, Path, SandboxCommand, SandboxError};
    #[cfg(target_os = "linux")]
    use crate::sandbox::NetworkAccess;

    /// Upstream mount-identity validation, fed recorded mountinfo for portable regression tests.
    #[cfg(unix)]
    pub fn check_control_socket_mounts(
        directory: &std::path::Path,
        device: &str,
        btrfs: bool,
        mount_id: Option<&str>,
        mountinfo: &[u8],
        masked_root: Option<&std::path::Path>,
    ) -> std::io::Result<std::collections::BTreeSet<PathBuf>> {
        let filesystem = if btrfs {
            super::daemon_mounts::SocketFilesystem::Btrfs
        } else {
            super::daemon_mounts::SocketFilesystem::Other
        };
        super::daemon_mounts::check_mounts(
            directory,
            device,
            filesystem,
            mount_id,
            mountinfo,
            masked_root,
        )
    }

    /// The launch arguments with control socket masks for `directories`, as a Linux launch builds
    /// them from the host's socket directory and its mount aliases.
    ///
    /// # Errors
    ///
    /// Returns what a launch would: [`crate::sandbox::SandboxError::UnusableRoot`] for an unusable
    /// root, and [`crate::sandbox::SandboxError::Unavailable`] when a directory cannot be masked.
    #[cfg(unix)]
    pub fn protected_args(
        request: &crate::sandbox::ConfinementRequest,
        directories: &std::collections::BTreeSet<PathBuf>,
    ) -> Result<Vec<String>, crate::sandbox::SandboxError> {
        super::protected_args(request, || Ok(directories.clone()))
    }

    /// The program this backend would launch for a given `PATH`.
    #[must_use]
    pub fn program_on_path(path: &OsStr) -> Option<PathBuf> {
        super::find_on_path(super::BWRAP_PROGRAM, Some(path))
    }

    /// The compiled syscall filter for a network setting, as a launch would carry it.
    #[cfg(target_os = "linux")]
    pub fn compiled_filter(network: NetworkAccess) -> std::io::Result<Vec<u8>> {
        super::seccomp::compile(network)
    }

    /// Wraps a command for a bubblewrap at `program`, without requiring one to be installed.
    #[cfg(target_os = "linux")]
    pub fn confine_with(
        program: &Path,
        request: &ConfinementRequest,
        command: SandboxCommand,
    ) -> Result<SandboxCommand, SandboxError> {
        super::confine_with(program, request, command)
    }
}

/// The arguments a launch uses: [`build_args`] followed by Codex's control socket masks.
///
/// As in Codex, unrestricted writes leave the control sockets reachable, so a writable `/` skips
/// the masks without preparing or inspecting the socket directory. Otherwise `directories` yields
/// the socket directory and its host mount aliases.
#[cfg(any(target_os = "linux", all(unix, feature = "test-api")))]
fn protected_args(
    request: &ConfinementRequest,
    directories: impl FnOnce() -> std::io::Result<BTreeSet<PathBuf>>,
) -> Result<Vec<String>, SandboxError> {
    let args = build_args(request)?;
    if request.has_full_disk_write_access() {
        return Ok(args);
    }
    directories()
        .and_then(|directories| insert_control_socket_masks(args, &directories))
        .map_err(|error| SandboxError::Unavailable {
            source: SandboxUnavailable::new(BACKEND_NAME, error.to_string()),
        })
}

/// The shared socket directory, created if missing, and every host path that aliases it: the set
/// Codex's `create_filesystem_args` masks.
#[cfg(target_os = "linux")]
fn control_socket_directories() -> std::io::Result<BTreeSet<PathBuf>> {
    let directory = ra_uds::prepare_shared_daemon_socket_directory()?;
    daemon_mounts::daemon_socket_mask_paths(&directory, None)
}

/// Inserts Codex's `append_daemon_socket_masks` after every bind that exposes a socket directory,
/// so a later writable root cannot reopen it.
///
/// A bind source inside a socket directory is refused, and a mask path that is not UTF-8 is an
/// error because a lossy conversion could mask a different path. A missing source is skipped:
/// bubblewrap skips it for `--ro-bind-try` and refuses to start for the other binds.
#[cfg(any(target_os = "linux", all(unix, feature = "test-api")))]
fn insert_control_socket_masks(
    args: Vec<String>,
    directories: &BTreeSet<PathBuf>,
) -> std::io::Result<Vec<String>> {
    let mut protected = Vec::new();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let is_bind = matches!(arg.as_str(), "--bind" | "--ro-bind" | "--ro-bind-try");
        protected.push(arg);
        if is_bind {
            let source = args
                .next()
                .ok_or_else(|| std::io::Error::other("missing bind source"))?;
            let destination = args
                .next()
                .ok_or_else(|| std::io::Error::other("missing bind destination"))?;
            protected.extend([source.clone(), destination.clone()]);
            let source = match std::fs::canonicalize(source) {
                Ok(source) => source,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            for directory in directories {
                if source.starts_with(directory) {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "app-server socket directory cannot be a sandbox mount root",
                    ));
                }
                if let Ok(relative) = directory.strip_prefix(&source) {
                    let destination = Path::new(&destination).join(relative);
                    let destination = destination
                        .to_str()
                        .ok_or_else(|| {
                            std::io::Error::other("app-server socket mask path must be valid UTF-8")
                        })?
                        .to_owned();
                    protected.extend([
                        "--perms".to_owned(),
                        "000".to_owned(),
                        "--tmpfs".to_owned(),
                        destination.clone(),
                        "--remount-ro".to_owned(),
                        destination,
                    ]);
                }
            }
        }
    }
    Ok(protected)
}
