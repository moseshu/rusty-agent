//! `ra-sandbox::unix_local`: what actually gets run, and in what environment.
//!
//! Running a command locally is four decisions stacked on each other — which shell, how the
//! arguments are quoted, where the process starts, what it can see — and each one is observable
//! from inside the command. These tests read the answers back out of the command itself rather than
//! inspecting the argument vector, because the argument vector is not the contract; what the
//! command experiences is.

use std::path::{Path, PathBuf};

use ra_core::sandbox::{
    CreateRequest, Environment, ErrorCode, ExecRequest, Manifest, SandboxClient, SandboxSession,
    ShellInvocation,
};
use ra_sandbox::unix_local::UnixLocalSandboxClient;

/// A workspace whose path contains no symlinks, so "as written" and "as resolved" agree.
///
/// Worth the extra step: a macOS temporary directory is reached through `/var -> /private/var`, and
/// the workspace-relative argument rewriting compares against the resolved root. Testing against
/// the unresolved spelling would test the symlink rather than the rewriting.
fn workspace() -> (tempfile::TempDir, PathBuf) {
    let directory = tempfile::tempdir().expect("temp");
    let resolved = std::fs::canonicalize(directory.path()).expect("canonical");
    (directory, resolved)
}

/// Opens a started session over `root`.
async fn session_at(client: &UnixLocalSandboxClient, root: &Path) -> Box<dyn SandboxSession> {
    session_with(
        client,
        Manifest::new().with_root(root.to_string_lossy().into_owned()),
    )
    .await
}

/// Opens a started session over `manifest`.
async fn session_with(
    client: &UnixLocalSandboxClient,
    manifest: Manifest,
) -> Box<dyn SandboxSession> {
    let session = client
        .create(CreateRequest::new().with_manifest(manifest))
        .await
        .expect("create");
    session.start().await.expect("start");
    session
}

