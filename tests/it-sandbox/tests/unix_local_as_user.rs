//! `ra-sandbox::unix_local`: file operations on behalf of another account.
//!
//! Naming a user does not move the work into `sudo`. The session asks, as that account, whether the
//! operation would be allowed, and then does it itself — except a write, which the account really
//! performs so that the file is its own. The reference tests this with a subclass that records the
//! command instead of running it; there is no subclass to write here, and a real second account with
//! password-free `sudo` is not something a test machine has.
//!
//! So the manifest puts a stand-in `sudo` first on the search path. It writes down exactly how it
//! was called and then runs the rest of the command as the account running the tests. Everything
//! between the session and the process — the shell prefix, the workspace-relative rewrite, the
//! macOS fence — is the real thing, and what the stand-in records is the argument vector that would
//! have reached the real `sudo`.

use std::path::PathBuf;

use ra_core::sandbox::{
    CreateRequest, Environment, ErrorCode, Manifest, SandboxClient, SandboxPathGrant,
    SandboxSession, User,
};
use ra_sandbox::unix_local::UnixLocalSandboxClient;

/// The stand-in `sudo`: records its arguments, NUL-separated, one file per call, then runs what
/// follows `-u <name> --`. The account named `broken` is refused, the way a real `sudo` refuses an
/// account it cannot switch to.
const FAKE_SUDO: &str = r#"#!/bin/sh
n=0
while [ -e "$SUDO_LOG/$n" ]; do n=$((n + 1)); done
for argument in "$@"; do printf '%s\0' "$argument"; done > "$SUDO_LOG/$n"
if [ "$2" = broken ]; then
    echo "sudo: unable to switch to broken" >&2
    exit 2
fi
shift 3
exec "$@"
"#;

/// A started session whose commands find the stand-in `sudo` first.
struct Fixture {
    _workspace: tempfile::TempDir,
    _tools: tempfile::TempDir,
    root: PathBuf,
    log: PathBuf,
    session: Box<dyn SandboxSession>,
}

impl Fixture {
    async fn new() -> Self {
        let workspace = tempfile::tempdir().expect("workspace");
        let root = std::fs::canonicalize(workspace.path()).expect("canonical");
        let tools = tempfile::tempdir().expect("tools");
        let tools_root = std::fs::canonicalize(tools.path()).expect("canonical");
        let bin = tools_root.join("bin");
        let log = tools_root.join("log");
        std::fs::create_dir_all(&bin).expect("bin");
        std::fs::create_dir_all(&log).expect("log");
        std::fs::write(bin.join("sudo"), FAKE_SUDO).expect("sudo");
        std::fs::set_permissions(
            bin.join("sudo"),
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .expect("mode");

        let mut manifest = Manifest::new()
            .with_root(root.to_string_lossy().into_owned())
            // The recorder has to be able to write its log from inside the macOS fence, and a
            // grant is how a manifest says a command may write somewhere other than the workspace.
            .with_path_grant(SandboxPathGrant::new(&tools_root.to_string_lossy()).expect("grant"));
        manifest.environment = Environment::new()
            .with("PATH", format!("{}:/usr/bin:/bin", bin.to_string_lossy()))
            .with("SUDO_LOG", log.to_string_lossy().into_owned());
        let session = UnixLocalSandboxClient::new()
            .create(CreateRequest::new().with_manifest(manifest))
            .await
            .expect("create");
        session.start().await.expect("start");
        Self {
            _workspace: workspace,
            _tools: tools,
            root,
            log,
            session,
        }
    }

    /// Every call the stand-in `sudo` received, in order.
    fn calls(&self) -> Vec<Vec<String>> {
        (0..)
            .map(|index| self.log.join(index.to_string()))
            .take_while(|path| path.exists())
            .map(|path| {
                let raw = std::fs::read(path).expect("call");
                raw.split(|byte| *byte == 0)
                    .filter(|argument| !argument.is_empty())
                    .map(|argument| String::from_utf8_lossy(argument).into_owned())
                    .collect()
            })
            .collect()
    }
}

fn account(name: &str) -> Option<User> {
    Some(User::new(name))
}

fn context<'a>(error: &'a ra_core::sandbox::SandboxError, key: &str) -> &'a serde_json::Value {
    error
        .context()
        .get(key)
        .unwrap_or_else(|| panic!("no `{key}` in {:?}", error.context()))
}

