//! `ra-sandbox::docker` against a real Docker daemon.
//!
//! The fakes in the other Docker test files pin down what the backend asks the daemon for; these
//! check that a real daemon answers the way the backend expects — that a container comes up idle,
//! that commands and files go through `exec`, that the workspace archive survives the trip out and
//! back in, that a published port resolves, and that a delete leaves nothing behind.
//!
//! **Ignored unless asked for**, because they need a daemon and pull an image: run them with
//! `cargo test -p it-sandbox --test docker_daemon -- --ignored`. The daemon is found the way
//! `docker.from_env()` finds it (`DOCKER_HOST`, then the platform's default socket), and the image
//! is `python:3.14-slim` unless `RA_DOCKER_TEST_IMAGE` names another.

use std::sync::Arc;

use ra_core::sandbox::{
    CreateRequest, Entry, ExecRequest, Manifest, SandboxClient, SandboxSession, ShellInvocation,
};
use ra_sandbox::docker::{
    BollardDockerApi, DEFAULT_PYTHON_SANDBOX_IMAGE, DockerApi, DockerNetworkMode,
    DockerSandboxClient, DockerSandboxClientOptions, DockerStateFields,
};

fn image() -> String {
    std::env::var("RA_DOCKER_TEST_IMAGE")
        .unwrap_or_else(|_| DEFAULT_PYTHON_SANDBOX_IMAGE.to_owned())
}

fn daemon() -> Arc<BollardDockerApi> {
    Arc::new(BollardDockerApi::connect_with_defaults().expect("a Docker daemon address"))
}

fn shell(script: &str) -> ExecRequest {
    ExecRequest::new([script.to_owned()])
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[tokio::test]
#[ignore = "needs a Docker daemon; run with --ignored"]
async fn a_container_session_runs_commands_moves_files_and_is_deleted() {
    let api = daemon();
    let client = DockerSandboxClient::new(api.clone());
    let manifest = Manifest::new().with_root("/workspace").with_entry(
        "hello.txt",
        Entry::file(b"hello from the manifest\n".to_vec()),
    );
    let session = client
        .create(
            CreateRequest::new()
                .with_manifest(manifest)
                .with_options(DockerSandboxClientOptions::new(image()).to_payload()),
        )
        .await
        .expect("created");
    session.start().await.expect("started");

    let pwd = session.exec(shell("pwd")).await.expect("pwd");
    assert_eq!(text(&pwd.stdout).trim(), "/workspace");
    assert_eq!(
        text(&session.read("hello.txt", None).await.expect("read")),
        "hello from the manifest\n"
    );

    let binary = (0_u8..=255).cycle().take(300_000).collect::<Vec<_>>();
    session
        .write("nested/data.bin", binary.clone(), None)
        .await
        .expect("write");
    assert_eq!(
        session.read("nested/data.bin", None).await.expect("read"),
        binary
    );
    let listing = session.ls("nested", None).await.expect("ls");
    assert!(
        listing
            .iter()
            .any(|entry| entry.path.ends_with("/data.bin"))
    );

    let missing = session
        .read("missing.txt", None)
        .await
        .expect_err("missing");
    assert_eq!(
        missing.error_code(),
        ra_core::sandbox::ErrorCode::WorkspaceReadNotFound
    );
    let escape = session
        .read("../etc/passwd", None)
        .await
        .expect_err("escape");
    assert_eq!(
        escape.error_code(),
        ra_core::sandbox::ErrorCode::InvalidManifestPath
    );

    let stderr = session
        .exec(
            ExecRequest::new([
                "sh".to_owned(),
                "-c".to_owned(),
                "echo out; echo err >&2; exit 3".to_owned(),
            ])
            .with_shell(ShellInvocation::None),
        )
        .await
        .expect("exec");
    assert_eq!(stderr.exit_code, 3);
    assert_eq!(text(&stderr.stdout), "out\n");
    assert_eq!(text(&stderr.stderr), "err\n");

    let archive = session.persist_workspace().await.expect("persist");
    session.rm("nested", true, None).await.expect("rm");
    session.hydrate_workspace(archive).await.expect("hydrate");
    assert_eq!(
        session.read("nested/data.bin", None).await.expect("read"),
        binary
    );

    let container_id = DockerStateFields::read(&session.state())
        .expect("fields")
        .container_id()
        .to_owned();
    client.delete(session.as_ref()).await.expect("deleted");
    let gone = api
        .inspect_container(&container_id)
        .await
        .expect_err("removed");
    assert!(gone.is_not_found(), "{gone}");
}

#[tokio::test]
#[ignore = "needs a Docker daemon; run with --ignored"]
async fn a_resumed_session_reconnects_to_its_running_container() {
    let api = daemon();
    let client = DockerSandboxClient::new(api.clone());
    let session = client
        .create(
            CreateRequest::new()
                .with_manifest(Manifest::new().with_root("/workspace"))
                .with_options(DockerSandboxClientOptions::new(image()).to_payload()),
        )
        .await
        .expect("created");
    session.start().await.expect("started");
    session
        .write("marker.txt", b"still here".to_vec(), None)
        .await
        .expect("write");

    let resumed = client.resume(session.state()).await.expect("resumed");
    resumed.start().await.expect("started");

    assert_eq!(
        DockerStateFields::read(&resumed.state())
            .expect("fields")
            .container_id(),
        DockerStateFields::read(&session.state())
            .expect("fields")
            .container_id()
    );
    assert_eq!(
        resumed.read("marker.txt", None).await.expect("read"),
        b"still here"
    );
    client.delete(resumed.as_ref()).await.expect("deleted");
}

#[tokio::test]
#[ignore = "needs a Docker daemon; run with --ignored"]
async fn a_published_port_resolves_to_a_loopback_address() {
    let api = daemon();
    let client = DockerSandboxClient::new(api);
    let options = DockerSandboxClientOptions::new(image())
        .with_exposed_ports([8765])
        .expect("ports");
    let session = client
        .create(CreateRequest::new().with_options(options.to_payload()))
        .await
        .expect("created");
    session.start().await.expect("started");

    let endpoint = session.resolve_exposed_port(8765).await.expect("resolved");

    assert_eq!(endpoint.host, "127.0.0.1");
    assert!(endpoint.port > 0);
    client.delete(session.as_ref()).await.expect("deleted");
}

#[tokio::test]
#[ignore = "needs a Docker daemon; run with --ignored"]
async fn network_mode_none_leaves_the_container_on_no_network() {
    let api = daemon();
    let client = DockerSandboxClient::new(api.clone());
    let options = DockerSandboxClientOptions::new(image())
        .with_network_mode(Some(DockerNetworkMode::None))
        .expect("mode");
    let session = client
        .create(CreateRequest::new().with_options(options.to_payload()))
        .await
        .expect("created");
    session.start().await.expect("started");

    let container_id = DockerStateFields::read(&session.state())
        .expect("fields")
        .container_id()
        .to_owned();
    let attrs = api.inspect_container(&container_id).await.expect("inspect");
    assert_eq!(attrs["HostConfig"]["NetworkMode"], "none");
    let interfaces = session
        .exec(shell("ls /sys/class/net"))
        .await
        .expect("exec");
    assert_eq!(text(&interfaces.stdout).trim(), "lo");
    client.delete(session.as_ref()).await.expect("deleted");
}