/// Runs a shell script with no shell wrapper of the session's own.
async fn script(session: &dyn SandboxSession, body: &str) -> String {
    let result = session
        .exec(
            ExecRequest::new(["sh".to_owned(), "-c".to_owned(), body.to_owned()])
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

#[tokio::test]
async fn the_default_shell_is_not_a_login_shell() {
    let (_temp, root) = workspace();
    // `HOME` is the workspace, so this is the profile a login shell would read.
    std::fs::write(
        root.join(".profile"),
        b"PROFILE_SOURCED=yes\nexport PROFILE_SOURCED\n",
    )
    .expect("profile");

    let client = UnixLocalSandboxClient::new();
    let session = session_at(&client, &root).await;
    let result = session
        .exec(ExecRequest::new([
            "printf '%s' \"${PROFILE_SOURCED-unset}\"".to_owned(),
        ]))
        .await
        .expect("exec");

    // The protocol's default is `sh -lc`; this backend deliberately uses `sh -c`, so a command does
    // not inherit whatever the workspace's profile happens to set.
    assert_eq!(String::from_utf8_lossy(&result.stdout), "unset");
    client.delete(session.as_ref()).await.expect("delete");
}

#[tokio::test]
async fn several_arguments_become_one_quoted_command_line() {
    let (_temp, root) = workspace();
    let client = UnixLocalSandboxClient::new();
    let session = session_at(&client, &root).await;

    let result = session
        .exec(ExecRequest::new([
            "printf".to_owned(),
            "[%s]".to_owned(),
            "two words".to_owned(),
        ]))
        .await
        .expect("exec");

    // Quoted back into one line rather than concatenated: an argument with a space is still one
    // argument when the shell has finished with it.
    assert_eq!(String::from_utf8_lossy(&result.stdout), "[two words]");
    client.delete(session.as_ref()).await.expect("delete");
}

#[tokio::test]
async fn an_argument_vector_can_be_run_with_no_shell_at_all() {
    let (_temp, root) = workspace();
    let client = UnixLocalSandboxClient::new();
    let session = session_at(&client, &root).await;

    let result = session
        .exec(
            ExecRequest::new(["printf".to_owned(), "%s".to_owned(), "$HOME".to_owned()])
                .with_shell(ShellInvocation::None),
        )
        .await
        .expect("exec");

    // No shell means no expansion: the dollar sign is part of the argument.
    assert_eq!(String::from_utf8_lossy(&result.stdout), "$HOME");
    client.delete(session.as_ref()).await.expect("delete");
}

#[tokio::test]
async fn an_absolute_argument_inside_the_workspace_is_rewritten_relative_to_it() {
    let (_temp, root) = workspace();
    let client = UnixLocalSandboxClient::new();
    let session = session_at(&client, &root).await;

    let inside = root.join("notes.md").to_string_lossy().into_owned();
    let outside = "/etc/hosts".to_owned();
    let result = session
        .exec(
            ExecRequest::new([
                "sh".to_owned(),
                "-c".to_owned(),
                r#"printf '%s|%s' "$1" "$2""#.to_owned(),
                "sh".to_owned(),
                inside,
                outside,
            ])
            .with_shell(ShellInvocation::None),
        )
        .await
        .expect("exec");

    // A path inside the workspace loses the host prefix — a command line should not carry the
    // directory a provider happened to pick — and one outside it is the caller's business.
    assert_eq!(
        String::from_utf8_lossy(&result.stdout),
        "notes.md|/etc/hosts"
    );
    client.delete(session.as_ref()).await.expect("delete");
}

#[tokio::test]
async fn a_command_runs_in_the_workspace_with_the_workspace_as_its_home() {
    let (_temp, root) = workspace();
    let client = UnixLocalSandboxClient::new();
    let session = session_at(&client, &root).await;

    let reported = script(session.as_ref(), r#"printf '%s|%s' "$PWD" "$HOME""#).await;
    assert_eq!(
        reported,
        format!("{}|{}", root.to_string_lossy(), root.to_string_lossy())
    );
    client.delete(session.as_ref()).await.expect("delete");
}

#[tokio::test]
async fn the_manifest_environment_wins_over_the_host_and_loses_to_the_workspace_home() {
    let (_temp, root) = workspace();
    let mut manifest = Manifest::new().with_root(root.to_string_lossy().into_owned());
    manifest.environment = Environment::new()
        .with("HOME", "/manifest-home")
        .with("CARGO_PKG_NAME", "from-manifest")
        .with("MANIFEST_ONLY", "configured");
    let client = UnixLocalSandboxClient::new();
    let session = session_with(&client, manifest).await;

    let reported = script(
        session.as_ref(),
        r#"printf '%s|%s|%s' "$HOME" "$CARGO_PKG_NAME" "$MANIFEST_ONLY""#,
    )
    .await;

    // The manifest overrides a host variable, because it is the configuration written for this
    // workspace. `HOME` is the exception: the session sets it last, so a command's home is the
    // workspace it is working in whatever the manifest says.
    assert_eq!(
        reported,
        format!("{}|from-manifest|configured", root.to_string_lossy())
    );
    client.delete(session.as_ref()).await.expect("delete");
}

#[tokio::test]
async fn a_custom_allowlist_is_exactly_what_reaches_a_command() {
    let (_temp, root) = workspace();
    let client = UnixLocalSandboxClient::with_host_environment_allowlist([
        "PATH".to_owned(),
        "CARGO_PKG_NAME".to_owned(),
    ]);
    let session = session_at(&client, &root).await;

    let reported = script(
        session.as_ref(),
        r#"printf '%s|%s|%s' "$CARGO_PKG_NAME" "${CARGO_MANIFEST_DIR-unset}" "${PATH:+set}""#,
    )
    .await;
    assert_eq!(reported, "it-sandbox|unset|set");
    client.delete(session.as_ref()).await.expect("delete");
}

#[tokio::test]
async fn a_timeout_ends_everything_the_command_started() {
    let (_temp, root) = workspace();
    let client = UnixLocalSandboxClient::new();
    let session = session_at(&client, &root).await;

    // The background subshell outlives the command it was started from. Killing only the process
    // that was started would leave it running, and it would write its marker a second later.
    let result = session
        .exec(
            ExecRequest::new(["(sleep 1; echo late > marker.txt) & sleep 30".to_owned()])
                .with_timeout_s(0.3),
        )
        .await;
    let error = result.expect_err("a command that outran its deadline");
    assert_eq!(error.error_code(), ErrorCode::ExecTimeout);

    tokio::time::sleep(std::time::Duration::from_millis(2_000)).await;
    assert!(
        !root.join("marker.txt").exists(),
        "the whole process group is ended, not just the command"
    );
    client.delete(session.as_ref()).await.expect("delete");
}

#[tokio::test]
async fn a_command_with_no_workspace_to_run_in_is_refused() {
    let (temp, root) = workspace();
    let client = UnixLocalSandboxClient::new();
    let session = session_at(&client, &root).await;
    drop(temp);

    let error = session
        .exec(ExecRequest::new(["true".to_owned()]))
        .await
        .expect_err("a workspace that is no longer there");
    assert_eq!(error.error_code(), ErrorCode::WorkspaceRootNotFound);
}

#[tokio::test]
async fn a_non_zero_exit_is_a_result_and_the_two_streams_stay_apart() {
    let (_temp, root) = workspace();
    let client = UnixLocalSandboxClient::new();
    let session = session_at(&client, &root).await;

    let result = session
        .exec(ExecRequest::new([
            "printf out; printf err >&2; exit 3".to_owned()
        ]))
        .await
        .expect("a command that ran");

    // A command that ran and failed is an answer, not a transport failure. A caller handed one
    // merged buffer could not tell which bytes were the diagnostics.
    assert_eq!(result.exit_code, 3);
    assert!(!result.ok());
    assert_eq!(result.stdout, b"out");
    assert_eq!(result.stderr, b"err");
    client.delete(session.as_ref()).await.expect("delete");
}

#[tokio::test]
async fn a_signalled_command_reports_the_signal_that_ended_it() {
    let (_temp, root) = workspace();
    let client = UnixLocalSandboxClient::new();
    let session = session_at(&client, &root).await;

    let result = session
        .exec(ExecRequest::new(["kill -TERM $$".to_owned()]))
        .await
        .expect("a command that ran");

    // Negated, so "was this interrupted" is answerable. Reporting zero would make a killed command
    // indistinguishable from one that exited cleanly.
    assert_eq!(result.exit_code, -15);
    client.delete(session.as_ref()).await.expect("delete");
}
