//! `ra-sandbox::unix_local`: the fence a local command runs inside.
//!
//! On macOS the reference wraps each command in `sandbox-exec` with a profile it builds per command.
//! Most of what that profile says is decided by inputs a test cannot give the process it runs in —
//! the host's search path, whether the tool exists at all — so the reference's tests replace them
//! by patching the platform, the tool lookup and the environment. [`HostConfinement`] holds those
//! inputs as a value instead, and these tests build one per question. They run on every unix, as
//! the reference's do; the profile is the same text wherever it is computed.
//!
//! The last two tests run real commands and check what the fence did to them, on the host this
//! suite is running on.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use ra_core::sandbox::{
    CreateRequest, ErrorCode, ExecRequest, Manifest, SandboxClient, SandboxPathGrant,
};
use ra_sandbox::unix_local::{HostConfinement, UnixLocalSandboxClient};

/// Where the tests say `sandbox-exec` is. Never run: the tests only read the vector it would start.
const TOOL: &str = "/usr/bin/sandbox-exec";

/// A directory with no symlinks in its path, so the path as written and as resolved agree.
fn directory() -> (tempfile::TempDir, PathBuf) {
    let directory = tempfile::tempdir().expect("temp");
    let resolved = std::fs::canonicalize(directory.path()).expect("canonical");
    (directory, resolved)
}

/// The environment a command gets, with `PATH` set to `path`.
fn env_with_path(path: &str) -> BTreeMap<String, String> {
    BTreeMap::from([("PATH".to_owned(), path.to_owned())])
}

/// The profile a confinement wraps `command` in.
fn profile(
    confinement: &HostConfinement,
    command: &[&str],
    workspace: &Path,
    env: &BTreeMap<String, String>,
    grants: &[SandboxPathGrant],
) -> Vec<String> {
    let wrapped = confinement
        .wrap(
            command.iter().map(|part| (*part).to_owned()).collect(),
            workspace,
            env,
            grants,
        )
        .expect("wrap");
    assert_eq!(wrapped[..2], [TOOL, "-p"]);
    assert_eq!(wrapped[3..], *command);
    wrapped[2].lines().map(str::to_owned).collect()
}

fn read_allow(path: &Path) -> String {
    format!(
        "(allow file-read-data file-read-metadata (subpath \"{}\"))",
        path.to_string_lossy()
    )
}

fn write_allow(path: &Path) -> String {
    format!(
        "(allow file-write* (subpath \"{}\"))",
        path.to_string_lossy()
    )
}

fn write_deny(path: &Path) -> String {
    format!(
        "(deny file-write* (subpath \"{}\"))",
        path.to_string_lossy()
    )
}

/// A Python virtual environment under a project directory, with an executable interpreter.
fn virtual_environment(parent: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let project = parent.join("host-project");
    let root = project.join(".venv");
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).expect("bin");
    std::fs::write(root.join("pyvenv.cfg"), "home = /usr/bin\n").expect("pyvenv.cfg");
    std::fs::write(bin.join("python"), "").expect("python");
    std::fs::set_permissions(
        bin.join("python"),
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .expect("mode");
    (project, root, bin)
}

#[test]
fn a_host_that_fences_commands_but_has_lost_the_tool_refuses_to_run_them() {
    let (_temp, workspace) = directory();
    let confinement = HostConfinement::sandbox_exec(None, "/usr/bin:/bin", "/");

    let error = confinement
        .wrap(
            vec!["pwd".to_owned()],
            &workspace,
            &env_with_path("/usr/bin:/bin"),
            &[],
        )
        .expect_err("a macOS host without sandbox-exec");

    // Not a quiet fallback to running unfenced: a host that has lost the tool is broken, not asking
    // for wider access.
    assert_eq!(error.error_code(), ErrorCode::ExecTransportError);
    assert_eq!(
        error
            .context()
            .get("reason")
            .and_then(serde_json::Value::as_str),
        Some("unix_local_confinement_unavailable")
    );
}

#[test]
fn an_unconfined_host_runs_the_command_as_written() {
    let (_temp, workspace) = directory();
    let command = vec!["pwd".to_owned()];

    let wrapped = HostConfinement::unconfined()
        .wrap(command.clone(), &workspace, &env_with_path("/usr/bin"), &[])
        .expect("wrap");

    assert_eq!(wrapped, command);
    // Which of the two this host is: only macOS has the fence, as in the reference.
    assert_eq!(
        HostConfinement::current() == HostConfinement::unconfined(),
        !cfg!(target_os = "macos")
    );
}

