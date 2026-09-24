//! `ra-sandbox::unix_local`: what reaches a command from this host's own environment.
//!
//! These questions need the process running the session to *have* particular variables — a
//! credential-shaped one, a locale variable the allowlist names, one that only looks like it — and a
//! test cannot give its own process new variables without racing every other test that reads them.
//! So each scenario runs in a child copy of this binary started with exactly the variables it needs.
//! The scenarios are `#[ignore]`d so that an ordinary run only sees the parent tests, which launch a
//! child, pass it the variables and report how it went.

use std::path::PathBuf;

use ra_core::sandbox::{
    CreateRequest, Environment, ExecRequest, Manifest, SandboxClient, SandboxPathGrant,
    SandboxSession, ShellInvocation,
};
use ra_sandbox::unix_local::UnixLocalSandboxClient;

/// Set in a child, so a scenario run any other way says so instead of passing on the wrong input.
const CHILD_MARKER: &str = "IT_SANDBOX_HOST_ENVIRONMENT_CHILD";

/// Runs one ignored scenario in a child process whose environment also has `vars`.
fn run_in_child(scenario: &str, vars: &[(&str, &str)]) {
    let output = std::process::Command::new(std::env::current_exe().expect("this binary"))
        .args([scenario, "--exact", "--ignored", "--test-threads=1"])
        .env(CHILD_MARKER, "1")
        .envs(vars.iter().copied())
        .output()
        .expect("child");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "{scenario} failed:\n{stdout}\n{stderr}"
    );
    // Guards against a filter that matched nothing, which libtest reports as success.
    assert!(
        stdout.contains("1 passed"),
        "{scenario} did not run:\n{stdout}"
    );
}

/// Refuses to run a scenario outside the child its parent prepared.
fn assert_in_child() {
    assert!(
        std::env::var_os(CHILD_MARKER).is_some(),
        "run through its parent test, which prepares the environment"
    );
}

/// A started session over a fresh workspace.
async fn started(client: &UnixLocalSandboxClient, manifest: Manifest) -> Box<dyn SandboxSession> {
    let session = client
        .create(CreateRequest::new().with_manifest(manifest))
        .await
        .expect("create");
    session.start().await.expect("start");
    session
}

