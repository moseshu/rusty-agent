//! `ra-sandbox::materialize`: the order a manifest is put into a workspace in.
//!
//! What is checked here is sequencing and command shape rather than bytes on disk, because that is
//! where a manifest application goes wrong silently: an entry applied before the account that owns
//! it exists, a directory filled by two tasks racing to create it, or a checkout asked for with the
//! wrong git incantation all produce a workspace that looks plausible and is not the one declared.
//! The session underneath is a recorder, so every decision is visible without a real filesystem.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ra_core::sandbox::{
    AsUser, Entry, EntryOwner, ErrorCode, ExecRequest, ExecResult, FileEntry, FileMode, Group,
    Manifest, Mount, MountPattern, MountProvider, MountStrategy, MountpointOptions,
    NOOP_SNAPSHOT_TYPE, Permissions, S3Mount, SandboxConcurrencyLimits, SandboxError,
    SandboxResult, SandboxSession, SandboxSessionState, SessionResources, Snapshot, User,
};
use ra_sandbox::materialize::ManifestApplier;

/// One thing the applier asked the session to do.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Call {
    Mkdir(String),
    Write(String),
    Exec(Vec<String>),
}

/// A session that records what it was asked to do and answers plausibly.
struct RecordingSession {
    resources: SessionResources,
    state: SandboxSessionState,
    calls: Mutex<Vec<Call>>,
    written: Mutex<BTreeMap<String, Vec<u8>>>,
    /// Commands whose first token matches the key fail with this exit code.
    failing: BTreeMap<String, i32>,
    /// How many writes were in flight at once, at the most.
    in_flight: AtomicUsize,
    peak_in_flight: AtomicUsize,
    /// Whether a write pauses, so overlapping writes are observable.
    slow_writes: bool,
    write_gates: BTreeMap<String, Arc<tokio::sync::Notify>>,
    write_signals: BTreeMap<String, Arc<tokio::sync::Notify>>,
    failed_write: Option<String>,
    blocked_git: bool,
    git_started: tokio::sync::Notify,
    git_active: AtomicUsize,
    cleanup_gate: Option<Arc<tokio::sync::Notify>>,
    cleanup_started: tokio::sync::Notify,
    cleanup_finished: tokio::sync::Notify,
}

impl RecordingSession {
    fn new(manifest: Manifest) -> Self {
        Self {
            resources: SessionResources::new(),
            state: SandboxSessionState::new(
                "recording",
                Snapshot::new(NOOP_SNAPSHOT_TYPE, "test"),
                manifest,
            ),
            calls: Mutex::new(Vec::new()),
            written: Mutex::new(BTreeMap::new()),
            failing: BTreeMap::new(),
            in_flight: AtomicUsize::new(0),
            peak_in_flight: AtomicUsize::new(0),
            slow_writes: false,
            write_gates: BTreeMap::new(),
            write_signals: BTreeMap::new(),
            failed_write: None,
            blocked_git: false,
            git_started: tokio::sync::Notify::new(),
            git_active: AtomicUsize::new(0),
            cleanup_gate: None,
            cleanup_started: tokio::sync::Notify::new(),
            cleanup_finished: tokio::sync::Notify::new(),
        }
    }

    fn failing(mut self, program: &str, exit_code: i32) -> Self {
        self.failing.insert(program.to_owned(), exit_code);
        self
    }

    const fn slow_writes(mut self) -> Self {
        self.slow_writes = true;
        self
    }

    fn calls(&self) -> Vec<Call> {
        self.calls.lock().expect("calls").clone()
    }

    /// Every command that was run, as one string each.
    fn commands(&self) -> Vec<String> {
        self.calls()
            .into_iter()
            .filter_map(|call| match call {
                Call::Exec(command) => Some(command.join(" ")),
                _ => None,
            })
            .collect()
    }

    /// The paths that were written, in the order they were written.
    fn writes(&self) -> Vec<String> {
        self.calls()
            .into_iter()
            .filter_map(|call| match call {
                Call::Write(path) => Some(path),
                _ => None,
            })
            .collect()
    }

    fn record(&self, call: Call) {
        self.calls.lock().expect("calls").push(call);
    }
}

#[async_trait]
impl SandboxSession for RecordingSession {
    fn backend_id(&self) -> &str {
        "recording"
    }

    fn resources(&self) -> &SessionResources {
        &self.resources
    }

    fn state(&self) -> SandboxSessionState {
        self.state.clone()
    }

    async fn exec(&self, request: ExecRequest) -> SandboxResult<ExecResult> {
        self.record(Call::Exec(request.command.clone()));
        let program = request.command.first().cloned().unwrap_or_default();
        if program == "git" && self.blocked_git {
            self.git_active.fetch_add(1, Ordering::SeqCst);
            let _active = ActiveOperation(&self.git_active);
            self.git_started.notify_one();
            std::future::pending::<()>().await;
        }
        if program == "rm"
            && self
                .commands()
                .iter()
                .filter(|call| call.starts_with("rm -rf"))
                .count()
                == 2
        {
            assert_eq!(
                self.git_active.load(Ordering::SeqCst),
                0,
                "cleanup started before checkout cancellation"
            );
            self.cleanup_started.notify_one();
            if let Some(gate) = &self.cleanup_gate {
                gate.notified().await;
            }
            self.cleanup_finished.notify_one();
        }
        let exit_code = self.failing.get(&program).copied().unwrap_or(0);
        Ok(ExecResult::new(
            Vec::new(),
            if exit_code == 0 {
                Vec::new()
            } else {
                format!("{program} refused").into_bytes()
            },
            exit_code,
        ))
    }