#[test]
fn common_interpreter_roots_are_readable_but_not_writable() {
    let (_workspace_temp, workspace) = directory();
    let (_home_temp, home) = directory();
    let path = format!(
        "/opt/homebrew/bin:/usr/local/bin:{}",
        home.join(".local/bin").to_string_lossy()
    );
    let confinement = HostConfinement::sandbox_exec(Some(PathBuf::from(TOOL)), "", &home);

    let lines = profile(
        &confinement,
        &["python3", "-V"],
        &workspace,
        &env_with_path(&path),
        &[],
    );

    // A toolchain reaches sideways into its own prefix, and a per-user one installs under a dotted
    // directory in the home; each is opened for reading as a whole.
    assert!(
        lines.contains(&read_allow(Path::new("/opt/homebrew"))),
        "{lines:#?}"
    );
    assert!(
        lines.contains(&read_allow(Path::new("/usr/local"))),
        "{lines:#?}"
    );
    assert!(
        lines.contains(&read_allow(&home.join(".local"))),
        "{lines:#?}"
    );
    assert!(lines.contains(&write_deny(Path::new("/opt"))));
    assert!(!lines.contains(&write_allow(Path::new("/opt/homebrew"))));
}

#[test]
fn a_virtual_environment_on_the_hosts_own_path_is_readable_from_its_root() {
    let (_workspace_temp, workspace) = directory();
    let (_temp, parent) = directory();
    let (project, root, bin) = virtual_environment(&parent);
    let bin_path = bin.to_string_lossy().into_owned();
    let confinement = HostConfinement::sandbox_exec(Some(PathBuf::from(TOOL)), &bin_path, "/");

    let lines = profile(
        &confinement,
        &["python", "-V"],
        &workspace,
        &env_with_path(&bin_path),
        &[],
    );

    // The interpreter reads its standard library from the environment root, not from `bin`.
    assert!(lines.contains(&read_allow(&root)), "{lines:#?}");
    assert!(!lines.contains(&read_allow(&project)), "{lines:#?}");
    assert!(!lines.contains(&write_allow(&root)));
}

#[test]
fn a_virtual_environment_only_the_manifest_names_is_not_widened_to_its_root() {
    let (_workspace_temp, workspace) = directory();
    let (_temp, parent) = directory();
    let (project, root, bin) = virtual_environment(&parent);
    let confinement =
        HostConfinement::sandbox_exec(Some(PathBuf::from(TOOL)), "/usr/bin:/bin", "/");

    let lines = profile(
        &confinement,
        &["python", "-V"],
        &workspace,
        &env_with_path(&bin.to_string_lossy()),
        &[],
    );

    // The command may use it, so its `bin` is readable. The widening is the host's decision: a
    // manifest that sets `PATH` must not be able to turn it into read access to a whole project.
    assert!(lines.contains(&read_allow(&bin)), "{lines:#?}");
    assert!(!lines.contains(&read_allow(&root)), "{lines:#?}");
    assert!(!lines.contains(&read_allow(&project)), "{lines:#?}");
}

#[test]
fn a_grant_opens_its_directory_and_a_read_only_one_opens_it_only_for_reading() {
    let (_workspace_temp, workspace) = directory();
    let (_temp, parent) = directory();
    let read_write = parent.join("read-write");
    let read_only = parent.join("read-only");
    std::fs::create_dir_all(&read_write).expect("read-write");
    std::fs::create_dir_all(&read_only).expect("read-only");
    let grants = [
        SandboxPathGrant::new(&read_write.to_string_lossy()).expect("grant"),
        SandboxPathGrant::new(&read_only.to_string_lossy())
            .expect("grant")
            .read_only(true),
    ];
    let confinement =
        HostConfinement::sandbox_exec(Some(PathBuf::from(TOOL)), "/usr/bin:/bin", "/");

    let lines = profile(
        &confinement,
        &["true"],
        &workspace,
        &env_with_path("/usr/bin:/bin"),
        &grants,
    );

    assert!(lines.contains(&read_allow(&read_write)));
    assert!(lines.contains(&write_allow(&read_write)));
    assert!(lines.contains(&read_allow(&read_only)));
    assert!(!lines.contains(&write_allow(&read_only)));
}

