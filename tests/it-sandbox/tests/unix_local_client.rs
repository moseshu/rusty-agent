//! `ra-sandbox::unix_local`: who owns a local workspace, and what that lets them do to it.
//!
//! The client is the only party allowed to delete a workspace directory, and the whole question is
//! whether it created that directory or was handed one. Getting it backwards in either direction is
//! expensive: delete a caller's project directory, or leak one temporary directory per run.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ra_core::sandbox::{
    CreateRequest, Dependencies, DependencyValue, DiscriminatedPayload, ErrorCode, ExecRequest,
    FactoryOptions, Manifest, SandboxClient, SandboxPathGrant, SandboxSession, ShellInvocation,
    Snapshot, SnapshotSpec, dependency_factory,
};
use ra_sandbox::snapshot::{
    RemoteSnapshotClient, RemoteSnapshotError, remote_snapshot_client_dependency,
};
use ra_sandbox::unix_local::{
    UNIX_LOCAL_BACKEND_ID, UnixLocalSandboxClient, UnixLocalSandboxClientOptions,
    workspace_root_owned,
};

/// A manifest rooted at a directory the caller owns.
fn manifest_at(root: &std::path::Path) -> Manifest {
    Manifest::new().with_root(root.to_string_lossy().into_owned())
}

/// Reads one variable out of a started session.
async fn read_variable(session: &dyn SandboxSession, name: &str) -> String {
    let script = format!("printf '%s' \"${{{name}-unset}}\"");
    let result = session
        .exec(
            ExecRequest::new(["sh".to_owned(), "-c".to_owned(), script])
                .with_shell(ShellInvocation::None),
        )
        .await
        .expect("exec");
    String::from_utf8_lossy(&result.stdout).into_owned()
}

#[tokio::test]
async fn a_session_with_no_root_of_its_own_gets_a_private_directory_and_owns_it() {
    let client = UnixLocalSandboxClient::new();
    let session = client.create(CreateRequest::new()).await.expect("create");
    let state = session.state();

    // The manifest default root is a container path. Used literally it would be one directory at
    // the filesystem root shared by every session that ever ran.
    let root = std::path::PathBuf::from(&state.manifest().root);
    assert_ne!(state.manifest().root, "/workspace");
    assert!(
        root.file_name()
            .expect("a name")
            .to_string_lossy()
            .starts_with("sandbox-local-")
    );
    assert!(workspace_root_owned(&state));
    assert_eq!(state.state_type(), UNIX_LOCAL_BACKEND_ID);

    session.start().await.expect("start");
    assert!(root.is_dir());
    session.close().await.expect("close");
    client.delete(session.as_ref()).await.expect("delete");
    assert!(!root.exists(), "an owned workspace is removed by delete");
}

#[tokio::test]
async fn a_caller_supplied_root_is_never_deleted() {
    let supplied = tempfile::tempdir().expect("temp");
    let client = UnixLocalSandboxClient::new();
    let session = client
        .create(CreateRequest::new().with_manifest(manifest_at(supplied.path())))
        .await
        .expect("create");

    assert!(!workspace_root_owned(&session.state()));
    session.start().await.expect("start");
    std::fs::write(supplied.path().join("notes.md"), b"mine").expect("write");

    client.delete(session.as_ref()).await.expect("delete");
    assert!(
        supplied.path().join("notes.md").is_file(),
        "a directory the caller handed in survives its session"
    );
}

#[tokio::test]
async fn start_recreates_a_workspace_that_is_gone_and_records_that_it_is_there() {
    let client = UnixLocalSandboxClient::new();
    let session = client.create(CreateRequest::new()).await.expect("create");
    let root = std::path::PathBuf::from(&session.state().manifest().root);
    std::fs::remove_dir_all(&root).expect("remove the workspace out from under the session");

    assert!(!session.state().workspace_root_ready());
    session.start().await.expect("start");
    assert!(root.is_dir());
    // Written down only after the start finished, so a later resume can tell a preserved workspace
    // from one this start had to create.
    assert!(session.state().workspace_root_ready());

    client.delete(session.as_ref()).await.expect("delete");
}

#[tokio::test]
async fn a_grant_that_names_a_separate_host_source_is_refused() {
    let source = tempfile::tempdir().expect("temp");
    let grant = SandboxPathGrant::new("/mnt/shared-data")
        .expect("grant")
        .with_host_path(&source.path().to_string_lossy())
        .expect("host path");
    let client = UnixLocalSandboxClient::new();

    let error = client
        .create(CreateRequest::new().with_manifest(Manifest::new().with_path_grant(grant)))
        .await
        .err()
        .expect("a split grant a single filesystem cannot honour");
    assert_eq!(error.error_code(), ErrorCode::SandboxConfigInvalid);
    assert_eq!(
        error
            .context()
            .get("grant_path")
            .and_then(serde_json::Value::as_str),
        Some("/mnt/shared-data")
    );
}