    async fn running(&self) -> SandboxResult<bool> {
        Ok(true)
    }

    async fn ls(&self, _path: &str, _user: AsUser) -> SandboxResult<Vec<FileEntry>> {
        Ok(Vec::new())
    }

    async fn rm(&self, _path: &str, _recursive: bool, _user: AsUser) -> SandboxResult<()> {
        Ok(())
    }

    async fn mkdir(&self, path: &str, _parents: bool, _user: AsUser) -> SandboxResult<()> {
        self.record(Call::Mkdir(path.to_owned()));
        Ok(())
    }

    async fn read(&self, path: &str, _user: AsUser) -> SandboxResult<Vec<u8>> {
        self.written
            .lock()
            .expect("written")
            .get(path)
            .cloned()
            .ok_or_else(|| SandboxError::workspace_read_not_found(path))
    }

    async fn write(&self, path: &str, data: Vec<u8>, _user: AsUser) -> SandboxResult<()> {
        let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        let _active = ActiveOperation(&self.in_flight);
        self.peak_in_flight.fetch_max(now, Ordering::SeqCst);
        if let Some(gate) = self.write_gates.get(path) {
            gate.notified().await;
        }
        if self.failed_write.as_deref() == Some(path) {
            return Err(SandboxError::workspace_read_not_found(path));
        }
        if self.slow_writes {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        self.record(Call::Write(path.to_owned()));
        self.written
            .lock()
            .expect("written")
            .insert(path.to_owned(), data);
        if let Some(signal) = self.write_signals.get(path) {
            signal.notify_one();
        }
        Ok(())
    }

    async fn persist_workspace(&self) -> SandboxResult<Vec<u8>> {
        Ok(Vec::new())
    }

    async fn hydrate_workspace(&self, _data: Vec<u8>) -> SandboxResult<()> {
        Ok(())
    }
}

/// Records that an operation has stopped even when its future was cancelled.
struct ActiveOperation<'a>(&'a AtomicUsize);

impl Drop for ActiveOperation<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// A manifest rooted where a container one would be, so paths read as the reference writes them.
fn manifest() -> Manifest {
    Manifest::new().with_root("/workspace")
}

/// An applier over a session, measuring relative sources from a directory that does not matter here.
fn applier(session: &Arc<RecordingSession>) -> ManifestApplier {
    ManifestApplier::new(session.clone(), PathBuf::from("/nowhere"))
}

#[tokio::test]
async fn a_full_application_creates_the_root_then_every_declared_entry() {
    let manifest = manifest()
        .with_entry("README.md", Entry::file("hello"))
        .with_entry(
            "src",
            Entry::dir().with_child("main.rs", Entry::file("fn main() {}")),
        );
    let session = Arc::new(RecordingSession::new(manifest.clone()));

    let receipt = applier(&session)
        .apply_manifest(&manifest, false)
        .await
        .expect("apply");

    assert_eq!(
        session.writes(),
        vec!["/workspace/README.md", "/workspace/src/main.rs"]
    );
    assert!(
        session
            .calls()
            .contains(&Call::Mkdir("/workspace".to_owned())),
        "the root is created before anything goes into it"
    );
    // Inline content is already in the manifest, so nothing about it needs a checksum to be
    // recognisable later. The receipt is for content that came from somewhere else.
    assert!(receipt.is_empty(), "inline files report no checksum");
}

#[tokio::test]
async fn accounts_are_created_before_the_entries_that_are_owned_by_them() {
    let manifest = manifest()
        .with_user(User::new("builder"))
        .with_group(Group::new("web", vec![User::new("nginx")]))
        .with_entry("site", Entry::dir());
    let session = Arc::new(RecordingSession::new(manifest.clone()));

    applier(&session)
        .apply_manifest(&manifest, true)
        .await
        .expect("apply");

    let commands = session.commands();
    let groupadd = commands
        .iter()
        .position(|command| command == "groupadd web")
        .expect("groupadd");
    let usermod = commands
        .iter()
        .position(|command| command == "usermod -aG web nginx")
        .expect("usermod");
    let chmod = commands
        .iter()
        .position(|command| command.starts_with("chmod"))
        .expect("chmod");

    assert!(groupadd < usermod, "a group exists before it has members");
    assert!(
        usermod < chmod,
        "accounts are provisioned before any entry is given its permissions"
    );
    // Both the manifest's own user and the group's member are created, each exactly once.
    let useradds: Vec<&String> = commands
        .iter()
        .filter(|command| command.starts_with("useradd"))
        .collect();
    assert_eq!(useradds.len(), 2, "{useradds:?}");
}

