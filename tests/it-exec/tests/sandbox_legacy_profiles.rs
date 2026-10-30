//! Root-list backend compatibility is checked before probing or launching a command.
use std::path::Path;

use ra_exec::sandbox::{
    ConfinementRequest, SandboxBackend, SandboxCommand, SandboxError, SandboxLevel, SandboxPolicy,
    SandboxUnavailable, resolve_confinement_with,
};
use ra_filesystem::{
    models::PermissionProfile,
    permissions::{
        FileSystemAccessMode, FileSystemPath, FileSystemSandboxEntry, FileSystemSandboxPolicy,
        NetworkSandboxPolicy,
    },
};
use ra_paths::AbsolutePathBuf;

#[derive(Debug)]
struct RootBackend;
impl SandboxBackend for RootBackend {
    fn name(&self) -> &'static str {
        "bwrap"
    }
    fn available_level(&self) -> Result<SandboxLevel, SandboxUnavailable> {
        Ok(SandboxLevel::Isolated)
    }
    fn confine(
        &self,
        request: &ConfinementRequest,
        _: SandboxCommand,
    ) -> Result<SandboxCommand, SandboxError> {
        Ok(SandboxCommand::new(
            "wrapper",
            request
                .writable_roots()
                .iter()
                .map(|path| path.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
        ))
    }
}
static BACKEND: RootBackend = RootBackend;

#[derive(Debug)]
struct UnavailableRootBackend;
impl SandboxBackend for UnavailableRootBackend {
    fn name(&self) -> &'static str {
        "bwrap"
    }
    fn available_level(&self) -> Result<SandboxLevel, SandboxUnavailable> {
        Err(SandboxUnavailable::new(
            "bwrap",
            "availability must be checked after profile compatibility",
        ))
    }
    fn confine(
        &self,
        _: &ConfinementRequest,
        _: SandboxCommand,
    ) -> Result<SandboxCommand, SandboxError> {
        panic!("a rejected profile must never reach command wrapping")
    }
}
static UNAVAILABLE_BACKEND: UnavailableRootBackend = UnavailableRootBackend;

fn entry(path: &Path, access: FileSystemAccessMode) -> FileSystemSandboxEntry {
    FileSystemSandboxEntry::new(
        AbsolutePathBuf::from_absolute_path(path).unwrap().into(),
        access,
    )
}
fn launch_policy(root: &Path, fs: &FileSystemSandboxPolicy) -> SandboxPolicy {
    SandboxPolicy::new(SandboxLevel::WorkspaceWrite).with_permission_profile(
        PermissionProfile::from_runtime_permissions(fs, NetworkSandboxPolicy::Restricted),
        root,
    )
}
fn rejected(root: &Path, fs: &FileSystemSandboxPolicy, reason: &str) {
    let policy = launch_policy(root, fs).accepting_down_to(SandboxLevel::Unconfined);
    // Persisted launch snapshots must not lose the restrictions either.
    let policy: SandboxPolicy =
        serde_json::from_value(serde_json::to_value(policy).unwrap()).unwrap();
    let error = resolve_confinement_with(Some(&UNAVAILABLE_BACKEND), &policy, None).unwrap_err();
    assert!(
        matches!(error, SandboxError::Preparation { .. }),
        "{error:?}"
    );
    assert!(error.to_string().contains(reason), "{error}");
    assert!(error.to_string().contains("command was not run"), "{error}");
}

#[test]
fn deny_paths_and_globs_cannot_be_flattened_into_roots() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    for path in [
        AbsolutePathBuf::from_absolute_path(root.join("private"))
            .unwrap()
            .into(),
        FileSystemPath::GlobPattern {
            pattern: format!("{}/*.secret", root.display()),
        },
    ] {
        let mut fs = FileSystemSandboxPolicy::read_only();
        fs.entries.push(FileSystemSandboxEntry::new(
            path,
            FileSystemAccessMode::Deny,
        ));
        rejected(root, &fs, "denied-read");
    }
}

#[test]
fn workspace_metadata_protection_is_refused_even_before_metadata_exists() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let fs = FileSystemSandboxPolicy::from_legacy_sandbox_policy_for_cwd(
        &ra_filesystem::legacy::SandboxPolicy::new_workspace_write_policy(),
        root,
    );
    rejected(root, &fs, "protected metadata");
    std::fs::create_dir(root.join(".git")).unwrap();
    rejected(root, &fs, "protected metadata");
}

#[test]
fn read_only_carveouts_are_refused_after_explicit_metadata_write_overrides() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let mut fs = FileSystemSandboxPolicy::read_only();
    fs.entries.push(entry(root, FileSystemAccessMode::Write));
    for name in [".git", ".agents", ".codex", ".rusty-agent", ".aws"] {
        fs.entries
            .push(entry(&root.join(name), FileSystemAccessMode::Write));
    }
    fs.entries
        .push(entry(&root.join("readonly"), FileSystemAccessMode::Read));
    rejected(root, &fs, "read-only paths within writable roots");
}

#[test]
fn restricted_reads_are_not_widened_to_full_host_reads() {
    let tmp = tempfile::tempdir().unwrap();
    let fs =
        FileSystemSandboxPolicy::restricted(vec![entry(tmp.path(), FileSystemAccessMode::Read)]);
    rejected(tmp.path(), &fs, "restricts readable paths");
}

#[test]
fn read_only_profile_remains_enforceable_and_ignores_legacy_write_fields() {
    let tmp = tempfile::tempdir().unwrap();
    let policy = launch_policy(tmp.path(), &FileSystemSandboxPolicy::read_only())
        .with_writable_root(tmp.path());
    let confinement = resolve_confinement_with(Some(&BACKEND), &policy, None).unwrap();
    assert!(confinement.request().unwrap().writable_roots().is_empty());
    let wrapped = confinement
        .apply(SandboxCommand::new("unused", Vec::<String>::new()))
        .unwrap();
    assert_eq!(wrapped.program(), Path::new("wrapper"));
    assert!(wrapped.args().is_empty());
}

#[test]
fn scratch_permissions_are_validated_before_backend_launch() {
    let tmp = tempfile::tempdir().unwrap();
    let scratch = tmp.path().join("scratch");
    std::fs::create_dir(&scratch).unwrap();
    let policy = launch_policy(tmp.path(), &FileSystemSandboxPolicy::read_only());
    let error = resolve_confinement_with(Some(&BACKEND), &policy, Some(&scratch)).unwrap_err();
    assert!(error.to_string().contains("protected metadata"), "{error}");
}

#[test]
fn full_disk_write_with_restricted_network_is_not_lowered_to_a_read_only_mount() {
    let tmp = tempfile::tempdir().unwrap();
    rejected(
        tmp.path(),
        &FileSystemSandboxPolicy::unrestricted(),
        "cannot be represented as legacy roots",
    );
}

#[test]
fn explicit_unconfined_profiles_do_not_acquire_a_legacy_sandbox() {
    let tmp = tempfile::tempdir().unwrap();
    for profile in [
        PermissionProfile::Disabled,
        PermissionProfile::External {
            network: NetworkSandboxPolicy::Restricted,
        },
    ] {
        let policy = SandboxPolicy::new(SandboxLevel::WorkspaceWrite)
            .with_permission_profile(profile, tmp.path());
        assert!(
            resolve_confinement_with(None, &policy, None)
                .unwrap()
                .backend()
                .is_none()
        );
    }
}