/// Runs `sh -c <script>` without a shell prefix of the session's own.
async fn printed(session: &dyn SandboxSession, script: &str) -> String {
    let result = session
        .exec(
            ExecRequest::new(["sh".to_owned(), "-c".to_owned(), script.to_owned()])
                .with_shell(ShellInvocation::None),
        )
        .await
        .expect("exec");
    assert_eq!(
        result.exit_code,
        0,
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    String::from_utf8_lossy(&result.stdout).into_owned()
}

fn workspace() -> (tempfile::TempDir, PathBuf) {
    let directory = tempfile::tempdir().expect("temp");
    let root = directory.path().join("workspace");
    (directory, root)
}

// --- inheriting by default ------------------------------------------------------------------

#[test]
fn by_default_a_command_sees_the_whole_host_environment() {
    run_in_child(
        "scenario_inherits_the_host_environment",
        &[
            ("OPENAI_API_KEY", "host-secret"),
            ("LC_MESSAGES", "C"),
            ("LC_CTYPE", "en_US.UTF-8"),
            ("LC_PRIVATE_TOKEN", "locale-secret"),
        ],
    );
}

#[tokio::test]
#[ignore = "run in a prepared child process by its parent test"]
async fn scenario_inherits_the_host_environment() {
    assert_in_child();
    let (_temp, root) = workspace();
    let mut manifest = Manifest::new().with_root(root.to_string_lossy().into_owned());
    manifest.environment = Environment::new()
        .with("HOME", "/manifest-home")
        .with("LC_CTYPE", "POSIX")
        .with("MANIFEST_ONLY", "configured");
    let session = started(&UnixLocalSandboxClient::new(), manifest).await;

    let reported = printed(
        session.as_ref(),
        "printf '%s|%s|%s|%s|%s|%s|%s' \"${OPENAI_API_KEY-unset}\" \"$MANIFEST_ONLY\" \"$HOME\" \
         \"${PATH:+set}\" \"$LC_MESSAGES\" \"$LC_CTYPE\" \"${LC_PRIVATE_TOKEN-unset}\"",
    )
    .await;

    // The permissive default: a credential the SDK was started with reaches the command. The
    // manifest wins over the host, except for `HOME`, which is always the workspace.
    assert_eq!(
        reported,
        format!(
            "host-secret|configured|{}|set|C|POSIX|locale-secret",
            root.to_string_lossy()
        )
    );
}

// --- the standard allowlist -----------------------------------------------------------------

#[test]
fn without_inheritance_a_command_sees_the_standard_allowlist_by_exact_name() {
    run_in_child(
        "scenario_uses_the_default_allowlist",
        &[
            ("HOST_ONLY_VALUE", "host-value"),
            ("LC_MESSAGES", "C"),
            ("LC_PRIVATE_TOKEN", "locale-secret"),
        ],
    );
}

#[tokio::test]
#[ignore = "run in a prepared child process by its parent test"]
async fn scenario_uses_the_default_allowlist() {
    assert_in_child();
    let (_temp, root) = workspace();
    let isolated = UnixLocalSandboxClient::isolated_environment();
    let session = started(
        &isolated,
        Manifest::new().with_root(root.to_string_lossy().into_owned()),
    )
    .await;

    // `LC_MESSAGES` is on the list; `LC_PRIVATE_TOKEN` only shares its prefix.
    assert_eq!(
        printed(
            session.as_ref(),
            "printf '%s|%s|%s' \"${HOST_ONLY_VALUE-unset}\" \"$LC_MESSAGES\" \
             \"${LC_PRIVATE_TOKEN-unset}\"",
        )
        .await,
        "unset|C|unset"
    );
    let state = session.state();
    let payload = isolated.serialize_session_state(&state).expect("serialize");
    let payload = payload.as_object().expect("an object");
    assert!(!payload.contains_key("inherit_host_environment"));
    assert!(!payload.contains_key("host_environment_allowlist"));

    let host_only = "printf '%s' \"${HOST_ONLY_VALUE-unset}\"";
    let resumed = isolated.resume(state.clone()).await.expect("resume");
    resumed.start().await.expect("start");
    assert_eq!(printed(resumed.as_ref(), host_only).await, "unset");

    // The policy is the resuming client's, not something the state carries.
    let inheriting = UnixLocalSandboxClient::new();
    let resumed = inheriting.resume(state).await.expect("resume");
    resumed.start().await.expect("start");
    assert_eq!(printed(resumed.as_ref(), host_only).await, "host-value");
}

// --- a custom allowlist ---------------------------------------------------------------------

#[test]
fn a_custom_allowlist_is_what_a_command_sees_before_and_after_a_resume() {
    run_in_child(
        "scenario_uses_a_custom_allowlist",
        &[
            ("CUSTOM_ALLOWED", "allowed-value"),
            ("HOST_ONLY_VALUE", "host-value"),
        ],
    );
}

#[tokio::test]
#[ignore = "run in a prepared child process by its parent test"]
async fn scenario_uses_a_custom_allowlist() {
    assert_in_child();
    let (_temp, root) = workspace();
    let client = UnixLocalSandboxClient::with_host_environment_allowlist([
        "PATH".to_owned(),
        "CUSTOM_ALLOWED".to_owned(),
    ]);
    let script = "printf '%s|%s' \"$CUSTOM_ALLOWED\" \"${HOST_ONLY_VALUE-unset}\"";
    let session = started(
        &client,
        Manifest::new().with_root(root.to_string_lossy().into_owned()),
    )
    .await;
    assert_eq!(
        printed(session.as_ref(), script).await,
        "allowed-value|unset"
    );

    let resumed = client.resume(session.state()).await.expect("resume");
    resumed.start().await.expect("start");
    assert_eq!(
        printed(resumed.as_ref(), script).await,
        "allowed-value|unset"
    );
}

// --- refusing before creating anything ------------------------------------------------------

#[test]
fn a_split_grant_is_refused_before_any_workspace_is_created() {
    // The child's temporary directory is one this test owns, so what the client creates there — and
    // what it does not — can be counted.
    let temporary = tempfile::tempdir().expect("temp");
    run_in_child(
        "scenario_refuses_a_host_path_grant_before_creating_a_workspace",
        &[("TMPDIR", &temporary.path().to_string_lossy())],
    );
    assert_eq!(
        std::fs::read_dir(temporary.path()).expect("list").count(),
        0,
        "the scenario deleted the one workspace it made on purpose"
    );
}

#[tokio::test]
#[ignore = "run in a prepared child process by its parent test"]
async fn scenario_refuses_a_host_path_grant_before_creating_a_workspace() {
    assert_in_child();
    let temporary = std::env::temp_dir();
    let workspaces = || {
        std::fs::read_dir(&temporary)
            .expect("list")
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("sandbox-local-")
            })
            .count()
    };
    // Any absolute host directory will do; it is never touched. Not one made for the test, which
    // would be made in the directory being counted.
    let grant = SandboxPathGrant::new("/mnt/shared-data")
        .expect("grant")
        .with_host_path("/usr")
        .expect("host path");
    let client = UnixLocalSandboxClient::new();

    let error = client
        .create(CreateRequest::new().with_manifest(Manifest::new().with_path_grant(grant)))
        .await
        .err()
        .expect("a split grant");
    assert!(
        error
            .message()
            .contains("does not support sandbox path grant host_path"),
        "{error}"
    );
    assert_eq!(workspaces(), 0, "a refused create left a workspace behind");

    // The control: the same client, asked for a session it can make, makes its workspace here —
    // so the count above was looking in the right place.
    let session = client.create(CreateRequest::new()).await.expect("create");
    assert_eq!(workspaces(), 1);
    client.delete(session.as_ref()).await.expect("delete");
    assert_eq!(workspaces(), 0);
}