#[tokio::test]
async fn nothing_is_provisioned_when_the_caller_says_the_accounts_already_exist() {
    let manifest = manifest().with_user(User::new("builder"));
    let session = Arc::new(RecordingSession::new(manifest.clone()));

    applier(&session)
        .apply_manifest(&manifest, false)
        .await
        .expect("apply");

    assert!(
        !session
            .commands()
            .iter()
            .any(|command| command.starts_with("useradd")),
        "a backend whose account database survived must not create them twice"
    );
}

#[tokio::test]
async fn a_failed_account_command_stops_the_application_before_any_entry_is_written() {
    let manifest = manifest()
        .with_user(User::new("builder"))
        .with_entry("README.md", Entry::file("hello"));
    let session = Arc::new(RecordingSession::new(manifest.clone()).failing("useradd", 9));

    let error = applier(&session)
        .apply_manifest(&manifest, true)
        .await
        .expect_err("refused");

    assert_eq!(error.error_code(), ErrorCode::ExecNonzero);
    assert!(session.writes().is_empty(), "{:?}", session.writes());
}

#[tokio::test]
async fn an_entry_is_given_the_ownership_and_permissions_it_declared() {
    let manifest = manifest().with_entry(
        "secret.txt",
        Entry::file("shh")
            .owned_by(EntryOwner::Group(Group::new("web", Vec::new())))
            .with_permissions(Permissions::default().owner_can(FileMode::All)),
    );
    let session = Arc::new(RecordingSession::new(manifest.clone()));

    applier(&session)
        .apply_manifest(&manifest, false)
        .await
        .expect("apply");

    let commands = session.commands();
    assert!(
        commands.contains(&"chgrp web /workspace/secret.txt".to_owned()),
        "{commands:?}"
    );
    // Four digits, and the file-type bit masked off: `chmod` is being asked what may be done with
    // the path, not what kind of thing is at it.
    assert!(
        commands.contains(&"chmod 0700 /workspace/secret.txt".to_owned()),
        "{commands:?}"
    );
}

#[tokio::test]
async fn a_directory_is_created_and_chmod_ed_as_a_directory() {
    let manifest = manifest().with_entry("cache", Entry::dir());
    let session = Arc::new(RecordingSession::new(manifest.clone()));

    applier(&session)
        .apply_manifest(&manifest, false)
        .await
        .expect("apply");

    assert!(
        session
            .calls()
            .contains(&Call::Mkdir("/workspace/cache".to_owned()))
    );
    assert!(
        session
            .commands()
            .contains(&"chmod 0755 /workspace/cache".to_owned()),
        "{:?}",
        session.commands()
    );
}

#[tokio::test]
async fn overlapping_entries_are_applied_in_declaration_order_rather_than_together() {
    // Two entries where one contains the other. Run concurrently, the one that creates the parent
    // can lose the race with the one that fills it, and which of them wins changes per run.
    let manifest = manifest()
        .with_entry("pkg", Entry::dir())
        .with_entry("pkg/mod.rs", Entry::file("mod inner;"))
        .with_entry("unrelated.txt", Entry::file("x"));
    let session = Arc::new(RecordingSession::new(manifest.clone()).slow_writes());

    applier(&session)
        .apply_manifest(&manifest, false)
        .await
        .expect("apply");

    let mkdir = session
        .calls()
        .iter()
        .position(|call| call == &Call::Mkdir("/workspace/pkg".to_owned()))
        .expect("the directory is created");
    let write = session
        .calls()
        .iter()
        .position(|call| call == &Call::Write("/workspace/pkg/mod.rs".to_owned()))
        .expect("the file is written");
    assert!(
        mkdir < write,
        "the parent is finished before the child starts"
    );
}

#[tokio::test]
async fn a_batch_runs_no_more_entries_at_once_than_it_was_allowed() {
    let mut manifest = manifest();
    for index in 0..8 {
        manifest = manifest.with_entry(format!("file-{index}.txt"), Entry::file("x"));
    }
    let session = Arc::new(RecordingSession::new(manifest.clone()).slow_writes());
    let limits = SandboxConcurrencyLimits::default()
        .with_manifest_entries(Some(2))
        .expect("a positive limit");

    applier(&session)
        .with_limits(limits)
        .apply_manifest(&manifest, false)
        .await
        .expect("apply");

    assert_eq!(session.writes().len(), 8);
    assert!(
        session.peak_in_flight.load(Ordering::SeqCst) <= 2,
        "saw {} writes at once",
        session.peak_in_flight.load(Ordering::SeqCst)
    );
}

#[tokio::test]
async fn an_unlimited_batch_really_does_run_everything_at_once() {
    let mut manifest = manifest();
    for index in 0..4 {
        manifest = manifest.with_entry(format!("file-{index}.txt"), Entry::file("x"));
    }
    let session = Arc::new(RecordingSession::new(manifest.clone()).slow_writes());
    let limits = SandboxConcurrencyLimits::default()
        .with_manifest_entries(None)
        .expect("no limit");

    applier(&session)
        .with_limits(limits)
        .apply_manifest(&manifest, false)
        .await
        .expect("apply");

    assert_eq!(session.peak_in_flight.load(Ordering::SeqCst), 4);
}