/// Asserts a call was the account wrapper around an access check with these trailing arguments.
fn assert_checked_as(call: &[String], name: &str, trailing: &[&str]) {
    assert_eq!(call[..3], ["-u", name, "--"], "{call:?}");
    assert_eq!(call[3..5], ["sh", "-lc"], "{call:?}");
    assert_eq!(call[call.len() - trailing.len()..], *trailing, "{call:?}");
}

#[tokio::test]
async fn mkdir_as_another_account_asks_as_that_account_and_then_creates_it_locally() {
    let fixture = Fixture::new().await;

    fixture
        .session
        .mkdir("nested", false, account("sandbox-user"))
        .await
        .expect("mkdir");

    assert!(fixture.root.join("nested").is_dir());
    let calls = fixture.calls();
    assert_eq!(calls.len(), 1, "{calls:?}");
    // The reference records the command before the workspace-relative rewrite and so sees the
    // absolute path; this is what reached the process, after it.
    assert_checked_as(&calls[0], "sandbox-user", &["nested", "0"]);
    assert!(
        !calls[0].iter().any(|part| part.starts_with("mkdir ")),
        "the directory is made locally, not by the account: {calls:?}"
    );
}

#[tokio::test]
async fn rm_as_another_account_asks_as_that_account_and_then_removes_it_locally() {
    let fixture = Fixture::new().await;
    std::fs::write(fixture.root.join("stale.txt"), b"stale").expect("stale");

    fixture
        .session
        .rm("stale.txt", false, account("sandbox-user"))
        .await
        .expect("rm");

    assert!(!fixture.root.join("stale.txt").exists());
    let calls = fixture.calls();
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert_checked_as(&calls[0], "sandbox-user", &["stale.txt", "0"]);
    assert!(
        !calls[0].iter().any(|part| part.starts_with("rm ")),
        "the file is removed locally, not by the account: {calls:?}"
    );
}

#[tokio::test]
async fn an_operation_the_account_may_not_perform_is_refused_before_it_happens() {
    let fixture = Fixture::new().await;

    // The check answers "no" for a non-recursive removal of something that is not there, which is
    // the one refusal the check script can be made to give without a second account.
    let error = fixture
        .session
        .rm("missing.txt", false, account("sandbox-user"))
        .await
        .expect_err("refused by the check");
    assert_eq!(error.error_code(), ErrorCode::WorkspaceArchiveWriteError);
    let path = fixture
        .root
        .join("missing.txt")
        .to_string_lossy()
        .into_owned();
    assert_eq!(
        context(&error, "command"),
        &serde_json::json!(["sh", "-lc", "<rm_access_check>", path, "0"])
    );
}