#[tokio::test]
async fn a_manifest_that_asks_for_accounts_is_refused_when_it_would_be_materialized() {
    let workspace = tempfile::tempdir().expect("temp");
    let client = UnixLocalSandboxClient::new();
    let session = client
        .create(CreateRequest::new().with_manifest(
            manifest_at(workspace.path()).with_user(ra_core::sandbox::User::new("build")),
        ))
        .await
        .expect("holding such a manifest is fine: another backend could honour it");

    // The refusal lands where the accounts would actually be created, which on this backend means
    // on the developer's own machine.
    let error = session
        .start()
        .await
        .expect_err("provisioning would run on the host");
    assert_eq!(error.error_code(), ErrorCode::SandboxConfigInvalid);
}

#[tokio::test]
async fn options_addressed_to_another_backend_are_refused() {
    let client = UnixLocalSandboxClient::new();
    let error = client
        .create(CreateRequest::new().with_options(DiscriminatedPayload::new("docker")))
        .await
        .err()
        .expect("options for a backend this client does not speak for");
    assert_eq!(error.error_code(), ErrorCode::SandboxConfigInvalid);
    assert_eq!(
        error
            .context()
            .get("options_type")
            .and_then(serde_json::Value::as_str),
        Some("docker")
    );
}

#[tokio::test]
async fn a_published_port_resolves_to_this_machine_and_an_unpublished_one_does_not() {
    let options = UnixLocalSandboxClientOptions::new()
        .with_exposed_ports([8080])
        .expect("ports");
    let client = UnixLocalSandboxClient::new();
    let session = client
        .create(CreateRequest::new().with_options(options.to_payload()))
        .await
        .expect("create");

    let endpoint = session.resolve_exposed_port(8080).await.expect("endpoint");
    // There is no forwarding to do: the sandbox and the host are the same machine.
    assert_eq!(endpoint.host, "127.0.0.1");
    assert_eq!(endpoint.port, 8080);

    let error = session
        .resolve_exposed_port(9090)
        .await
        .expect_err("a port nobody published");
    assert_eq!(error.error_code(), ErrorCode::ExposedPortUnavailable);
    assert_eq!(
        error
            .context()
            .get("reason")
            .and_then(serde_json::Value::as_str),
        Some("not_configured")
    );
    assert_eq!(error.retryable(), Some(false));

    client.delete(session.as_ref()).await.expect("delete");
}

#[tokio::test]
async fn a_session_takes_the_storage_it_was_asked_for_and_is_named_after_itself() {
    let client = UnixLocalSandboxClient::new();
    let snapshots = tempfile::tempdir().expect("temp");

    // Told where to keep snapshots but not what to call one: the session is the only thing that
    // can say, and it has not been created yet when the caller writes the run configuration down.
    let session = client
        .create(CreateRequest::new().with_snapshot_spec(SnapshotSpec::Local {
            base_path: snapshots.path().to_path_buf(),
        }))
        .await
        .expect("create");
    let state = session.state();

    assert_eq!(
        state.snapshot(),
        &ra_core::sandbox::Snapshot::local(state.session_id().to_string(), snapshots.path())
            .expect("named")
    );

    // Told nothing: still a snapshot, still named after the session, and it stores nothing.
    let plain = client.create(CreateRequest::new()).await.expect("create");
    let plain_state = plain.state();
    assert!(plain_state.snapshot().is_noop());
    assert_eq!(
        plain_state.snapshot().id(),
        plain_state.session_id().to_string()
    );

    client.delete(session.as_ref()).await.expect("delete");
    client.delete(plain.as_ref()).await.expect("delete");
}

#[tokio::test]
async fn a_state_from_another_backend_is_refused() {
    let client = UnixLocalSandboxClient::new();
    let foreign = ra_core::sandbox::SandboxSessionState::new(
        "docker",
        ra_core::sandbox::Snapshot::noop(),
        Manifest::new(),
    );
    let error = client
        .resume(foreign)
        .await
        .err()
        .expect("a state this backend cannot read");
    assert_eq!(error.error_code(), ErrorCode::SandboxConfigInvalid);
}