#[tokio::test]
async fn a_receipt_lists_files_in_entry_order_however_the_writes_interleaved() {
    let mut manifest = manifest();
    for index in 0..4 {
        manifest = manifest.with_entry(
            format!("file-{index}.txt"),
            Entry::local_file(format!("src-{index}.txt")),
        );
    }
    let directory = tempfile::tempdir().expect("a temporary directory");
    for index in 0..4 {
        std::fs::write(
            directory.path().join(format!("src-{index}.txt")),
            format!("content {index}"),
        )
        .expect("write the source");
    }
    let gate = Arc::new(tokio::sync::Notify::new());
    let mut recording = RecordingSession::new(manifest.clone());
    recording
        .write_gates
        .insert("/workspace/file-0.txt".into(), gate.clone());
    recording
        .write_signals
        .insert("/workspace/file-3.txt".into(), gate);
    let session = Arc::new(recording);

    let receipt = ManifestApplier::new(session.clone(), directory.path().to_path_buf())
        .apply_manifest(&manifest, false)
        .await
        .expect("apply");

    assert_eq!(session.writes().last().unwrap(), "/workspace/file-0.txt");
    let listed: Vec<&str> = receipt
        .files()
        .iter()
        .map(|file| file.path().as_str())
        .collect();
    assert_eq!(
        listed,
        vec![
            "/workspace/file-0.txt",
            "/workspace/file-1.txt",
            "/workspace/file-2.txt",
            "/workspace/file-3.txt",
        ]
    );
}

#[tokio::test]
async fn only_the_ephemeral_entries_are_rebuilt_and_no_account_is_touched() {
    let manifest = manifest()
        .with_user(User::new("builder"))
        .with_entry("keep.txt", Entry::file("persisted"))
        .with_entry("cache.txt", Entry::file("rebuilt").ephemeral(true));
    let session = Arc::new(RecordingSession::new(manifest.clone()));

    applier(&session)
        .apply_ephemeral(&manifest)
        .await
        .expect("apply");

    assert_eq!(session.writes(), vec!["/workspace/cache.txt"]);
    assert!(
        !session
            .commands()
            .iter()
            .any(|command| command.starts_with("useradd")),
        "a workspace coming back up keeps whatever accounts its backend kept"
    );
}

#[tokio::test]
async fn an_ephemeral_entry_nested_under_a_persisted_directory_is_found() {
    let manifest = manifest().with_entry(
        "app",
        Entry::dir()
            .with_child("config.toml", Entry::file("persisted"))
            .with_child("token", Entry::file("secret").ephemeral(true)),
    );
    let session = Arc::new(RecordingSession::new(manifest.clone()));

    applier(&session)
        .apply_ephemeral(&manifest)
        .await
        .expect("apply");

    assert_eq!(session.writes(), vec!["/workspace/app/token"]);
}

#[tokio::test]
async fn an_ephemeral_directory_is_rebuilt_whole_including_children_that_are_not_ephemeral() {
    // The directory is what was never persisted, so nothing inside it was persisted either.
    let manifest = manifest().with_entry(
        "cache",
        Entry::dir()
            .ephemeral(true)
            .with_child("index", Entry::file("rebuilt"))
            .with_child("pinned", Entry::file("also rebuilt").ephemeral(false)),
    );
    let session = Arc::new(RecordingSession::new(manifest.clone()));

    applier(&session)
        .apply_ephemeral(&manifest)
        .await
        .expect("apply");

    assert_eq!(
        session.writes(),
        vec!["/workspace/cache/index", "/workspace/cache/pinned"]
    );
}

#[tokio::test]
async fn a_mount_is_refused_after_everything_queued_before_it_has_been_written() {
    let mount = Mount::new(
        MountProvider::S3(S3Mount {
            bucket: "artifacts".to_owned(),
            ..Default::default()
        }),
        MountStrategy::InContainer {
            pattern: MountPattern::Mountpoint(MountpointOptions::default()),
        },
    )
    .expect("a supported provider and strategy");
    let manifest = manifest()
        .with_entry("a.txt", Entry::file("first"))
        .with_entry("b.txt", Entry::file("second"))
        .with_entry("mounted", Entry::mount(mount));
    let session = Arc::new(RecordingSession::new(manifest.clone()));

    let error = applier(&session)
        .apply_manifest(&manifest, false)
        .await
        .expect_err("in-container mount patterns are not implemented yet");

    assert_eq!(error.error_code(), ErrorCode::SandboxConfigInvalid);
    assert_eq!(error.context().get("pattern"), Some(&"mountpoint".into()));
    // The entries queued before the mount ran first: a mount is applied alone, and what was already
    // in flight is finished rather than abandoned.
    assert_eq!(
        session.writes(),
        vec!["/workspace/a.txt", "/workspace/b.txt"]
    );
}