#[tokio::test]
async fn a_readable_file_is_read_after_one_check() {
    let fixture = Fixture::new().await;
    std::fs::write(fixture.root.join("notes.md"), b"hello").expect("notes");

    let content = fixture
        .session
        .read("notes.md", account("sandbox-user"))
        .await
        .expect("read");

    assert_eq!(content, b"hello");
    let calls = fixture.calls();
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert_checked_as(&calls[0], "sandbox-user", &["notes.md"]);
    assert_eq!(calls[0][5], r#"[ -r "$1" ]"#);
}

#[tokio::test]
async fn a_missing_file_is_told_apart_from_an_unreadable_one_by_probing_as_the_account() {
    let fixture = Fixture::new().await;

    let error = fixture
        .session
        .read("missing.txt", account("sandbox-user"))
        .await
        .expect_err("missing");

    assert_eq!(error.error_code(), ErrorCode::WorkspaceReadNotFound);
    let path = fixture
        .root
        .join("missing.txt")
        .to_string_lossy()
        .into_owned();
    assert_eq!(
        context(&error, "command"),
        &serde_json::json!(["sh", "-lc", "<read_access_check>", path])
    );
    assert_eq!(context(&error, "stdout_bytes"), &serde_json::json!(0));
    assert_eq!(
        context(&error, "existence_probe_exit_code"),
        &serde_json::json!(1)
    );
    assert_eq!(
        context(&error, "existence_probe_stdout_bytes"),
        &serde_json::json!(0)
    );

    // The probe runs as the same account, with a plain shell, and is the reference's script. It is
    // handed the absolute path, not the workspace-relative form every other command gets: it
    // resolves a relative path as though it began at `/`, which is how the reference itself reports
    // an unreadable file as missing on this backend (see the next test).
    let calls = fixture.calls();
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert_eq!(calls[1][..5], ["-u", "sandbox-user", "--", "sh", "-c"]);
    assert!(calls[1][5].starts_with("# READ_PATH_PROBE_V3\n"));
    assert_eq!(calls[1][6..], ["sh".to_owned(), path]);
}

#[tokio::test]
async fn a_file_that_is_there_but_unreadable_is_a_read_failure_not_a_missing_file() {
    let fixture = Fixture::new().await;
    let locked = fixture.root.join("locked.txt");
    std::fs::write(&locked, b"secret").expect("locked");
    std::fs::set_permissions(&locked, std::os::unix::fs::PermissionsExt::from_mode(0o000))
        .expect("mode");
    if std::fs::File::open(&locked).is_ok() {
        // Running with the privilege to read anything: the check can never say no.
        eprintln!("skipped: this account can read a file with no permissions");
        return;
    }

    let error = fixture
        .session
        .read("locked.txt", account("sandbox-user"))
        .await
        .expect_err("unreadable");

    // The reference answers `WorkspaceReadNotFound` here on this backend (checked against its own
    // code): its probe is handed the workspace-relative path and looks for `/locked.txt`. The file
    // is there, and the answer here says so.
    assert_eq!(error.error_code(), ErrorCode::WorkspaceArchiveReadError);
    assert_eq!(
        context(&error, "existence_probe_exit_code"),
        &serde_json::json!(0)
    );
}

#[tokio::test]
async fn an_account_that_cannot_be_switched_to_is_a_read_failure_without_a_probe() {
    let fixture = Fixture::new().await;
    std::fs::write(fixture.root.join("notes.md"), b"hello").expect("notes");

    let error = fixture
        .session
        .read("notes.md", account("broken"))
        .await
        .expect_err("the account switch failed");

    // Only an exit of 1 is the check saying no. Anything else is a failure to ask, and probing
    // through the same broken switch would not tell missing from unreadable.
    assert_eq!(error.error_code(), ErrorCode::WorkspaceArchiveReadError);
    assert!(
        context(&error, "stderr")
            .as_str()
            .is_some_and(|stderr| stderr.contains("unable to switch to broken")),
        "{:?}",
        error.context()
    );
    assert!(!error.context().contains_key("existence_probe_exit_code"));
    assert_eq!(fixture.calls().len(), 1);
}

#[tokio::test]
async fn a_write_as_another_account_is_performed_by_that_account() {
    let fixture = Fixture::new().await;

    fixture
        .session
        .write("out/data.txt", b"payload".to_vec(), account("sandbox-user"))
        .await
        .expect("write");

    assert_eq!(
        std::fs::read(fixture.root.join("out/data.txt")).expect("written"),
        b"payload"
    );
    let calls = fixture.calls();
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert_eq!(
        calls[0],
        [
            "-u",
            "sandbox-user",
            "--",
            "sh",
            "-c",
            r#"mkdir -p "$(dirname "$1")" && cat > "$1""#,
            "sh",
            "out/data.txt",
        ]
    );
}

#[tokio::test]
async fn a_listing_as_another_account_is_that_accounts_ls() {
    let fixture = Fixture::new().await;
    std::fs::write(fixture.root.join("a.txt"), b"a").expect("a");

    let entries = fixture
        .session
        .ls("", account("sandbox-user"))
        .await
        .expect("ls");

    let expected = fixture.root.join("a.txt").to_string_lossy().into_owned();
    assert!(
        entries.iter().any(|entry| entry.path == expected),
        "{entries:?}"
    );
    let calls = fixture.calls();
    assert_eq!(
        calls,
        [["-u", "sandbox-user", "--", "ls", "-la", "--", "."]]
    );
}