#[tokio::test]
async fn the_environment_policy_comes_from_the_resuming_client_not_from_the_state() {
    let workspace = tempfile::tempdir().expect("temp");
    let isolated = UnixLocalSandboxClient::isolated_environment();
    let session = isolated
        .create(CreateRequest::new().with_manifest(manifest_at(workspace.path())))
        .await
        .expect("create");
    session.start().await.expect("start");
    assert_eq!(
        read_variable(session.as_ref(), "CARGO_PKG_NAME").await,
        "unset"
    );
    let state = session.state();

    // The policy is the host's decision about its own machine, so it must not travel in the state.
    let payload = isolated
        .serialize_session_state(&state)
        .expect("serialize")
        .as_object()
        .expect("an object")
        .clone();
    assert!(!payload.contains_key("host_environment_allowlist"));
    assert!(!payload.contains_key("inherit_host_environment"));

    let resumed = UnixLocalSandboxClient::new()
        .resume(state.clone())
        .await
        .expect("resume");
    resumed.start().await.expect("start");
    assert_eq!(
        read_variable(resumed.as_ref(), "CARGO_PKG_NAME").await,
        "it-sandbox",
        "a client that inherits the host environment sees it, whoever created the session"
    );

    let resumed_isolated = isolated.resume(state).await.expect("resume");
    resumed_isolated.start().await.expect("start");
    assert_eq!(
        read_variable(resumed_isolated.as_ref(), "CARGO_PKG_NAME").await,
        "unset",
        "and a client that closes it keeps it closed"
    );
}

/// Remote storage in memory.
#[derive(Default)]
struct MemoryRemote {
    stored: Mutex<BTreeMap<String, Vec<u8>>>,
}

#[async_trait]
impl RemoteSnapshotClient for MemoryRemote {
    async fn upload(&self, snapshot_id: &str, data: Vec<u8>) -> Result<(), RemoteSnapshotError> {
        self.stored
            .lock()
            .expect("stored")
            .insert(snapshot_id.to_owned(), data);
        Ok(())
    }

    async fn download(&self, snapshot_id: &str) -> Result<Vec<u8>, RemoteSnapshotError> {
        self.stored
            .lock()
            .expect("stored")
            .get(snapshot_id)
            .cloned()
            .ok_or_else(|| RemoteSnapshotError::new(format!("no snapshot {snapshot_id}")))
    }

    async fn exists(&self, snapshot_id: &str) -> Result<bool, RemoteSnapshotError> {
        Ok(self
            .stored
            .lock()
            .expect("stored")
            .contains_key(snapshot_id))
    }
}

#[tokio::test]
async fn a_session_persists_to_the_remote_client_its_dependencies_name() {
    let workspace = tempfile::tempdir().expect("temp");
    let remote = Arc::new(MemoryRemote::default());
    let session = UnixLocalSandboxClient::new()
        .create(
            CreateRequest::new()
                .with_manifest(manifest_at(workspace.path()))
                .with_snapshot(Snapshot::remote("snap-123", "tests.remote_snapshot_client")),
        )
        .await
        .expect("create");
    // Bound before start: starting asks the remote storage whether there is anything to restore.
    let dependencies = Dependencies::new();
    dependencies
        .bind_value(
            "tests.remote_snapshot_client",
            remote_snapshot_client_dependency(remote.clone()),
            false,
        )
        .expect("bound");
    session.set_dependencies(Some(Arc::new(dependencies)));
    session.start().await.expect("start");
    std::fs::write(workspace.path().join("notes.txt"), b"kept").expect("write");

    session.stop().await.expect("stop");

    let stored = remote.stored.lock().expect("stored");
    assert!(stored.contains_key("snap-123"));
    assert!(!stored["snap-123"].is_empty());
}

#[tokio::test]
async fn every_session_gets_its_own_copy_of_the_client_dependencies() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    let template = Dependencies::new();
    template
        .bind_factory(
            "tests.per_session",
            FactoryOptions::default(),
            dependency_factory(move |_| {
                let call = counter.fetch_add(1, Ordering::SeqCst);
                async move { Ok(DependencyValue::new(Arc::new(call))) }
            }),
        )
        .expect("bound");
    let client = UnixLocalSandboxClient::new().with_dependencies(template);
    let first = client.create(CreateRequest::new()).await.expect("first");
    let second = client.create(CreateRequest::new()).await.expect("second");

    let from_first = first
        .dependencies()
        .require_as::<usize>("tests.per_session", None)
        .await
        .expect("first value");
    let again_from_first = first
        .dependencies()
        .require_as::<usize>("tests.per_session", None)
        .await
        .expect("cached");
    let from_second = second
        .dependencies()
        .require_as::<usize>("tests.per_session", None)
        .await
        .expect("second value");

    // Each session ran the factory in its own container, and caches only its own result.
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(Arc::ptr_eq(&from_first, &again_from_first));
    assert_ne!(*from_first, *from_second);

    // Closing one session releases its container, not the other's.
    first.close().await.expect("close");
    assert!(first.dependencies().is_closed());
    assert!(!second.dependencies().is_closed());
    for session in [first, second] {
        client.delete(session.as_ref()).await.expect("delete");
    }
}