#[tokio::test]
async fn a_checkout_of_a_named_ref_clones_it_shallow_and_copies_it_into_place() {
    let manifest = manifest().with_entry("vendor", Entry::git_repo("acme/widgets", "v1.2.3"));
    let session = Arc::new(RecordingSession::new(manifest.clone()));

    applier(&session)
        .apply_manifest(&manifest, false)
        .await
        .expect("apply");

    let commands = session.commands();
    assert_eq!(commands[0], "command -v git >/dev/null 2>&1");
    let clone = commands
        .iter()
        .find(|command| command.starts_with("git clone"))
        .expect("a clone");
    assert!(
        clone.contains("--depth 1 --no-tags --branch v1.2.3 https://github.com/acme/widgets.git"),
        "{clone}"
    );
    // The temporary directory is removed before the clone and again after the copy, so a failure
    // between them does not leave the tree behind.
    assert_eq!(
        commands
            .iter()
            .filter(|command| command.starts_with("rm -rf --"))
            .count(),
        2,
        "{commands:?}"
    );
    assert!(
        commands
            .iter()
            .any(|command| command.starts_with("cp -R --")
                && command.ends_with("/workspace/vendor/")),
        "{commands:?}"
    );
}

#[tokio::test]
async fn a_checkout_of_a_commit_fetches_it_because_a_clone_cannot_name_one() {
    let manifest =
        manifest().with_entry("vendor", Entry::git_repo("acme/widgets", "0a1b2c3d4e5f6a7"));
    let session = Arc::new(RecordingSession::new(manifest.clone()));

    applier(&session)
        .apply_manifest(&manifest, false)
        .await
        .expect("apply");

    let commands = session.commands();
    assert!(
        commands
            .iter()
            .any(|command| command.starts_with("git init"))
    );
    assert!(
        commands.iter().any(
            |command| command.contains("remote add origin https://github.com/acme/widgets.git")
        )
    );
    assert!(
        commands
            .iter()
            .any(|command| command.contains("fetch --depth 1 --no-tags origin 0a1b2c3d4e5f6a7"))
    );
    assert!(
        commands
            .iter()
            .any(|command| command.contains("checkout --detach FETCH_HEAD"))
    );
    assert!(
        !commands
            .iter()
            .any(|command| command.starts_with("git clone")),
        "a fetch that worked is not followed by a clone"
    );
}

#[tokio::test]
async fn a_checkout_needs_git_in_the_sandbox_and_says_so_rather_than_failing_at_the_clone() {
    let manifest = manifest().with_entry("vendor", Entry::git_repo("acme/widgets", "v1"));
    let session = Arc::new(
        RecordingSession::new(manifest.clone()).failing("command -v git >/dev/null 2>&1", 127),
    );

    let error = applier(&session)
        .apply_manifest(&manifest, false)
        .await
        .expect_err("no git");

    assert_eq!(error.error_code(), ErrorCode::GitMissingInImage);
    assert_eq!(error.retryable(), Some(false));
    assert!(
        !session
            .commands()
            .iter()
            .any(|command| command.starts_with("git ")),
        "nothing is attempted once the tool is known to be missing"
    );
}

#[tokio::test]
async fn a_clone_that_failed_reports_the_ref_and_the_command_output() {
    let manifest = manifest().with_entry("vendor", Entry::git_repo("acme/widgets", "v1"));
    let session = Arc::new(RecordingSession::new(manifest.clone()).failing("git", 128));

    let error = applier(&session)
        .apply_manifest(&manifest, false)
        .await
        .expect_err("clone failed");

    assert_eq!(error.error_code(), ErrorCode::GitCloneError);
    // Unclassified rather than permanent: a clone fails for a bad ref and for a flaky network, and
    // the difference is not visible from here.
    assert_eq!(error.retryable(), None);
    assert_eq!(
        error.context().get("ref").and_then(|value| value.as_str()),
        Some("v1")
    );
    assert_eq!(
        error.context().get("repo").and_then(|value| value.as_str()),
        Some("acme/widgets")
    );
}

#[tokio::test]
async fn a_checkout_subpath_must_name_somewhere_inside_the_repository() {
    for (subpath, reason) in [
        ("/etc", "absolute"),
        ("   ", "empty"),
        ("../outside", "parent_traversal"),
        ("docs\\api", "windows_path"),
    ] {
        let manifest = manifest().with_entry(
            "vendor",
            Entry::git_repo("acme/widgets", "v1").with_subpath(subpath),
        );
        let session = Arc::new(RecordingSession::new(manifest.clone()));

        let error = applier(&session)
            .apply_manifest(&manifest, false)
            .await
            .err()
            .unwrap_or_else(|| panic!("`{subpath}` should be refused"));

        assert_eq!(error.error_code(), ErrorCode::GitSubpathError, "{subpath}");
        assert_eq!(
            error
                .context()
                .get("reason")
                .and_then(|value| value.as_str()),
            Some(reason),
            "{subpath}"
        );
        // Refused before anything runs, because the answer does not depend on the sandbox.
        assert!(session.commands().is_empty(), "{subpath}");
    }
}

