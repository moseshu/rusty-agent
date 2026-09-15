//! The platform sandbox backends: which one is chosen, what it refuses, and what a command can
//! still reach once it is in force.
//!
//! **Three kinds of test live here, and they are not interchangeable.** Policy translation is pure
//! and runs on every platform, because a Linux host's arguments are worth protecting on the machine
//! they were written on. Selection and downgrade rules run against a stub backend, because the only
//! interesting cases are backends that cannot deliver, and the real one on this machine can. Only
//! the last group actually spawns anything, and it is confined to the platform whose backend is
//! being exercised.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use ra_exec::{
    sandbox::{
        Confinement, ConfinementRequest, ExecEnvironment, NetworkAccess, SandboxBackend,
        SandboxCommand, SandboxError, SandboxLevel, SandboxPolicy, SandboxUnavailable,
        resolve_confinement, resolve_confinement_with,
    },
    tmpdir::RunTempDir,
};
use tempfile::TempDir;

// Only the group that spawns under a real backend needs these, and that group is macOS-only.
#[cfg(target_os = "macos")]
use ra_exec::{
    command::ExecRequest,
    sandbox::platform_backend,
    session::{ExecError, ExecExecutionResult, ProcessManager},
};
#[cfg(target_os = "macos")]
use std::{
    io::{Read as _, Write as _},
    net::{TcpListener, TcpStream},
};

/// A backend that reports whatever the test needs it to and never confines anything.
#[derive(Debug)]
struct StubBackend {
    available: Result<SandboxLevel, &'static str>,
}

impl SandboxBackend for StubBackend {
    fn name(&self) -> &'static str {
        "stub"
    }

    fn available_level(&self) -> Result<SandboxLevel, SandboxUnavailable> {
        self.available
            .map_err(|reason| SandboxUnavailable::new("stub", reason))
    }

    fn confine(
        &self,
        _request: &ConfinementRequest,
        command: SandboxCommand,
    ) -> Result<SandboxCommand, SandboxError> {
        Ok(command)
    }
}

static STUB_ISOLATED: StubBackend = StubBackend {
    available: Ok(SandboxLevel::Isolated),
};
static STUB_WORKSPACE_ONLY: StubBackend = StubBackend {
    available: Ok(SandboxLevel::WorkspaceWrite),
};
static STUB_BROKEN: StubBackend = StubBackend {
    available: Err("the machine has no such thing"),
};