#[test]
fn a_read_only_grant_inside_a_writable_one_is_denied_after_the_parent_is_allowed() {
    let (_workspace_temp, workspace) = directory();
    let (_temp, parent) = directory();
    let read_write = parent.join("read-write");
    let protected = read_write.join("protected");
    std::fs::create_dir_all(&protected).expect("protected");
    let grants = [
        SandboxPathGrant::new(&read_write.to_string_lossy()).expect("grant"),
        SandboxPathGrant::new(&protected.to_string_lossy())
            .expect("grant")
            .read_only(true),
    ];
    let confinement =
        HostConfinement::sandbox_exec(Some(PathBuf::from(TOOL)), "/usr/bin:/bin", "/");

    let lines = profile(
        &confinement,
        &["true"],
        &workspace,
        &env_with_path("/usr/bin:/bin"),
        &grants,
    );

    // A later rule wins, so the narrowing has to come after the allow it narrows.
    let allowed = lines
        .iter()
        .position(|line| *line == write_allow(&read_write))
        .expect("parent allowed");
    let denied = lines
        .iter()
        .position(|line| *line == write_deny(&protected))
        .expect("child denied");
    assert!(allowed < denied, "{lines:#?}");
    assert!(!lines.contains(&write_allow(&protected)));
}

#[test]
fn a_grant_that_is_a_link_to_the_filesystem_root_is_refused() {
    let (_workspace_temp, workspace) = directory();
    let (_temp, parent) = directory();
    let alias = parent.join("root-alias");
    std::os::unix::fs::symlink("/", &alias).expect("link");
    let grants = [SandboxPathGrant::new(&alias.to_string_lossy()).expect("a grant as written")];
    let confinement =
        HostConfinement::sandbox_exec(Some(PathBuf::from(TOOL)), "/usr/bin:/bin", "/");

    let error = confinement
        .wrap(
            vec!["true".to_owned()],
            &workspace,
            &env_with_path("/usr/bin:/bin"),
            &grants,
        )
        .expect_err("a grant of the whole host");

    assert_eq!(error.error_code(), ErrorCode::SandboxConfigInvalid);
    assert_eq!(
        error.message(),
        "sandbox path grant path must not resolve to filesystem root"
    );
}

#[tokio::test]
async fn a_command_runs_in_the_workspace_it_was_given() {
    let (_temp, workspace) = directory();
    let client = UnixLocalSandboxClient::new();
    let session = client
        .create(
            CreateRequest::new()
                .with_manifest(Manifest::new().with_root(workspace.to_string_lossy().into_owned())),
        )
        .await
        .expect("create");
    session.start().await.expect("start");

    let result = session
        .exec(ExecRequest::new(["pwd".to_owned()]))
        .await
        .expect("exec");

    assert!(result.ok(), "{}", String::from_utf8_lossy(&result.stderr));
    assert_eq!(
        String::from_utf8_lossy(&result.stdout).trim(),
        workspace.to_string_lossy()
    );
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn a_fenced_command_cannot_reach_outside_the_workspace() {
    let (_temp, workspace) = directory();
    let client = UnixLocalSandboxClient::new();
    let session = client
        .create(
            CreateRequest::new()
                .with_manifest(Manifest::new().with_root(workspace.to_string_lossy().into_owned())),
        )
        .await
        .expect("create");
    session.start().await.expect("start");
    let run = |script: &str| session.exec(ExecRequest::new([script.to_owned()]));

    let inside = run("echo hi > note.txt && cat note.txt")
        .await
        .expect("exec");
    assert!(inside.ok(), "{}", String::from_utf8_lossy(&inside.stderr));
    assert!(
        String::from_utf8_lossy(&inside.stdout)
            .trim()
            .ends_with("hi")
    );

    assert!(!run("cat /etc/passwd >/dev/null").await.expect("exec").ok());
    assert!(
        !run("echo nope > /usr/local/test-sandbox")
            .await
            .expect("exec")
            .ok()
    );
    let sibling = workspace.parent().expect("parent").join("escape.txt");
    let _ = std::fs::remove_file(&sibling);
    assert!(!run("echo nope > ../escape.txt").await.expect("exec").ok());
    assert!(!sibling.exists());
}