#[tokio::test]
async fn three_spellings_of_no_subpath_all_mean_the_whole_repository() {
    for subpath in ["", ".", "./"] {
        let manifest = manifest().with_entry(
            "vendor",
            Entry::git_repo("acme/widgets", "v1").with_subpath(subpath),
        );
        let session = Arc::new(RecordingSession::new(manifest.clone()));

        applier(&session)
            .apply_manifest(&manifest, false)
            .await
            .unwrap_or_else(|error| panic!("`{subpath}` should be accepted: {error:?}"));

        let copy = session
            .commands()
            .into_iter()
            .find(|command| command.starts_with("cp -R --"))
            .expect("a copy");
        let source = copy.split_whitespace().nth(3).expect("a source");
        assert!(
            source.ends_with("/.") && !source.contains("/./"),
            "`{subpath}` copied from {source}"
        );
    }
}

#[tokio::test]
async fn a_checkout_of_a_subpath_copies_only_that_directory() {
    let manifest = manifest().with_entry(
        "vendor",
        Entry::git_repo("acme/widgets", "v1").with_subpath("packages/core"),
    );
    let session = Arc::new(RecordingSession::new(manifest.clone()));

    applier(&session)
        .apply_manifest(&manifest, false)
        .await
        .expect("apply");

    let copy = session
        .commands()
        .into_iter()
        .find(|command| command.starts_with("cp -R --"))
        .expect("a copy");
    assert!(copy.contains("/packages/core/."), "{copy}");
}

#[tokio::test]
async fn a_later_failure_cancels_an_earlier_pending_entry() {
    let manifest = manifest()
        .with_entry("a-pending", Entry::file(b"pending".to_vec()))
        .with_entry("b-failed", Entry::file(b"failed".to_vec()))
        .with_entry("c-unstarted", Entry::file(b"unstarted".to_vec()));
    let mut recording = RecordingSession::new(manifest.clone());
    recording.write_gates.insert(
        "/workspace/a-pending".into(),
        Arc::new(tokio::sync::Notify::new()),
    );
    recording.failed_write = Some("/workspace/b-failed".into());
    let session = Arc::new(recording);
    let limits = SandboxConcurrencyLimits::new()
        .with_manifest_entries(Some(2))
        .unwrap();
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        applier(&session)
            .with_limits(limits)
            .apply_manifest(&manifest, false),
    )
    .await
    .expect("a later failure must not wait for an earlier entry");
    assert_eq!(
        result.unwrap_err().error_code(),
        ErrorCode::WorkspaceReadNotFound
    );
    assert_eq!(session.in_flight.load(Ordering::SeqCst), 0);
    assert!(session.writes().is_empty());
}

#[tokio::test]
async fn finished_entries_release_capacity_before_earlier_entries_finish() {
    let manifest = manifest()
        .with_entry("a-waits", Entry::file(b"first".to_vec()))
        .with_entry("b-finishes", Entry::file(b"second".to_vec()))
        .with_entry("c-unblocks", Entry::file(b"third".to_vec()));
    let signal = Arc::new(tokio::sync::Notify::new());
    let mut recording = RecordingSession::new(manifest.clone());
    recording
        .write_gates
        .insert("/workspace/a-waits".into(), signal.clone());
    recording
        .write_signals
        .insert("/workspace/c-unblocks".into(), signal);
    let session = Arc::new(recording);
    let limits = SandboxConcurrencyLimits::new()
        .with_manifest_entries(Some(2))
        .unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        applier(&session)
            .with_limits(limits)
            .apply_manifest(&manifest, false),
    )
    .await
    .expect("the third entry must start while the first is pending")
    .expect("apply");
    assert_eq!(
        session.writes(),
        vec![
            "/workspace/b-finishes",
            "/workspace/c-unblocks",
            "/workspace/a-waits"
        ]
    );
    assert_eq!(session.peak_in_flight.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn dropping_an_application_cancels_checkout_and_finishes_cleanup() {
    let manifest = manifest().with_entry("repo", Entry::git_repo("acme/widgets", "main"));
    let mut recording = RecordingSession::new(manifest.clone());
    recording.blocked_git = true;
    let session = Arc::new(recording);
    let applying = applier(&session);
    let mut application = Box::pin(applying.apply_manifest(&manifest, false));
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        tokio::select! {
            result = &mut application => panic!("checkout should be pending: {result:?}"),
            () = session.git_started.notified() => {}
        }
    })
    .await
    .expect("checkout started");
    drop(application);
    drop(applying);
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        session.cleanup_finished.notified(),
    )
    .await
    .expect("cancellation cleanup finished");
    assert_eq!(session.git_active.load(Ordering::SeqCst), 0);
    assert_eq!(
        session
            .commands()
            .iter()
            .filter(|call| call.starts_with("rm -rf"))
            .count(),
        2
    );
    assert!(
        !session
            .commands()
            .iter()
            .any(|call| call.starts_with("cp "))
    );
}