fn stub(backend: &'static StubBackend) -> Option<&'static dyn SandboxBackend> {
    Some(backend)
}

/// Resolves a request for `level` against `backend` with one writable root.
fn resolve(
    backend: Option<&'static dyn SandboxBackend>,
    policy: SandboxPolicy,
) -> Result<Confinement, SandboxError> {
    resolve_confinement_with(backend, &policy, None)
}

/// The request a backend would be handed for this level, without going near a real one.
fn request_for(level: SandboxLevel, network: NetworkAccess, roots: &[&Path]) -> ConfinementRequest {
    let mut policy = SandboxPolicy::new(level).with_network(network);
    for root in roots {
        policy = policy.with_writable_root(*root);
    }
    let confinement = resolve(stub(&STUB_ISOLATED), policy).expect("stub delivers every level");
    confinement
        .request()
        .expect("a confined command has a request")
        .clone()
}

// --------------------------------------------------------------------------------------------
// The ladder, selection and downgrade rules.
// --------------------------------------------------------------------------------------------

#[test]
fn test_levels_are_ordered_from_weakest_to_strongest() {
    assert!(SandboxLevel::Unconfined < SandboxLevel::WorkspaceWrite);
    assert!(SandboxLevel::WorkspaceWrite < SandboxLevel::Isolated);
    assert!(!SandboxLevel::Unconfined.needs_backend());
    assert!(SandboxLevel::WorkspaceWrite.needs_backend());
}

#[test]
fn test_an_environment_without_a_policy_reports_unconfined_rather_than_nothing() {
    let confinement = ExecEnvironment::new()
        .resolve_confinement()
        .expect("no policy cannot fail");

    assert_eq!(confinement.level(), SandboxLevel::Unconfined);
    assert_eq!(confinement.backend(), None);
    assert!(!confinement.report().is_downgraded());
}

#[test]
fn test_unconfined_cannot_promise_to_deny_the_network() {
    let policy = SandboxPolicy::new(SandboxLevel::Unconfined).with_network(NetworkAccess::Denied);

    let error = resolve_confinement(&policy, None).expect_err("a level that confines nothing");

    assert!(
        matches!(error, SandboxError::Contradictory { .. }),
        "a contradiction is refused rather than silently resolved one way: {error}"
    );
}

#[test]
fn test_a_missing_backend_refuses_instead_of_falling_back_to_the_baseline() {
    let error = resolve(None, SandboxPolicy::new(SandboxLevel::WorkspaceWrite))
        .expect_err("nothing can confine this");

    match error {
        SandboxError::NoBackend { detail, .. } => {
            assert!(
                detail.contains("backend") || detail.contains("no backend features"),
                "the error names what this build actually contains: {detail}"
            );
        }
        other => panic!("expected a missing backend, got {other:?}"),
    }
}

#[test]
fn test_an_unusable_backend_refuses_and_carries_its_own_reason() {
    let error = resolve(
        stub(&STUB_BROKEN),
        SandboxPolicy::new(SandboxLevel::Isolated),
    )
    .expect_err("the backend said it cannot run here");

    match error {
        SandboxError::Unavailable { source } => {
            assert_eq!(source.backend(), "stub");
            assert!(source.reason().contains("no such thing"));
        }
        other => panic!("expected an unavailable backend, got {other:?}"),
    }
}

#[test]
fn test_a_backend_that_falls_short_refuses_when_no_downgrade_was_authorised() {
    let error = resolve(
        stub(&STUB_WORKSPACE_ONLY),
        SandboxPolicy::new(SandboxLevel::Isolated),
    )
    .expect_err("isolated was asked for and cannot be delivered");

    match error {
        SandboxError::LevelNotDeliverable {
            requested,
            available,
            ..
        } => {
            assert_eq!(requested, SandboxLevel::Isolated);
            assert_eq!(available, SandboxLevel::WorkspaceWrite);
        }
        other => panic!("expected a level that cannot be delivered, got {other:?}"),
    }
}

#[test]
fn test_an_authorised_downgrade_is_taken_and_reported_with_the_result() {
    let confinement = resolve(
        stub(&STUB_WORKSPACE_ONLY),
        SandboxPolicy::new(SandboxLevel::Isolated).accepting_down_to(SandboxLevel::WorkspaceWrite),
    )
    .expect("the host said this far down is acceptable");

    assert_eq!(confinement.level(), SandboxLevel::WorkspaceWrite);
    assert_eq!(
        confinement.downgraded_from(),
        Some(SandboxLevel::Isolated),
        "what was asked for stays on the record, not only in a log line"
    );
    let report = confinement.report();
    assert!(report.is_downgraded());
    assert_eq!(report.level(), SandboxLevel::WorkspaceWrite);
    assert_eq!(report.backend(), Some("stub"));
}

#[test]
fn test_a_downgrade_stops_at_the_floor_the_host_authorised() {
    let error = resolve(
        stub(&STUB_WORKSPACE_ONLY),
        SandboxPolicy::new(SandboxLevel::Isolated).accepting_down_to(SandboxLevel::Isolated),
    )
    .expect_err("the floor is the requested level, so there is nowhere to go");

    assert!(
        matches!(error, SandboxError::BelowAcceptedLevel { .. }),
        "got {error:?}"
    );
}

#[test]
fn test_a_downgrade_may_not_quietly_hand_the_network_back() {
    // The host authorised falling all the way to `unconfined`, but asked for the network to be
    // denied — and `unconfined` has no way to deny it. The filesystem authorisation does not carry.
    let error = resolve(
        stub(&STUB_BROKEN),
        SandboxPolicy::new(SandboxLevel::Isolated)
            .accepting_down_to(SandboxLevel::Unconfined)
            .with_network(NetworkAccess::Denied),
    )
    .expect_err("the backend is unusable, so only `unconfined` is left");

    assert!(
        matches!(error, SandboxError::Unavailable { .. }),
        "an unusable backend is reported as such before any downgrade is considered: {error:?}"
    );
}

#[test]
fn test_the_scratch_directory_is_writable_in_every_generated_policy() {
    let parent = TempDir::new().expect("temp dir");
    let scratch = RunTempDir::open_in(parent.path(), "scratch-in-policy").expect("scratch");
    let scratch_path = scratch.path().to_path_buf();
    let environment = ExecEnvironment::new()
        .with_temp_dir(Arc::new(scratch))
        .with_sandbox(SandboxPolicy::new(SandboxLevel::WorkspaceWrite));

    let confinement = environment.resolve_confinement();
    let Ok(confinement) = confinement else {
        // No backend on this platform: the claim under test is about the writable set, and there is
        // no set to inspect. The refusal itself is covered by its own test above.
        return;
    };
    let request = confinement.request().expect("a confined command");

    assert!(
        request.writable_roots().contains(&scratch_path),
        "a command is told to use the scratch directory, so it must be able to write to it: {:?}",
        request.writable_roots()
    );
}

// --------------------------------------------------------------------------------------------
// Policy translation. Pure, and run on every platform on purpose.
// --------------------------------------------------------------------------------------------

#[test]
fn test_bwrap_binds_the_world_read_only_before_the_writable_roots() {
    let request = request_for(
        SandboxLevel::WorkspaceWrite,
        NetworkAccess::Denied,
        &[Path::new("/work/repo")],
    );

    let args = ra_exec::sandbox::bwrap::build_args(&request).expect("absolute roots");

    let ro = index_of(&args, "--ro-bind").expect("the read-only world");
    let bind = index_of(&args, "--bind").expect("the writable root");
    assert!(
        ro < bind,
        "a writable bind placed before the read-only world would be buried by it: {args:?}"
    );
    assert_eq!(args[ro + 1], "/", "the whole filesystem is readable here");
    assert_eq!(args[bind + 1], "/work/repo");
    assert_eq!(args[bind + 2], "/work/repo", "bound at the same path");
    assert_eq!(args.last().map(String::as_str), Some("--"));
}

#[test]
fn test_bwrap_unshares_the_network_only_when_the_policy_denies_it() {
    let denied = ra_exec::sandbox::bwrap::build_args(&request_for(
        SandboxLevel::WorkspaceWrite,
        NetworkAccess::Denied,
        &[],
    ))
    .expect("args");
    let allowed = ra_exec::sandbox::bwrap::build_args(&request_for(
        SandboxLevel::WorkspaceWrite,
        NetworkAccess::Allowed,
        &[],
    ))
    .expect("args");

    assert!(denied.iter().any(|arg| arg == "--unshare-net"));
    assert!(!allowed.iter().any(|arg| arg == "--unshare-net"));
    for args in [&denied, &allowed] {
        assert!(
            args.iter().any(|arg| arg == "--die-with-parent"),
            "the sandbox never outlives the process that asked for it"
        );
        assert!(args.iter().any(|arg| arg == "--unshare-pid"));
    }
}

#[test]
fn test_bwrap_at_the_isolated_level_binds_only_the_platform_and_the_declared_roots() {
    let request = request_for(
        SandboxLevel::Isolated,
        NetworkAccess::Denied,
        &[Path::new("/work/repo")],
    );

    let args = ra_exec::sandbox::bwrap::build_args(&request).expect("args");

    assert!(
        !args
            .windows(2)
            .any(|pair| pair[0] == "--ro-bind" && pair[1] == "/"),
        "the whole filesystem is not readable at this level: {args:?}"
    );
    assert!(
        args.windows(3)
            .any(|triple| triple[0] == "--ro-bind-try" && triple[1] == "/usr"),
        "the platform's own runtime is bound with -try, since a distribution has only some of it"
    );
}

#[test]
fn test_bwrap_refuses_a_root_that_is_not_an_absolute_literal_path() {
    let request = request_for(
        SandboxLevel::WorkspaceWrite,
        NetworkAccess::Denied,
        &[Path::new("relative/root")],
    );

    let error = ra_exec::sandbox::bwrap::build_args(&request).expect_err("relative root");

    assert!(
        matches!(error, SandboxError::UnusableRoot { .. }),
        "{error:?}"
    );
}

#[test]
fn test_seatbelt_denies_by_default_and_names_every_root_as_a_parameter() {
    let request = request_for(
        SandboxLevel::WorkspaceWrite,
        NetworkAccess::Denied,
        &[Path::new("/work/repo")],
    );

    let profile = ra_exec::sandbox::seatbelt::build_profile(&request).expect("profile");

    assert!(profile.policy().starts_with("(version 1)"));
    assert!(profile.policy().contains("(deny default)"));
    assert!(
        !profile.policy().contains("/work/repo"),
        "a path is passed as a parameter, never pasted into the policy text: {}",
        profile.policy()
    );
    assert_eq!(
        profile.params().first().map(|(key, _)| key.as_str()),
        Some("WRITABLE_ROOT_0")
    );
    assert!(
        !profile.policy().contains("network-outbound"),
        "denial needs no rule; the base already denies what is not named"
    );
}

#[test]
fn test_seatbelt_opens_the_network_only_when_the_policy_allows_it() {
    let allowed = ra_exec::sandbox::seatbelt::build_profile(&request_for(
        SandboxLevel::WorkspaceWrite,
        NetworkAccess::Allowed,
        &[],
    ))
    .expect("profile");

    assert!(allowed.policy().contains("(allow network-outbound)"));
    assert!(
        allowed
            .policy()
            .contains("com.apple.SystemConfiguration.DNSConfiguration"),
        "open sockets without a resolver would be an allowance that does not work"
    );
}

#[test]
fn test_seatbelt_reads_the_root_directory_itself_at_the_isolated_level() {
    let profile = ra_exec::sandbox::seatbelt::build_profile(&request_for(
        SandboxLevel::Isolated,
        NetworkAccess::Denied,
        &[],
    ))
    .expect("profile");

    assert!(
        profile.policy().contains("(literal \"/\")"),
        "without read access to the root directory every binary dies before main: {}",
        profile.policy()
    );
    assert!(profile.policy().contains("(subpath \"/usr\")"));
    assert!(
        !profile.policy().contains("(subpath \"/\")"),
        "reads are confined at this level: {}",
        profile.policy()
    );
}

#[test]
fn test_seatbelt_refuses_a_relative_root() {
    let error = ra_exec::sandbox::seatbelt::normalize_root(Path::new("work/repo"))
        .expect_err("a relative root resolves against whatever directory we happen to be in");

    assert!(
        matches!(error, SandboxError::UnusableRoot { .. }),
        "{error:?}"
    );
}

/// The first position of `needle` in `args`.
fn index_of(args: &[String], needle: &str) -> Option<usize> {
    args.iter().position(|arg| arg == needle)
}

// --------------------------------------------------------------------------------------------
// Real confinement. macOS only, because Seatbelt is the backend this machine family can enforce.
// --------------------------------------------------------------------------------------------

/// A manager confined to `root`, at `level`, with the given network access.
#[cfg(target_os = "macos")]
fn confined_manager(root: &Path, level: SandboxLevel, network: NetworkAccess) -> ProcessManager {
    let policy = SandboxPolicy::new(level)
        .with_network(network)
        .with_writable_root(root)
        .with_readable_root(root);
    ProcessManager::default().with_environment(ExecEnvironment::new().with_sandbox(policy))
}

/// Runs `command` in `root` and returns the summary, failing the test if it yielded.
#[cfg(target_os = "macos")]
async fn run_in(
    manager: &ProcessManager,
    root: &Path,
    command: &str,
) -> ra_exec::output::ExecOutputSummary {
    let request = ExecRequest::new(command).with_cwd(root);
    match manager.execute(request, None).await.expect("execute") {
        ExecExecutionResult::Completed(summary) => summary,
        other => panic!("command did not finish: {command} ({other:?})"),
    }
}

/// A canonical temporary root, since the kernel matches the path it resolved to.
#[cfg(target_os = "macos")]
fn canonical_temp() -> (TempDir, PathBuf) {
    let dir = TempDir::new().expect("temp dir");
    let path = dir.path().canonicalize().expect("canonicalize");
    (dir, path)
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn test_a_confined_command_writes_inside_its_root_and_nowhere_else() {
    let (_guard, root) = canonical_temp();
    let (_outside_guard, outside) = canonical_temp();
    let manager = confined_manager(&root, SandboxLevel::WorkspaceWrite, NetworkAccess::Denied);

    let inside = run_in(&manager, &root, "echo written > inside.txt && echo ok").await;
    assert_eq!(inside.exit_code(), Some(0), "stderr: {}", inside.stderr());
    assert!(root.join("inside.txt").is_file());

    let escape = format!("echo leaked > {}/outside.txt", outside.display());
    let denied = run_in(&manager, &root, &escape).await;
    assert_ne!(denied.exit_code(), Some(0), "the write outside must fail");
    assert!(
        !outside.join("outside.txt").exists(),
        "nothing may appear outside the writable root"
    );
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn test_the_level_in_force_decides_whether_a_file_outside_can_be_read() {
    let (_guard, root) = canonical_temp();
    let (_secret_guard, secret_dir) = canonical_temp();
    std::fs::write(secret_dir.join("secret.txt"), "classified").expect("write secret");
    let read_it = format!("cat {}", secret_dir.join("secret.txt").display());

    let workspace_write =
        confined_manager(&root, SandboxLevel::WorkspaceWrite, NetworkAccess::Denied);
    let readable = run_in(&workspace_write, &root, &read_it).await;
    assert!(
        readable.stdout().contains("classified"),
        "workspace-write confines writes, not reads: {}",
        readable.stderr()
    );

    let isolated = confined_manager(&root, SandboxLevel::Isolated, NetworkAccess::Denied);
    let denied = run_in(&isolated, &root, &read_it).await;
    assert!(
        !denied.stdout().contains("classified"),
        "isolated confines reads to the declared roots"
    );
    assert_ne!(denied.exit_code(), Some(0));
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn test_network_denial_is_enforced_against_a_listener_on_this_machine() {
    // A local listener rather than a name on the internet: the test has to be able to tell "the
    // sandbox refused" from "this machine is offline", and only a socket it opened itself can.
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
    let port = listener.local_addr().expect("addr").port();
    std::thread::spawn(move || {
        for stream in listener.incoming().take(2) {
            let Ok(mut stream) = stream else { continue };
            let _ = stream.write_all(b"reached\n");
        }
    });
    assert!(
        TcpStream::connect(("127.0.0.1", port)).is_ok(),
        "the listener has to be reachable for the denial to mean anything"
    );

    let (_guard, root) = canonical_temp();
    let probe = format!("exec 3<>/dev/tcp/127.0.0.1/{port} && head -c 7 <&3");

    let denied_manager =
        confined_manager(&root, SandboxLevel::WorkspaceWrite, NetworkAccess::Denied);
    let denied = ExecRequest::new(&probe)
        .with_cwd(&root)
        .with_shell(Some("/bin/bash"));
    let denied = match denied_manager.execute(denied, None).await.expect("execute") {
        ExecExecutionResult::Completed(summary) => summary,
        other => panic!("probe did not finish: {other:?}"),
    };
    assert!(
        !denied.stdout().contains("reached"),
        "a denied policy must not reach a socket on this machine: {denied:?}"
    );

    let allowed_manager =
        confined_manager(&root, SandboxLevel::WorkspaceWrite, NetworkAccess::Allowed);
    let allowed = ExecRequest::new(&probe)
        .with_cwd(&root)
        .with_shell(Some("/bin/bash"));
    let allowed = match allowed_manager
        .execute(allowed, None)
        .await
        .expect("execute")
    {
        ExecExecutionResult::Completed(summary) => summary,
        other => panic!("probe did not finish: {other:?}"),
    };
    assert!(
        allowed.stdout().contains("reached"),
        "the same probe has to succeed when the policy allows it, or the test proves nothing: {}",
        allowed.stderr()
    );
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn test_the_result_says_what_confined_the_command() {
    let (_guard, root) = canonical_temp();
    let manager = confined_manager(&root, SandboxLevel::Isolated, NetworkAccess::Denied);

    let summary = run_in(&manager, &root, "echo hello").await;

    let sandbox = summary
        .sandbox()
        .expect("a confined command reports its confinement");
    assert_eq!(sandbox.level(), SandboxLevel::Isolated);
    assert_eq!(sandbox.network(), NetworkAccess::Denied);
    assert_eq!(sandbox.backend(), Some("seatbelt"));
    assert!(!sandbox.is_downgraded());
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn test_a_command_can_write_to_the_run_scratch_directory_it_is_pointed_at() {
    let (_guard, root) = canonical_temp();
    let parent = TempDir::new().expect("temp dir");
    let parent_path = parent.path().canonicalize().expect("canonicalize");
    let scratch = RunTempDir::open_in(&parent_path, "confined-scratch").expect("scratch");
    let scratch_path = scratch.path().to_path_buf();
    let policy = SandboxPolicy::new(SandboxLevel::Isolated)
        .with_writable_root(&root)
        .with_readable_root(&root);
    let manager = ProcessManager::default().with_environment(
        ExecEnvironment::new()
            .with_temp_dir(Arc::new(scratch))
            .with_sandbox(policy),
    );

    let summary = run_in(
        &manager,
        &root,
        "echo scratch > \"$RUSTY_AGENT_TMPDIR/note.txt\" && echo ok",
    )
    .await;

    assert_eq!(summary.exit_code(), Some(0), "stderr: {}", summary.stderr());
    let mut written = String::new();
    std::fs::File::open(scratch_path.join("note.txt"))
        .expect("the scratch file exists")
        .read_to_string(&mut written)
        .expect("read back");
    assert_eq!(written.trim(), "scratch");
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn test_a_policy_the_backend_cannot_express_fails_the_command_as_a_sandbox_fault() {
    let policy = SandboxPolicy::new(SandboxLevel::WorkspaceWrite).with_writable_root("relative");
    let manager =
        ProcessManager::default().with_environment(ExecEnvironment::new().with_sandbox(policy));

    let error = manager
        .execute(ExecRequest::new("echo hello"), None)
        .await
        .expect_err("a relative root cannot be enforced");

    assert!(
        matches!(error, ExecError::Sandbox { .. }),
        "reported as the sandbox fault it is, not as a command that would not start: {error:?}"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn test_the_platform_backend_on_this_machine_is_seatbelt_and_can_deliver_the_top_level() {
    let backend = platform_backend().expect("macOS has a backend in a default build");

    assert_eq!(backend.name(), "seatbelt");
    assert_eq!(
        backend.available_level().expect("available"),
        SandboxLevel::Isolated
    );
}

// --------------------------------------------------------------------------------------------
// The Linux syscall filter. Linux only, because that is the only place it exists.
// --------------------------------------------------------------------------------------------

/// The `sock_filter` records a compiled BPF program is made of, parsed back out of the blob.
#[cfg(target_os = "linux")]
fn instructions(filter: &[u8]) -> Vec<(u16, u8, u8, u32)> {
    assert_eq!(
        filter.len() % 8,
        0,
        "a BPF program is a whole number of eight-byte instructions"
    );
    filter
        .chunks_exact(8)
        .map(|chunk| {
            (
                u16::from_ne_bytes([chunk[0], chunk[1]]),
                chunk[2],
                chunk[3],
                u32::from_ne_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]),
            )
        })
        .collect()
}

#[cfg(target_os = "linux")]
#[test]
fn test_denying_the_network_adds_syscalls_to_the_filter() {
    use ra_exec::sandbox::bwrap::test_api;

    let allowed = test_api::compiled_filter(NetworkAccess::Allowed).expect("compiles");
    let denied = test_api::compiled_filter(NetworkAccess::Denied).expect("compiles");

    assert!(!allowed.is_empty(), "every launch carries a filter");
    assert!(
        denied.len() > allowed.len(),
        "denying the network has to add rules, not only unshare a namespace: {} vs {} bytes",
        denied.len(),
        allowed.len()
    );
    for filter in [&allowed, &denied] {
        let program = instructions(filter);
        assert!(
            program
                .iter()
                .any(|(code, _, _, k)| *code == 0x0006 && *k == 0x0005_0001),
            "a matched rule has to return EPERM: {program:?}"
        );
    }
}

/// x32 reports the same `AUDIT_ARCH_X86_64` as the native ABI but renumbers its syscalls, so the
/// generated denylist would be looking up the wrong table. The guard in front of it is the only
/// thing standing between that and a filter that quietly matches nothing.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
fn test_the_filter_rejects_the_x32_syscall_table_before_anything_else() {
    use ra_exec::sandbox::bwrap::test_api;

    let filter = test_api::compiled_filter(NetworkAccess::Denied).expect("compiles");
    let program = instructions(&filter);

    assert_eq!(
        program[0],
        (0x0020, 0, 0, 0),
        "first: load the syscall number"
    );
    assert_eq!(
        program[1],
        (0x0035, 0, 1, 0x4000_0000),
        "second: take the next instruction when the x32 bit is set, skip it otherwise"
    );
    assert_eq!(
        program[2],
        (0x0006, 0, 0, 0x0005_0001),
        "third: EPERM, reached only by an x32 syscall number"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn test_a_confined_command_carries_the_filter_to_bubblewrap() {
    use ra_exec::sandbox::bwrap::test_api;

    let request = request_for(
        SandboxLevel::WorkspaceWrite,
        NetworkAccess::Denied,
        &[Path::new("/work/repo")],
    );
    // A path that need not exist: what is under test is the command this produces, not whether this
    // machine has bubblewrap installed.
    let wrapped = test_api::confine_with(
        Path::new("/nonexistent/bwrap"),
        &request,
        SandboxCommand::new("/bin/sh", ["-c", "echo hello"]),
    )
    .expect("the policy is expressible");

    let prepared = wrapped.prepare().expect("the filter can be transferred");
    let args: Vec<String> = prepared
        .get_args()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();

    let seccomp = args
        .iter()
        .position(|arg| arg == "--seccomp")
        .expect("the syscall policy must reach bubblewrap, or the command runs unfiltered");
    let descriptor: i32 = args[seccomp + 1].parse().expect("a descriptor number");
    assert!(
        descriptor >= 3,
        "the policy must not sit on a standard descriptor: the child's stdio is dup2'd over those \
         before it can be handed on, got {descriptor}"
    );
    assert!(args.iter().any(|arg| arg == "--unshare-net"));
    assert!(args.iter().any(|arg| arg == "echo hello"));
}

// --------------------------------------------------------------------------------------------
// Which program the Linux backend is willing to launch. Portable, because the decision is.
// --------------------------------------------------------------------------------------------

/// Writes an executable file named `bwrap` into `directory` and returns its path.
fn fake_bwrap(directory: &Path) -> PathBuf {
    let path = directory.join("bwrap");
    std::fs::write(&path, "#!/bin/sh\nexit 0\n").expect("write the stand-in");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("make it executable");
    }
    path
}

/// Restores the process working directory when it goes out of scope, panic or not.
///
/// **This is the only thing in this file that touches the working directory**, and it has to stay
/// that way: the tests in one binary share a process, so a test that read the working directory
/// could observe a neighbour's. Nothing here does — every path is absolute, and the spawn tests set
/// their own `cwd` on the request.
struct CwdGuard(PathBuf);

impl Drop for CwdGuard {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.0);
    }
}

/// A relative `PATH` entry names a different directory depending on who is asking.
///
/// **This is the sandbox escape it prevents, reproduced rather than described.** Availability is
/// probed from wherever the host process happens to be; the command is then launched with the
/// working directory the *request* asked for — the workspace a model has been writing into. With
/// `bin` on `PATH` those resolve to different files, so a trusted `bwrap` is probed and
/// `<workspace>/bin/bwrap` is executed: the program enforcing the sandbox would be the one the
/// sandboxed side wrote there.
///
/// The lookup below runs with the working directory moved into a tree that *does* contain
/// `bin/bwrap`, which is exactly the position the workspace is in. Before relative entries were
/// dropped it returned that file.
#[test]
fn test_a_relative_path_entry_is_never_used_to_find_the_sandbox() {
    use ra_exec::sandbox::bwrap::test_api;

    let workspace = TempDir::new().expect("temp dir");
    let planted = workspace.path().join("bin");
    std::fs::create_dir(&planted).expect("plant a directory a relative entry would find");
    fake_bwrap(&planted);

    // Every lookup happens while the working directory is the planted tree; the assertions run
    // after it has been restored, so a failure cannot leave the process somewhere unexpected.
    let (relative, dot, mixed) = {
        let original = std::env::current_dir().expect("current dir");
        let _guard = CwdGuard(original);
        std::env::set_current_dir(workspace.path()).expect("move into the planted tree");
        let mixed_path =
            std::env::join_paths([Path::new("bin"), planted.as_path()]).expect("join paths");
        (
            test_api::program_on_path(std::ffi::OsStr::new("bin")),
            test_api::program_on_path(std::ffi::OsStr::new(".")),
            test_api::program_on_path(&mixed_path),
        )
    };

    assert_eq!(
        relative, None,
        "`bin` holds an executable named bwrap right here, and it must still be refused: a \
         relative entry resolves against a working directory that is not the one the command runs \
         in"
    );
    assert_eq!(dot, None, "`.` is the same hazard spelled shorter");
    let found = mixed.expect("the absolute entry still answers");
    assert!(
        found.is_absolute() && found.starts_with(planted.as_path()),
        "the launched program must resolve the same way from every working directory: {}",
        found.display()
    );
}

#[test]
fn test_the_located_program_is_absolute_when_there_is_one() {
    if let Some(program) = ra_exec::sandbox::bwrap::located_program() {
        assert!(
            program.is_absolute(),
            "the probe and the launch share this path, so it may not depend on a working \
             directory: {}",
            program.display()
        );
    }
}