#[tokio::test]
async fn a_batch_failure_waits_for_cancelled_checkout_cleanup() {
    let manifest = manifest()
        .with_entry("a-repo", Entry::git_repo("acme/widgets", "main"))
        .with_entry("b-failed", Entry::file(b"failed".to_vec()));
    let fail_gate = Arc::new(tokio::sync::Notify::new());
    let cleanup_gate = Arc::new(tokio::sync::Notify::new());
    let mut recording = RecordingSession::new(manifest.clone());
    recording.blocked_git = true;
    recording.failed_write = Some("/workspace/b-failed".into());
    recording
        .write_gates
        .insert("/workspace/b-failed".into(), fail_gate.clone());
    recording.cleanup_gate = Some(cleanup_gate.clone());
    let session = Arc::new(recording);
    let applying = applier(&session);
    let mut application = Box::pin(applying.apply_manifest(&manifest, false));
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        tokio::select! {
            result = &mut application => panic!("checkout should be pending: {result:?}"),
            () = session.git_started.notified() => {}
        }
        fail_gate.notify_one();
        tokio::select! {
            result = &mut application => panic!("returned before cleanup: {result:?}"),
            () = session.cleanup_started.notified() => {}
        }
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), &mut application)
                .await
                .is_err()
        );
        cleanup_gate.notify_one();
        let error = application.await.expect_err("the write failed");
        assert_eq!(error.error_code(), ErrorCode::WorkspaceReadNotFound);
    })
    .await
    .expect("batch cancellation and cleanup finished");
}

// --- the rest of the reference's `test_entries.py` and `test_manifest_application.py` ---------

/// The commands that removed the temporary checkout, with where each ran.
fn removals(commands: &[String]) -> Vec<(usize, &String)> {
    commands
        .iter()
        .enumerate()
        .filter(|(_, command)| command.starts_with("rm -rf --"))
        .collect()
}

#[tokio::test]
async fn a_failed_checkout_still_removes_its_temporary_clone_afterwards() {
    // Removed before the clone and again after whatever failed, so a failure between them does not
    // leave the tree in the sandbox's temporary directory.
    for (failing, failed_step, code) in [
        ("git", "git clone", ErrorCode::GitCloneError),
        ("cp", "cp -R --", ErrorCode::GitCopyError),
    ] {
        let manifest = manifest().with_entry("vendor", Entry::git_repo("acme/widgets", "main"));
        let session = Arc::new(RecordingSession::new(manifest.clone()).failing(failing, 1));

        let error = applier(&session)
            .apply_manifest(&manifest, false)
            .await
            .expect_err("the checkout fails");

        assert_eq!(error.error_code(), code, "{failing}");
        let commands = session.commands();
        let removed = removals(&commands);
        assert_eq!(removed.len(), 2, "{commands:?}");
        assert_eq!(removed[0].1, removed[1].1, "the same directory both times");
        let failed_at = commands
            .iter()
            .position(|command| command.starts_with(failed_step))
            .expect("the failing step ran");
        assert!(removed[1].0 > failed_at, "{commands:?}");
    }
}

#[tokio::test]
async fn every_spelling_of_a_subpath_outside_the_repository_is_refused_before_anything_runs() {
    for (subpath, reason) in [
        ("   ", "empty"),
        ("/docs", "absolute"),
        ("../outside", "parent_traversal"),
        ("docs/../../outside", "parent_traversal"),
        ("C:/repo", "windows_path"),
        ("C:repo", "windows_path"),
        ("C:", "windows_path"),
        (" c:repo ", "windows_path"),
        ("docs\\outside", "windows_path"),
    ] {
        let manifest = manifest().with_entry(
            "vendor",
            Entry::git_repo("acme/widgets", "main").with_subpath(subpath),
        );
        let session = Arc::new(RecordingSession::new(manifest.clone()));

        let error = applier(&session)
            .apply_manifest(&manifest, false)
            .await
            .err()
            .unwrap_or_else(|| panic!("`{subpath}` should be refused"));

        assert_eq!(error.error_code(), ErrorCode::GitSubpathError, "{subpath}");
        assert_eq!(
            error
                .context()
                .get("reason")
                .and_then(|value| value.as_str()),
            Some(reason),
            "{subpath}"
        );
        assert_eq!(
            error
                .context()
                .get("subpath")
                .and_then(|value| value.as_str()),
            Some(subpath)
        );
        assert!(session.commands().is_empty(), "{subpath}");
    }
}

#[tokio::test]
async fn every_spelling_of_the_repository_root_copies_the_whole_checkout() {
    for subpath in ["", ".", "./", "./.", " ./ "] {
        let manifest = manifest().with_entry(
            "vendor",
            Entry::git_repo("acme/widgets", "main").with_subpath(subpath),
        );
        let session = Arc::new(RecordingSession::new(manifest.clone()));

        applier(&session)
            .apply_manifest(&manifest, false)
            .await
            .unwrap_or_else(|error| panic!("`{subpath}` should be accepted: {error:?}"));

        let copy = session
            .commands()
            .into_iter()
            .find(|command| command.starts_with("cp -R --"))
            .expect("a copy");
        let mut arguments = copy.split_whitespace().skip(3);
        let source = arguments.next().expect("a source");
        assert!(source.starts_with("/tmp/sandbox-git-"), "{source}");
        assert!(
            source.ends_with("/.") && !source.ends_with("//."),
            "`{subpath}` copied from {source}"
        );
        assert_eq!(arguments.next(), Some("/workspace/vendor/"));
    }
}

#[tokio::test]
async fn a_permission_command_that_fails_fails_the_application() {
    let manifest = manifest().with_entry("copied.txt", Entry::file("hello"));
    let session = Arc::new(RecordingSession::new(manifest.clone()).failing("chmod", 1));
    let error = applier(&session)
        .apply_manifest(&manifest, false)
        .await
        .expect_err("chmod failed");
    assert_eq!(error.error_code(), ErrorCode::ExecNonzero);

    // Ownership is settled first, and a failure there stops the entry before its mode is touched.
    let owned = self::manifest().with_entry(
        "copied.txt",
        Entry::file("hello").owned_by(EntryOwner::User(User::new("sandbox-user"))),
    );
    let session = Arc::new(RecordingSession::new(owned.clone()).failing("chgrp", 1));
    let error = applier(&session)
        .apply_manifest(&owned, false)
        .await
        .expect_err("chgrp failed");
    assert_eq!(error.error_code(), ErrorCode::ExecNonzero);
    let commands = session.commands();
    assert!(
        commands.contains(&"chgrp sandbox-user /workspace/copied.txt".to_owned()),
        "{commands:?}"
    );
    assert!(
        !commands.iter().any(|command| command.starts_with("chmod")),
        "{commands:?}"
    );
}

#[tokio::test]
async fn accounts_are_provisioned_with_the_references_commands_each_exactly_once() {
    // `alice` is both a user and a member; `bob` is only a member. Each gets an account once, and
    // no member gets a group of its own name from `groupadd`: `useradd -U` makes that one.
    let manifest = manifest()
        .with_user(User::new("alice"))
        .with_group(Group::new(
            "dev",
            vec![User::new("alice"), User::new("bob")],
        ));
    let session = Arc::new(RecordingSession::new(manifest.clone()));

    applier(&session)
        .apply_manifest(&manifest, true)
        .await
        .expect("apply");

    let commands = session.commands();
    assert_eq!(commands[0], "groupadd dev", "{commands:?}");
    for name in ["alice", "bob"] {
        assert!(
            !commands.contains(&format!("groupadd {name}")),
            "{commands:?}"
        );
        assert_eq!(
            commands
                .iter()
                .filter(|command| **command == format!("useradd -U -M -s /usr/sbin/nologin {name}"))
                .count(),
            1,
            "{commands:?}"
        );
        assert!(
            commands.contains(&format!("usermod -aG dev {name}")),
            "{commands:?}"
        );
    }
}

#[tokio::test]
async fn a_provisioning_failure_reports_the_command_and_what_it_printed() {
    let manifest = manifest().with_group(Group::new("dev", Vec::new()));
    let session = Arc::new(RecordingSession::new(manifest.clone()).failing("groupadd", 9));

    let error = applier(&session)
        .apply_manifest(&manifest, true)
        .await
        .expect_err("groupadd failed");

    assert_eq!(error.error_code(), ErrorCode::ExecNonzero);
    assert_eq!(
        error
            .context()
            .get("command_str")
            .and_then(|value| value.as_str()),
        Some("groupadd dev")
    );
    assert_eq!(
        error
            .context()
            .get("stderr")
            .and_then(|value| value.as_str()),
        Some("groupadd refused")
    );
    // Only one stream said anything, so the message is that stream without a label.
    assert_eq!(error.message(), "groupadd refused");
}

#[tokio::test]
async fn a_split_grant_authorizes_a_local_source_by_its_host_path() {
    let directory = tempfile::tempdir().expect("a temporary directory");
    let top = std::fs::canonicalize(directory.path()).expect("resolve");
    let base = top.join("base");
    let outside = top.join("outside");
    std::fs::create_dir(&base).expect("base");
    std::fs::create_dir(&outside).expect("outside");
    std::fs::write(outside.join("secret.txt"), "secret").expect("write");
    let grant = ra_core::sandbox::SandboxPathGrant::new("/mnt/shared-data")
        .expect("grant")
        .with_host_path(&outside.to_string_lossy())
        .expect("host path")
        .read_only(true);
    let manifest = manifest().with_path_grant(grant).with_entry(
        "copied.txt",
        Entry::local_file(outside.join("secret.txt").to_string_lossy().into_owned()),
    );
    let session = Arc::new(RecordingSession::new(manifest.clone()));

    ManifestApplier::new(session.clone(), base)
        .apply_manifest(&manifest, false)
        .await
        .expect("apply");

    assert_eq!(
        session
            .written
            .lock()
            .expect("written")
            .get("/workspace/copied.txt"),
        Some(&b"secret".to_vec())
    );
}
