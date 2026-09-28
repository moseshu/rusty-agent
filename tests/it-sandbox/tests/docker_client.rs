//! `ra-sandbox::docker`: the client — what a container is created with, and when one is reused.
//!
//! Ports the client half of the reference's `tests/sandbox/test_docker.py` and the whole of
//! `test_docker_network_mode.py`. The daemon is the fake in `support/docker_fake.rs`; where the
//! reference replaces `_create_container` or `uuid.uuid4` to steer a test, these steer the fake's
//! container creation instead and read the session identity back from the volume names it was
//! asked for.

#[path = "support/docker_fake.rs"]
mod docker_fake;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use docker_fake::{FakeDocker, IMAGE, VolumeBehavior, docker_state};
use ra_core::sandbox::{
    AsUser, AzureBlobMount, BoxMount, CreateRequest, Entry, ErrorCode, ExecRequest, ExecResult,
    FileEntry, GcsMount, Manifest, ManifestRegistries, Mount, MountConfigError, MountPattern,
    MountProvider, MountStrategy, PosixPath, REDACTED_MOUNT_AUTHORITY_KEY, RcloneOptions,
    S3FilesMount, S3FilesOptions, S3Mount, SandboxClient, SandboxError, SandboxPathGrant,
    SandboxResult, SandboxSession, SandboxSessionState, SessionPath, SessionResources,
    ShellInvocation, SnapshotSpec, TypeRegistry, builtin_snapshot_registry, client_options_kind,
};
use ra_sandbox::docker::{
    ContainerCreateSpec, DOCKER_BACKEND_ID, DockerApiError, DockerDriverConfig, DockerMount,
    DockerMountKind, DockerNetworkMode, DockerSandboxClient, DockerSandboxClientOptions,
    DockerStateFields, PublishedPort, docker_volume_name, docker_volume_names_for_manifest,
};
use serde_json::{Value, json};
use uuid::Uuid;

const SESSION_ID: &str = "12345678-1234-5678-1234-567812345678";
const DATA_VOLUME: &str = "sandbox_12345678123456781234567812345678_ac6cdb3eb035_workspace_data";

/// Takes the failure out of a result whose success type need not be printable.
trait ExpectFailure {
    fn failure(self, what: &str) -> SandboxError;
}

impl<T> ExpectFailure for SandboxResult<T> {
    fn failure(self, what: &str) -> SandboxError {
        match self {
            Ok(_) => panic!("expected a failure: {what}"),
            Err(error) => error,
        }
    }
}

fn session_id() -> Uuid {
    Uuid::parse_str(SESSION_ID).expect("uuid")
}

fn client(fake: &Arc<FakeDocker>) -> DockerSandboxClient {
    DockerSandboxClient::new(fake.clone())
}

fn no_labels() -> BTreeMap<String, String> {
    BTreeMap::new()
}

fn options() -> DockerSandboxClientOptions {
    DockerSandboxClientOptions::new(IMAGE)
}

fn s3(bucket: &str, key: Option<(&str, &str)>, strategy: MountStrategy) -> Mount {
    Mount::new(
        MountProvider::S3(S3Mount {
            bucket: bucket.to_owned(),
            access_key_id: key.map(|(id, _)| id.to_owned()),
            secret_access_key: key.map(|(_, secret)| secret.to_owned()),
            ..S3Mount::default()
        }),
        strategy,
    )
    .expect("a supported mount")
}

fn rclone_volume() -> MountStrategy {
    MountStrategy::docker_volume("rclone")
}

/// The base create arguments every container gets.
fn base_spec(environment: Option<BTreeMap<String, String>>) -> ContainerCreateSpec {
    ContainerCreateSpec::idle(IMAGE).with_environment(environment)
}

fn volume(
    target: &str,
    source: &str,
    read_only: bool,
    driver: &str,
    options: &[(&str, &str)],
) -> DockerMount {
    DockerMount::volume(
        target,
        source,
        read_only,
        Some(DockerDriverConfig::new(
            driver,
            options
                .iter()
                .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                .collect(),
        )),
    )
}

fn creates(fake: &FakeDocker) -> Vec<ContainerCreateSpec> {
    fake.creates.lock().expect("creates").clone()
}

fn round_trip(client: &DockerSandboxClient, state: &SandboxSessionState) -> SandboxSessionState {
    let payload = client.serialize_session_state(state).expect("serialize");
    client
        .deserialize_session_state(
            payload,
            &builtin_snapshot_registry(),
            &ManifestRegistries::builtin(),
        )
        .expect("deserialize")
}

fn deserialize(client: &DockerSandboxClient, payload: Value) -> SandboxResult<SandboxSessionState> {
    client.deserialize_session_state(
        payload,
        &builtin_snapshot_registry(),
        &ManifestRegistries::builtin(),
    )
}

fn fields(state: &SandboxSessionState) -> DockerStateFields {
    DockerStateFields::read(state).expect("docker fields")
}

fn running_container() -> Value {
    json!({"State": {"Status": "running"}, "Mounts": [], "Config": {"Labels": {}}})
}

/// The volume names a creation was asked to attach, in order.
fn requested_volumes(spec: &ContainerCreateSpec) -> Vec<String> {
    spec.mounts()
        .into_iter()
        .flatten()
        .filter(|mount| mount.kind() == DockerMountKind::Volume)
        .map(|mount| mount.source().to_owned())
        .collect()
}

// --- options -------------------------------------------------------------------------------

#[test]
fn options_accept_network_mode_none() {
    let options = options()
        .with_network_mode(Some(DockerNetworkMode::None))
        .expect("allowed");

    assert_eq!(options.network_mode(), Some(DockerNetworkMode::None));
}

#[test]
fn options_refuse_other_network_modes() {
    let payload = options().to_payload().with_field("network_mode", "bridge");

    let error = DockerSandboxClientOptions::from_payload(&payload).failure("refused");

    assert_eq!(error.error_code(), ErrorCode::SandboxConfigInvalid);
}

#[test]
fn options_refuse_exposed_ports_without_a_network() {
    let ports_first = options()
        .with_exposed_ports([8080])
        .expect("ports")
        .with_network_mode(Some(DockerNetworkMode::None))
        .failure("refused");
    let mode_first = options()
        .with_network_mode(Some(DockerNetworkMode::None))
        .expect("mode")
        .with_exposed_ports([8080])
        .failure("refused");

    for error in [ports_first, mode_first] {
        assert!(error.message().contains("exposed_ports"), "{error}");
    }
}

#[test]
fn options_round_trip_network_mode_through_the_registry() {
    let options = options()
        .with_network_mode(Some(DockerNetworkMode::None))
        .expect("mode");
    let mut registry = TypeRegistry::new(client_options_kind());
    DockerSandboxClientOptions::register(&mut registry).expect("register");

    let parsed = registry
        .parse(&options.to_payload().to_json())
        .expect("parse");
    let restored = DockerSandboxClientOptions::from_payload(&parsed).expect("read");

    assert_eq!(restored, options);
    assert_eq!(restored.network_mode(), Some(DockerNetworkMode::None));
}

#[test]
fn options_without_network_mode_keep_the_default() {
    let mut registry = TypeRegistry::new(client_options_kind());
    DockerSandboxClientOptions::register(&mut registry).expect("register");

    let parsed = registry
        .parse(&json!({"type": "docker", "image": IMAGE}))
        .expect("parse");
    let restored = DockerSandboxClientOptions::from_payload(&parsed).expect("read");

    assert_eq!(restored.network_mode(), None);
    assert!(restored.labels().is_empty());
}

// --- what a container is created with ------------------------------------------------------

#[tokio::test]
async fn create_container_pulls_a_registry_port_image_by_repository_and_tag() {
    let fake = Arc::new(FakeDocker::new());

    let error = client(&fake)
        .create_container(
            "localhost:5000/myimg:latest",
            None,
            &[],
            None,
            None,
            &no_labels(),
        )
        .await
        .failure("still missing after the pull");

    assert_eq!(error.error_code(), ErrorCode::SandboxConfigInvalid);
    assert_eq!(
        *fake.pulls.lock().expect("pulls"),
        vec![("localhost:5000/myimg".to_owned(), Some("latest".to_owned()))]
    );
}

#[tokio::test]
async fn create_container_publishes_exposed_ports_on_loopback() {
    let fake = Arc::new(FakeDocker::new().with_image(IMAGE));

    client(&fake)
        .create_container(IMAGE, None, &[8765, 9000], None, None, &no_labels())
        .await
        .expect("created");

    let expected = base_spec(None).with_ports(
        [8765, 9000]
            .into_iter()
            .map(|port| PublishedPort::new(format!("{port}/tcp"), "127.0.0.1", None))
            .collect(),
    );
    assert_eq!(creates(&fake), vec![expected]);
}

#[tokio::test]
async fn create_container_applies_labels_and_omits_empty_ones() {
    let fake = Arc::new(FakeDocker::new().with_image(IMAGE));
    let labels = BTreeMap::from([("com.example.owner".to_owned(), "worker-123".to_owned())]);

    client(&fake)
        .create_container(IMAGE, None, &[], None, None, &labels)
        .await
        .expect("created");
    client(&fake)
        .create_container(IMAGE, None, &[], None, None, &no_labels())
        .await
        .expect("created");

    let specs = creates(&fake);
    assert_eq!(specs[0].labels(), Some(&labels));
    assert_eq!(specs[1].labels(), None);
}

#[tokio::test]
async fn create_container_passes_network_mode_none_and_omits_it_by_default() {
    let fake = Arc::new(FakeDocker::new().with_image(IMAGE));

    client(&fake)
        .create_container(
            IMAGE,
            None,
            &[],
            Some(DockerNetworkMode::None),
            None,
            &no_labels(),
        )
        .await
        .expect("created");
    client(&fake)
        .create_container(IMAGE, None, &[], None, None, &no_labels())
        .await
        .expect("created");

    let expected = base_spec(None).with_network_mode("none");
    let specs = creates(&fake);
    assert_eq!(specs[0], expected);
    assert_eq!(specs[1].network_mode(), None);
}

#[tokio::test]
async fn create_container_binds_an_explicit_host_path_and_leaves_path_only_grants_alone() {
    let tmp = tempfile::tempdir().expect("tmp");
    let host_path = std::fs::canonicalize(tmp.path())
        .expect("canonical")
        .join("shared-data");
    std::fs::create_dir(&host_path).expect("mkdir");
    let fake = Arc::new(FakeDocker::new().with_image(IMAGE));
    let split = Manifest::new().with_path_grant(
        SandboxPathGrant::new("/mnt/shared-data")
            .expect("grant")
            .with_host_path(&host_path.to_string_lossy())
            .expect("host path")
            .read_only(true),
    );
    let path_only = Manifest::new().with_path_grant(
        SandboxPathGrant::new("/tmp")
            .expect("grant")
            .read_only(true),
    );

    let client = client(&fake);
    client
        .create_container(IMAGE, Some(&split), &[], None, None, &no_labels())
        .await
        .expect("created");
    client
        .create_container(IMAGE, Some(&path_only), &[], None, None, &no_labels())
        .await
        .expect("created");

    let specs = creates(&fake);
    assert_eq!(
        specs[0].mounts(),
        Some(
            &[DockerMount::bind(
                "/mnt/shared-data",
                host_path.to_string_lossy(),
                true
            )][..]
        )
    );
    assert_eq!(specs[1].mounts(), None);
}

#[tokio::test]
async fn a_target_shared_by_split_and_path_only_grants_is_refused_before_the_image_lookup() {
    let tmp = tempfile::tempdir().expect("tmp");
    for explicit_first in [false, true] {
        let path_only = SandboxPathGrant::new("/mnt/shared-data").expect("grant");
        let explicit = SandboxPathGrant::new("/mnt/shared-data")
            .expect("grant")
            .with_host_path(&tmp.path().to_string_lossy())
            .expect("host path");
        let (first, second) = if explicit_first {
            (explicit, path_only)
        } else {
            (path_only, explicit)
        };
        let manifest = Manifest::new()
            .with_path_grant(first)
            .with_path_grant(second);
        let fake = Arc::new(FakeDocker::new().with_image(IMAGE));

        let error = client(&fake)
            .create_container(IMAGE, Some(&manifest), &[], None, None, &no_labels())
            .await
            .failure("refused");

        assert!(
            error
                .message()
                .contains("duplicate Docker sandbox path grant target"),
            "{error}"
        );
        assert!(fake.image_lookups.lock().expect("lookups").is_empty());
    }
}

#[tokio::test]
async fn a_host_path_target_overlapping_the_workspace_is_refused_before_the_image_lookup() {
    let tmp = tempfile::tempdir().expect("tmp");
    for (root, target) in [
        ("/workspace", "/workspace/shared-data"),
        ("/workspace/project", "/workspace"),
    ] {
        let manifest = Manifest::new().with_root(root).with_path_grant(
            SandboxPathGrant::new(target)
                .expect("grant")
                .with_host_path(&tmp.path().to_string_lossy())
                .expect("host path"),
        );
        let fake = Arc::new(FakeDocker::new().with_image(IMAGE));

        let error = client(&fake)
            .create_container(IMAGE, Some(&manifest), &[], None, None, &no_labels())
            .await
            .failure("refused");

        assert!(
            error
                .message()
                .contains("host_path target must be outside the workspace root"),
            "{error}"
        );
        assert!(fake.image_lookups.lock().expect("lookups").is_empty());
    }
}

/// Creates a container for a one-mount manifest and returns the mounts it was asked for.
async fn created_mounts(
    mount: Mount,
    session: Option<Uuid>,
) -> (Option<Vec<DockerMount>>, ContainerCreateSpec) {
    let fake = Arc::new(FakeDocker::new().with_image(IMAGE));
    let manifest = Manifest::new().with_entry("data", Entry::mount(mount));
    client(&fake)
        .create_container(IMAGE, Some(&manifest), &[], None, session, &no_labels())
        .await
        .expect("created");
    let spec = creates(&fake).remove(0);
    (spec.mounts().map(<[DockerMount]>::to_vec), spec)
}

#[tokio::test]
async fn an_s3_volume_takes_its_driver_from_the_strategy_not_a_mount_pattern() {
    let mount = Mount::new(
        MountProvider::S3(S3Mount {
            bucket: "bucket".to_owned(),
            access_key_id: Some("key-id".to_owned()),
            secret_access_key: Some("secret".to_owned()),
            prefix: Some("logs/".to_owned()),
            region: Some("us-west-2".to_owned()),
            endpoint_url: Some("https://s3.example.test".to_owned()),
            ..S3Mount::default()
        }),
        MountStrategy::DockerVolume {
            driver: "mountpoint".to_owned(),
            driver_options: BTreeMap::from([("allow_other".to_owned(), "true".to_owned())]),
        },
    )
    .expect("a supported mount")
    .writable(true);

    let (mounts, spec) = created_mounts(mount, Some(session_id())).await;

    assert_eq!(spec.environment(), Some(&BTreeMap::new()));
    assert_eq!(
        mounts,
        Some(vec![volume(
            "/workspace/data",
            DATA_VOLUME,
            false,
            "mountpoint",
            &[
                ("bucket", "bucket"),
                ("access_key_id", "key-id"),
                ("secret_access_key", "secret"),
                ("endpoint_url", "https://s3.example.test"),
                ("region", "us-west-2"),
                ("prefix", "logs/"),
                ("allow_other", "true"),
            ],
        )])
    );
}

#[tokio::test]
async fn an_s3_volume_through_rclone() {
    let mount = s3("bucket", Some(("key-id", "secret")), rclone_volume());

    let (mounts, _) = created_mounts(mount, Some(session_id())).await;

    assert_eq!(
        mounts,
        Some(vec![volume(
            "/workspace/data",
            DATA_VOLUME,
            true,
            "rclone",
            &[
                ("type", "s3"),
                ("s3-provider", "AWS"),
                ("path", "bucket"),
                ("s3-access-key-id", "key-id"),
                ("s3-secret-access-key", "secret"),
            ],
        )])
    );
}

#[tokio::test]
async fn a_gcs_volume_through_rclone_with_a_service_account() {
    let mount = Mount::new(
        MountProvider::Gcs(GcsMount {
            bucket: "bucket".to_owned(),
            service_account_file: Some("/data/config/gcs.json".to_owned()),
            ..GcsMount::default()
        }),
        rclone_volume(),
    )
    .expect("a supported mount");

    let (mounts, _) = created_mounts(mount, None).await;

    assert_eq!(
        mounts,
        Some(vec![volume(
            "/workspace/data",
            "sandbox_ac6cdb3eb035_workspace_data",
            true,
            "rclone",
            &[
                ("type", "google cloud storage"),
                ("path", "bucket"),
                ("gcs-service-account-file", "/data/config/gcs.json"),
            ],
        )])
    );
}

#[tokio::test]
async fn a_gcs_volume_with_hmac_keys_through_rclone_s3_compatibility() {
    let mount = Mount::new(
        MountProvider::Gcs(GcsMount {
            bucket: "bucket".to_owned(),
            access_id: Some("access-id".to_owned()),
            secret_access_key: Some("secret-key".to_owned()),
            prefix: Some("prefix/".to_owned()),
            region: Some("auto".to_owned()),
            ..GcsMount::default()
        }),
        rclone_volume(),
    )
    .expect("a supported mount")
    .writable(true);

    let (mounts, _) = created_mounts(mount, None).await;

    assert_eq!(
        mounts,
        Some(vec![volume(
            "/workspace/data",
            "sandbox_ac6cdb3eb035_workspace_data",
            false,
            "rclone",
            &[
                ("type", "s3"),
                ("path", "bucket/prefix/"),
                ("s3-provider", "GCS"),
                ("s3-access-key-id", "access-id"),
                ("s3-secret-access-key", "secret-key"),
                ("s3-endpoint", "https://storage.googleapis.com"),
                ("s3-region", "auto"),
            ],
        )])
    );
}

#[tokio::test]
async fn an_azure_volume_through_rclone() {
    let mount = Mount::new(
        MountProvider::AzureBlob(AzureBlobMount {
            account: "acct".to_owned(),
            container: "container".to_owned(),
            endpoint: Some("https://blob.example.test".to_owned()),
            identity_client_id: Some("client-id".to_owned()),
            account_key: Some("account-key".to_owned()),
        }),
        rclone_volume(),
    )
    .expect("a supported mount");

    let (mounts, _) = created_mounts(mount, None).await;

    assert_eq!(
        mounts,
        Some(vec![volume(
            "/workspace/data",
            "sandbox_ac6cdb3eb035_workspace_data",
            true,
            "rclone",
            &[
                ("type", "azureblob"),
                ("path", "container"),
                ("azureblob-account", "acct"),
                ("azureblob-endpoint", "https://blob.example.test"),
                ("azureblob-msi-client-id", "client-id"),
                ("azureblob-key", "account-key"),
            ],
        )])
    );
}

#[tokio::test]
async fn a_box_volume_through_rclone() {
    let mount = Mount::new(
        MountProvider::Box(BoxMount {
            path: Some("/Shared/Finance".to_owned()),
            client_id: Some("client-id".to_owned()),
            client_secret: Some("client-secret".to_owned()),
            access_token: Some("access-token".to_owned()),
            root_folder_id: Some("12345".to_owned()),
            impersonate: Some("user-42".to_owned()),
            ..BoxMount::default()
        }),
        rclone_volume(),
    )
    .expect("a supported mount")
    .writable(true);

    let (mounts, _) = created_mounts(mount, None).await;

    assert_eq!(
        mounts,
        Some(vec![volume(
            "/workspace/data",
            "sandbox_ac6cdb3eb035_workspace_data",
            false,
            "rclone",
            &[
                ("type", "box"),
                ("path", "Shared/Finance"),
                ("box-client-id", "client-id"),
                ("box-client-secret", "client-secret"),
                ("box-access-token", "access-token"),
                ("box-root-folder-id", "12345"),
                ("box-impersonate", "user-42"),
            ],
        )])
    );
}

#[tokio::test]
async fn a_mount_type_without_a_volume_driver_is_refused_before_anything_is_created() {
    let fake = Arc::new(FakeDocker::new().with_image(IMAGE));
    let custom = Mount::new(
        MountProvider::Extension(ra_core::sandbox::DiscriminatedPayload::new(
            "recording_mount",
        )),
        rclone_volume(),
    )
    .expect("an extension mount");
    let manifest = Manifest::new().with_entry("custom", Entry::mount(custom));

    let error = client(&fake)
        .create_container(IMAGE, Some(&manifest), &[], None, None, &no_labels())
        .await
        .failure("refused");

    assert_eq!(error.error_code(), ErrorCode::MountConfigInvalid);
    assert!(
        error
            .message()
            .contains("docker-volume mounts are not supported for this mount type"),
        "{error}"
    );
    assert!(creates(&fake).is_empty());
}

#[test]
fn an_s3_files_mount_refuses_a_volume_driver() {
    let error = Mount::new(
        MountProvider::S3Files(S3FilesMount {
            file_system_id: "fs-1234567890abcdef0".to_owned(),
            ..S3FilesMount::default()
        }),
        rclone_volume(),
    )
    .expect_err("refused");

    assert!(matches!(error, MountConfigError::UnsupportedDriver { .. }));
    assert_eq!(error.to_string(), "invalid Docker volume driver");
}

#[tokio::test]
async fn an_in_container_rclone_mount_gets_fuse() {
    let mount = s3(
        "bucket",
        None,
        MountStrategy::InContainer {
            pattern: MountPattern::Rclone(RcloneOptions::default()),
        },
    );

    let (_, spec) = created_mounts(mount, None).await;

    let expected = base_spec(Some(BTreeMap::new()))
        .with_devices(vec!["/dev/fuse".to_owned()])
        .with_cap_add(vec!["SYS_ADMIN".to_owned()])
        .with_security_opt(vec!["apparmor:unconfined".to_owned()]);
    assert_eq!(spec, expected);
}

#[tokio::test]
async fn an_s3_files_mount_gets_sys_admin_without_fuse() {
    let mount = Mount::new(
        MountProvider::S3Files(S3FilesMount {
            file_system_id: "fs-1234567890abcdef0".to_owned(),
            ..S3FilesMount::default()
        }),
        MountStrategy::InContainer {
            pattern: MountPattern::S3Files(S3FilesOptions::default()),
        },
    )
    .expect("a supported mount");

    let (_, spec) = created_mounts(mount, None).await;

    let expected = base_spec(Some(BTreeMap::new()))
        .with_cap_add(vec!["SYS_ADMIN".to_owned()])
        .with_security_opt(vec!["apparmor:unconfined".to_owned()]);
    assert_eq!(spec, expected);
}

#[test]
fn volume_names_do_not_collide_across_separator_aliases() {
    assert_eq!(
        docker_volume_name(Some(session_id()), &PosixPath::new("/workspace/a_b")),
        "sandbox_12345678123456781234567812345678_e00b2d707edb_workspace_a_b"
    );
    assert_eq!(
        docker_volume_name(Some(session_id()), &PosixPath::new("/workspace/a/b")),
        "sandbox_12345678123456781234567812345678_212366248685_workspace_a_b"
    );
}

#[test]
fn volume_names_use_only_strictly_safe_suffix_characters() {
    assert_eq!(
        docker_volume_name(None, &PosixPath::new("/workspace/data set/@prod")),
        "sandbox_fe44fda0e4f6_workspace_data_set__prod"
    );
}

// --- state ---------------------------------------------------------------------------------

#[test]
fn state_round_trip_keeps_labels_and_reads_old_payloads_without_them() {
    let fake = Arc::new(FakeDocker::new());
    let client = client(&fake);
    let labels = BTreeMap::from([("com.example.owner".to_owned(), "worker-123".to_owned())]);
    let state = DockerStateFields::new(IMAGE, "container")
        .with_labels(labels.clone())
        .apply(docker_state(Manifest::new(), "container"));

    assert_eq!(fields(&round_trip(&client, &state)).labels(), &labels);

    let mut payload = client.serialize_session_state(&state).expect("serialize");
    payload.as_object_mut().expect("object").remove("labels");
    let restored = deserialize(&client, payload).expect("an old payload still reads");
    assert!(fields(&restored).labels().is_empty());
}

#[test]
fn state_round_trip_keeps_network_mode_and_reads_old_payloads_without_it() {
    let fake = Arc::new(FakeDocker::new());
    let client = client(&fake);
    let state = DockerStateFields::new(IMAGE, "missing-container")
        .with_network_mode(Some(DockerNetworkMode::None))
        .apply(docker_state(Manifest::new(), "missing-container"));

    assert_eq!(
        fields(&round_trip(&client, &state)).network_mode(),
        Some(DockerNetworkMode::None)
    );

    let mut payload = client.serialize_session_state(&state).expect("serialize");
    payload
        .as_object_mut()
        .expect("object")
        .remove("network_mode");
    assert_eq!(
        fields(&deserialize(&client, payload).expect("reads")).network_mode(),
        None
    );
}

#[test]
fn a_state_with_an_unknown_network_mode_or_ports_without_a_network_is_refused_unread() {
    let fake = Arc::new(FakeDocker::new());
    let client = client(&fake);
    let state = docker_state(Manifest::new(), "missing-container");

    let mut bridge = client.serialize_session_state(&state).expect("serialize");
    bridge["network_mode"] = json!("bridge");
    let mut ported = client.serialize_session_state(&state).expect("serialize");
    ported["network_mode"] = json!("none");
    ported["exposed_ports"] = json!([8080]);

    for payload in [bridge, ported] {
        let error = deserialize(&client, payload).failure("refused");
        assert_eq!(error.error_code(), ErrorCode::SandboxConfigInvalid);
        assert_eq!(error.message(), "sandbox session state payload is invalid");
    }
    assert!(fake.inspects.lock().expect("inspects").is_empty());
    assert!(fake.image_lookups.lock().expect("lookups").is_empty());
}

// --- create --------------------------------------------------------------------------------

#[tokio::test]
async fn create_persists_configured_labels_and_network_mode() {
    let fake = Arc::new(FakeDocker::new().with_image(IMAGE));
    let labels = BTreeMap::from([("com.example.owner".to_owned(), "worker-123".to_owned())]);
    let options = options()
        .with_labels(labels.clone())
        .with_network_mode(Some(DockerNetworkMode::None))
        .expect("mode");

    let session = client(&fake)
        .create(CreateRequest::new().with_options(options.to_payload()))
        .await
        .expect("created");

    let state = fields(&session.state());
    assert_eq!(state.labels(), &labels);
    assert_eq!(state.network_mode(), Some(DockerNetworkMode::None));
    let spec = creates(&fake).remove(0);
    assert_eq!(spec.labels(), Some(&labels));
    assert_eq!(spec.network_mode(), Some("none"));
    assert_eq!(
        *fake.starts.lock().expect("starts"),
        vec![state.container_id().to_owned()]
    );
}

#[tokio::test]
async fn create_without_options_is_refused() {
    let fake = Arc::new(FakeDocker::new().with_image(IMAGE));

    let error = client(&fake)
        .create(CreateRequest::new())
        .await
        .failure("needs an image");

    assert_eq!(error.error_code(), ErrorCode::SandboxConfigInvalid);
}

fn credentialed_manifest(secret: &str) -> Manifest {
    Manifest::new().with_entry(
        "data",
        Entry::mount(s3("bucket", Some(("access-key", secret)), rclone_volume())),
    )
}

#[tokio::test]
async fn create_removes_the_container_and_volumes_when_the_start_fails() {
    let fake = Arc::new(FakeDocker::new().with_image(IMAGE));
    fake.on_create(|fake, spec| {
        for volume in requested_volumes(spec) {
            fake.add_volume(&volume);
        }
        Ok("failed-start-container".to_owned())
    });
    *fake.start_error.lock().expect("start") =
        Some(DockerApiError::api(500, "container startup failed"));

    let error = client(&fake)
        .create(
            CreateRequest::new()
                .with_manifest(credentialed_manifest("secret-key"))
                .with_options(options().to_payload()),
        )
        .await
        .failure("start failed");

    assert!(
        error.message().contains("protected mount configuration"),
        "{error}"
    );
    assert_eq!(
        *fake.container_removals.lock().expect("removals"),
        vec![("failed-start-container".to_owned(), true)]
    );
    let requested = requested_volumes(&creates(&fake)[0]);
    assert_eq!(*fake.volume_removals.lock().expect("removals"), requested);
    assert!(fake.volume_names().is_empty());
}

#[tokio::test]
async fn create_removes_volumes_when_acquiring_the_container_fails() {
    let fake = Arc::new(FakeDocker::new().with_image(IMAGE));
    fake.on_create(|fake, spec| {
        for volume in requested_volumes(spec) {
            fake.add_volume(&volume);
        }
        Err(DockerApiError::api(
            500,
            "container acquisition failed with secret-key",
        ))
    });

    let error = client(&fake)
        .create(
            CreateRequest::new()
                .with_manifest(credentialed_manifest("secret-key"))
                .with_options(options().to_payload()),
        )
        .await
        .failure("acquisition failed");

    assert!(
        error.message().contains("protected mount configuration"),
        "{error}"
    );
    assert!(!format!("{error:?}").contains("secret-key"));
    assert!(fake.container_removals.lock().expect("removals").is_empty());
    assert_eq!(fake.volume_removals.lock().expect("removals").len(), 1);
    assert!(fake.volume_names().is_empty());
}

#[tokio::test]
async fn create_removes_what_it_acquired_when_the_snapshot_cannot_be_named() {
    use std::os::unix::ffi::OsStrExt;
    let fake = Arc::new(FakeDocker::new().with_image(IMAGE));
    fake.on_create(|fake, spec| {
        for volume in requested_volumes(spec) {
            fake.add_volume(&volume);
        }
        Ok("started-container".to_owned())
    });
    let unnameable = SnapshotSpec::Local {
        base_path: std::ffi::OsStr::from_bytes(b"/tmp/\xff").into(),
    };

    let error = client(&fake)
        .create(
            CreateRequest::new()
                .with_manifest(credentialed_manifest("secret-key"))
                .with_snapshot_spec(unnameable)
                .with_options(options().to_payload()),
        )
        .await
        .failure("the snapshot cannot be named");

    assert!(
        error.message().contains("protected mount configuration"),
        "{error}"
    );
    assert_eq!(
        *fake.starts.lock().expect("starts"),
        vec!["started-container".to_owned()]
    );
    assert_eq!(
        *fake.container_removals.lock().expect("removals"),
        vec![("started-container".to_owned(), true)]
    );
    assert!(fake.volume_names().is_empty());
}

// --- delete --------------------------------------------------------------------------------

fn volume_manifest() -> Manifest {
    Manifest::new()
        .with_entry("data", Entry::mount(s3("bucket", None, rclone_volume())))
        .with_entry(
            "in-container",
            Entry::mount(s3(
                "bucket",
                None,
                MountStrategy::InContainer {
                    pattern: MountPattern::Rclone(RcloneOptions::default()),
                },
            )),
        )
}

#[tokio::test]
async fn delete_removes_the_container_and_its_generated_volumes() {
    let fake = Arc::new(
        FakeDocker::new()
            .with_container("container", json!({"State": {"Status": "exited"}}))
            .with_volume(DATA_VOLUME, VolumeBehavior::Removes),
    );
    let client = client(&fake);
    let state = docker_state(volume_manifest(), "container").with_session_id(session_id());
    let session =
        ra_sandbox::docker::DockerSandboxSession::new(fake.clone(), state).expect("state");

    client.delete(&session).await.expect("deleted");

    assert_eq!(
        *fake.container_removals.lock().expect("removals"),
        vec![("container".to_owned(), false)]
    );
    assert_eq!(
        *fake.volume_lookups.lock().expect("lookups"),
        vec![DATA_VOLUME.to_owned()]
    );
    assert_eq!(
        *fake.volume_removals.lock().expect("removals"),
        vec![DATA_VOLUME.to_owned()]
    );
}

/// A Docker session whose shutdown fails, as the reference's tests make one by replacing `shutdown`.
struct FailingShutdown {
    state: SandboxSessionState,
    resources: SessionResources,
    message: String,
}

#[async_trait]
impl SandboxSession for FailingShutdown {
    fn backend_id(&self) -> &str {
        DOCKER_BACKEND_ID
    }
    fn state(&self) -> SandboxSessionState {
        self.state.clone()
    }
    fn resources(&self) -> &SessionResources {
        &self.resources
    }
    async fn exec(&self, _request: ExecRequest) -> SandboxResult<ExecResult> {
        unimplemented!("not reached")
    }
    async fn running(&self) -> SandboxResult<bool> {
        Ok(false)
    }
    async fn ls(&self, _path: SessionPath<'_>, _user: AsUser) -> SandboxResult<Vec<FileEntry>> {
        unimplemented!("not reached")
    }
    async fn rm(
        &self,
        _path: SessionPath<'_>,
        _recursive: bool,
        _user: AsUser,
    ) -> SandboxResult<()> {
        unimplemented!("not reached")
    }
    async fn mkdir(
        &self,
        _path: SessionPath<'_>,
        _parents: bool,
        _user: AsUser,
    ) -> SandboxResult<()> {
        unimplemented!("not reached")
    }
    async fn read(&self, _path: SessionPath<'_>, _user: AsUser) -> SandboxResult<Vec<u8>> {
        unimplemented!("not reached")
    }
    async fn write(
        &self,
        _path: SessionPath<'_>,
        _data: Vec<u8>,
        _user: AsUser,
    ) -> SandboxResult<()> {
        unimplemented!("not reached")
    }
    async fn persist_workspace(&self) -> SandboxResult<Vec<u8>> {
        unimplemented!("not reached")
    }
    async fn hydrate_workspace(&self, _data: Vec<u8>) -> SandboxResult<()> {
        unimplemented!("not reached")
    }
    async fn shutdown(&self) -> SandboxResult<()> {
        Err(SandboxError::new(
            ErrorCode::ExecTransportError,
            ra_core::sandbox::OpName::Shutdown,
            self.message.clone(),
        ))
    }
}

#[tokio::test]
async fn delete_redacts_the_first_failure_and_still_settles_every_volume() {
    let sentinel = "delete-boundary-secret";
    let manifest = Manifest::new()
        .with_entry(
            "left",
            Entry::mount(s3(
                "left-bucket",
                Some(("access-key", sentinel)),
                rclone_volume(),
            )),
        )
        .with_entry(
            "middle",
            Entry::mount(s3("middle-bucket", None, rclone_volume())),
        )
        .with_entry(
            "right",
            Entry::mount(s3("right-bucket", None, rclone_volume())),
        );
    let names = docker_volume_names_for_manifest(&manifest, Some(session_id())).expect("names");
    let fake = Arc::new(
        FakeDocker::new()
            .with_container("container", json!({"State": {"Status": "exited"}}))
            .with_volume(
                &names[0],
                VolumeBehavior::Fails(DockerApiError::api(500, "secondary volume removal failed")),
            )
            .with_volume(&names[1], VolumeBehavior::Removes)
            .with_volume(&names[2], VolumeBehavior::Removes),
    );
    *fake.container_remove_error.lock().expect("error") = Some(DockerApiError::api(
        500,
        "secondary container removal failed",
    ));
    let session = FailingShutdown {
        state: docker_state(manifest, "container").with_session_id(session_id()),
        resources: SessionResources::new(),
        message: format!("shutdown echoed {sentinel}"),
    };

    let error = client(&fake)
        .delete(&session)
        .await
        .failure("shutdown failed");

    assert!(
        error.message().contains("protected mount configuration"),
        "{error}"
    );
    assert!(!format!("{error:?}").contains(sentinel));
    assert!(std::error::Error::source(&error).is_none());
    assert_eq!(
        *fake.container_removals.lock().expect("removals"),
        vec![("container".to_owned(), false)]
    );
    assert_eq!(*fake.volume_removals.lock().expect("removals"), names);
}

#[tokio::test]
async fn delete_still_shuts_down_and_removes_volumes_after_the_container_lookup_fails() {
    for (lookup_error, succeeds) in [
        (DockerApiError::not_found("container not found"), true),
        (DockerApiError::transport("container lookup failed"), false),
    ] {
        let manifest =
            Manifest::new().with_entry("data", Entry::mount(s3("bucket", None, rclone_volume())));
        let names = docker_volume_names_for_manifest(&manifest, Some(session_id())).expect("names");
        let fake = Arc::new(FakeDocker::new().with_volume(&names[0], VolumeBehavior::Removes));
        *fake.inspect_error.lock().expect("error") = Some(lookup_error);
        let state = docker_state(manifest, "container").with_session_id(session_id());
        let session =
            ra_sandbox::docker::DockerSandboxSession::new(fake.clone(), state).expect("state");

        let outcome = client(&fake).delete(&session).await;

        assert_eq!(outcome.is_ok(), succeeds, "{outcome:?}");
        assert!(
            fake.inspects
                .lock()
                .expect("inspects")
                .contains(&"container".to_owned())
        );
        assert_eq!(*fake.volume_lookups.lock().expect("lookups"), names);
        assert_eq!(*fake.volume_removals.lock().expect("removals"), names);
    }
}

// --- resume --------------------------------------------------------------------------------

/// A state as a host would hand it back: written, read, and rebound from `trusted`.
fn persisted_and_rebound(
    client: &DockerSandboxClient,
    persisted: &SandboxSessionState,
    trusted: &Manifest,
) -> SandboxSessionState {
    round_trip(client, persisted)
        .rebind_persisted_mount_authority(Some(trusted), DOCKER_BACKEND_ID)
        .expect("rebound")
}

#[tokio::test]
async fn resume_gives_a_replacement_fresh_volumes_and_removes_them_when_it_fails() {
    let manifest = credentialed_manifest("secret-key");
    let fake = Arc::new(
        FakeDocker::new()
            .with_image(IMAGE)
            .with_volume(DATA_VOLUME, VolumeBehavior::Removes),
    );
    let client = client(&fake);
    let persisted = docker_state(manifest.clone(), "missing-container")
        .with_session_id(session_id())
        .with_workspace_root_ready(true);
    // Written with the persisted identity put back, as the reference's test does: the replacement
    // must get fresh volumes even when the state still names the old session.
    let mut payload = client
        .serialize_session_state(&persisted)
        .expect("serialize");
    payload["session_id"] = json!(SESSION_ID);
    let state = deserialize(&client, payload)
        .expect("reads")
        .rebind_persisted_mount_authority(Some(&manifest), DOCKER_BACKEND_ID)
        .expect("rebound");
    assert_eq!(state.session_id(), session_id());
    fake.on_create(|fake, spec| {
        for volume in requested_volumes(spec) {
            fake.add_volume(&volume);
        }
        Err(DockerApiError::api(
            500,
            "replacement acquisition failed with secret-key",
        ))
    });

    let error = client.resume(state).await.failure("replacement failed");

    assert!(
        error.message().contains("protected mount configuration"),
        "{error}"
    );
    let replacement = requested_volumes(&creates(&fake)[0]);
    assert_eq!(replacement.len(), 1);
    assert_ne!(replacement[0], DATA_VOLUME);
    assert_eq!(*fake.volume_removals.lock().expect("removals"), replacement);
    assert_eq!(fake.volume_names(), [DATA_VOLUME.to_owned()].into());
}

#[tokio::test]
async fn resume_attaches_the_current_authority_under_a_fresh_volume_identity() {
    let current = Manifest::new().with_entry(
        "data",
        Entry::mount(s3(
            "bucket",
            Some(("current-access-key", "current-secret-key")),
            rclone_volume(),
        )),
    );
    let previous = Manifest::new().with_entry(
        "data",
        Entry::mount(s3(
            "bucket",
            Some(("previous-access-key", "previous-secret-key")),
            rclone_volume(),
        )),
    );
    let fake = Arc::new(
        FakeDocker::new()
            .with_image(IMAGE)
            .with_volume(DATA_VOLUME, VolumeBehavior::Removes),
    );
    let client = client(&fake);
    let persisted = docker_state(previous, "missing-container").with_session_id(session_id());
    let state = persisted_and_rebound(&client, &persisted, &current);

    let session = client.resume(state).await.expect("resumed");

    let spec = creates(&fake).remove(0);
    let options = spec.mounts().expect("mounts")[0]
        .driver_config()
        .expect("driver")
        .options()
        .clone();
    assert_eq!(options["s3-access-key-id"], "current-access-key");
    assert_eq!(options["s3-secret-access-key"], "current-secret-key");
    assert_eq!(spec.network_mode(), None);
    assert_ne!(session.state().session_id(), session_id());
    assert!(fake.volume_lookups.lock().expect("lookups").is_empty());
    assert!(fake.volume_removals.lock().expect("removals").is_empty());
}

#[tokio::test]
async fn resume_never_removes_the_volume_the_state_itself_names() {
    let manifest = credentialed_manifest("secret-key");
    let fake = Arc::new(FakeDocker::new().with_image(IMAGE).with_volume(
        DATA_VOLUME,
        VolumeBehavior::Fails(DockerApiError::api(
            500,
            "persisted volume must not be removed",
        )),
    ));
    let state = docker_state(manifest, "missing-container")
        .with_session_id(session_id())
        .with_workspace_root_ready(true);

    let session = client(&fake).resume(state).await.expect("resumed");

    let resumed = session.state();
    assert_eq!(fields(&resumed).container_id(), "created-1");
    assert_ne!(resumed.session_id(), session_id());
    assert!(!resumed.workspace_root_ready());
    assert!(fake.volume_removals.lock().expect("removals").is_empty());
}

#[tokio::test]
async fn resume_keeps_the_volume_identity_of_a_credentialless_direct_state() {
    let manifest =
        Manifest::new().with_entry("data", Entry::mount(s3("bucket", None, rclone_volume())));
    let fake = Arc::new(
        FakeDocker::new()
            .with_image(IMAGE)
            .with_volume(DATA_VOLUME, VolumeBehavior::Removes),
    );
    let state = docker_state(manifest, "missing-container").with_session_id(session_id());

    let session = client(&fake).resume(state).await.expect("resumed");

    assert_eq!(session.state().session_id(), session_id());
    assert_eq!(
        requested_volumes(&creates(&fake)[0]),
        vec![DATA_VOLUME.to_owned()]
    );
    assert!(fake.volume_removals.lock().expect("removals").is_empty());
}

#[tokio::test]
async fn an_abandoned_resume_removes_its_partial_volume_before_the_next_try() {
    let manifest = credentialed_manifest("secret-key");
    let fake = Arc::new(
        FakeDocker::new()
            .with_image(IMAGE)
            .with_volume(DATA_VOLUME, VolumeBehavior::Removes),
    );
    let client = client(&fake);
    let state = docker_state(manifest, "missing-container")
        .with_session_id(session_id())
        .with_workspace_root_ready(true);
    fake.on_create(|fake, spec| {
        for volume in requested_volumes(spec) {
            fake.add_volume(&volume);
        }
        fake.containers
            .lock()
            .expect("containers")
            .insert("replacement".to_owned(), running_container());
        Ok("replacement".to_owned())
    });
    fake.hang_creates
        .store(true, std::sync::atomic::Ordering::SeqCst);

    let abandoned =
        tokio::time::timeout(Duration::from_millis(50), client.resume(state.clone())).await;
    assert!(abandoned.is_err(), "the first try never finishes");
    // The daemon response must be collected even though the caller stopped waiting.
    fake.create_response.notify_one();
    // Give the detached acquisition and its cleanup a turn to finish.
    for _ in 0..100 {
        if fake.volume_removals.lock().expect("removals").len() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let partial = requested_volumes(&creates(&fake)[0]);
    assert_eq!(*fake.volume_removals.lock().expect("removals"), partial);
    assert_eq!(fake.volume_names(), [DATA_VOLUME.to_owned()].into());
    assert!(fake.containers.lock().expect("containers").is_empty());
    assert_eq!(
        *fake.container_removals.lock().expect("removals"),
        [("replacement".to_owned(), true)]
    );

    fake.hang_creates
        .store(false, std::sync::atomic::Ordering::SeqCst);
    let session = client.resume(state).await.expect("the second try");

    let second = requested_volumes(&creates(&fake)[1]);
    assert_ne!(second, partial);
    assert!(!session.state().workspace_root_ready());
    assert_eq!(
        fake.volume_names(),
        [DATA_VOLUME.to_owned(), second[0].clone()].into()
    );
}

#[tokio::test]
async fn resume_reuses_a_running_container_with_the_readiness_the_state_recorded() {
    for ready in [true, false] {
        let fake = Arc::new(FakeDocker::new().with_container("container", running_container()));
        let state = docker_state(Manifest::new().with_root("/workspace"), "container")
            .with_workspace_root_ready(ready);

        let session = client(&fake).resume(state).await.expect("resumed");

        assert_eq!(session.state().workspace_root_ready(), ready);
        assert!(!session.should_provision_accounts());
        assert!(creates(&fake).is_empty());
    }
}

#[tokio::test]
async fn resume_reconnects_a_serialized_state_whose_external_mount_has_no_credentials() {
    let manifest = Manifest::new().with_entry(
        "data",
        Entry::mount(s3("example-bucket", None, rclone_volume())),
    );
    let fake = Arc::new(FakeDocker::new().with_container("container", running_container()));
    let client = client(&fake);
    let state = docker_state(manifest, "container").with_workspace_root_ready(true);

    let restored = round_trip(&client, &state);
    assert!(!restored.mount_authority_redacted());
    assert!(!restored.mount_authority_rebound());
    assert_eq!(restored.session_id(), state.session_id());
    assert_eq!(fields(&restored).container_id(), "container");
    assert!(restored.workspace_root_ready());

    let session = client.resume(restored).await.expect("resumed");
    assert_eq!(fields(&session.state()).container_id(), "container");
    assert!(creates(&fake).is_empty());
}

#[tokio::test]
async fn a_scrubbed_identity_is_not_reconnected_even_from_a_tampered_state() {
    let manifest = credentialed_manifest("previous-secret-key");
    let fake = Arc::new(
        FakeDocker::new()
            .with_image(IMAGE)
            .with_container("surviving-container", running_container()),
    );
    let client = client(&fake);
    let state = docker_state(manifest, "surviving-container");

    let mut payload = client.serialize_session_state(&state).expect("serialize");
    assert_eq!(payload["container_id"], "");
    let scrubbed = Uuid::parse_str(payload["session_id"].as_str().expect("id")).expect("uuid");
    assert_ne!(scrubbed, state.session_id());
    let object = payload.as_object_mut().expect("object");
    object.remove(REDACTED_MOUNT_AUTHORITY_KEY);
    object["manifest"]["entries"]
        .as_object_mut()
        .expect("entries")
        .remove("data");
    let restored = deserialize(&client, payload).expect("reads");

    let session = client.resume(restored).await.expect("resumed");

    assert_eq!(session.state().session_id(), scrubbed);
    assert_eq!(fields(&session.state()).container_id(), "created-1");
    assert!(
        !fake
            .inspects
            .lock()
            .expect("inspects")
            .contains(&"surviving-container".to_owned())
    );
}

#[tokio::test]
async fn rebound_authority_replaces_even_a_container_that_still_exists() {
    let previous = credentialed_manifest("previous-secret-key");
    let current = Manifest::new().with_entry(
        "data",
        Entry::mount(s3(
            "bucket",
            Some(("current-access-key", "current-secret-key")),
            rclone_volume(),
        )),
    );
    let fake = Arc::new(
        FakeDocker::new()
            .with_image(IMAGE)
            .with_container("container", running_container()),
    );
    let client = client(&fake);
    let state = persisted_and_rebound(&client, &docker_state(previous, "container"), &current);
    let original_id = state.session_id();

    let session = client.resume(state).await.expect("resumed");

    let options = creates(&fake)[0].mounts().expect("mounts")[0]
        .driver_config()
        .expect("driver")
        .options()
        .clone();
    assert_eq!(options["s3-secret-access-key"], "current-secret-key");
    assert!(session.state().mount_authority_rebound());
    assert_ne!(session.state().session_id(), original_id);
    assert_eq!(fields(&session.state()).container_id(), "created-1");
}

#[tokio::test]
async fn resume_accepts_a_live_container_with_a_credentialless_external_mount() {
    let manifest = Manifest::new().with_entry(
        "data",
        Entry::mount(s3("example-bucket", None, rclone_volume())),
    );
    let fake = Arc::new(FakeDocker::new().with_container("container", running_container()));

    let session = client(&fake)
        .resume(docker_state(manifest.clone(), "container"))
        .await
        .expect("resumed");

    assert_eq!(session.state().manifest(), &manifest);
    assert!(creates(&fake).is_empty());
}

#[tokio::test]
async fn a_direct_state_carrying_authority_is_given_a_fresh_container() {
    let manifest = credentialed_manifest("current-secret-key");
    let fake = Arc::new(
        FakeDocker::new()
            .with_image(IMAGE)
            .with_container("existing-container", running_container()),
    );
    let state = docker_state(manifest.clone(), "existing-container");
    let original_id = state.session_id();

    let session = client(&fake).resume(state).await.expect("resumed");

    assert_ne!(session.state().session_id(), original_id);
    assert_eq!(fields(&session.state()).container_id(), "created-1");
    assert!(!session.workspace_state_preserved_on_start());
}

#[tokio::test]
async fn a_reused_container_must_bind_exactly_the_trusted_host_paths() {
    let tmp = tempfile::tempdir().expect("tmp");
    let host_path = std::fs::canonicalize(tmp.path())
        .expect("canonical")
        .join("shared-data");
    std::fs::create_dir(&host_path).expect("mkdir");
    let manifest = Manifest::new().with_path_grant(
        SandboxPathGrant::new("/mnt/shared-data")
            .expect("grant")
            .with_host_path(&host_path.to_string_lossy())
            .expect("host path")
            .read_only(true),
    );
    let bind = |rw: bool| {
        json!({
            "State": {"Status": "running"},
            "Mounts": [{
                "Type": "bind",
                "Source": host_path.to_string_lossy(),
                "Destination": "/mnt/shared-data",
                "RW": rw,
            }],
        })
    };

    let matching = Arc::new(FakeDocker::new().with_container("container", bind(false)));
    client(&matching)
        .resume(docker_state(manifest.clone(), "container"))
        .await
        .expect("matches");

    let mismatched = Arc::new(FakeDocker::new().with_container("container", running_container()));
    let error = client(&mismatched)
        .resume(docker_state(manifest, "container"))
        .await
        .failure("does not match");
    assert!(
        error
            .message()
            .contains("does not match the current trusted manifest"),
        "{error}"
    );

    for manifest in [
        Manifest::new().with_path_grant(SandboxPathGrant::new("/mnt/shared-data").expect("grant")),
        Manifest::new(),
    ] {
        let stale = Arc::new(FakeDocker::new().with_container("container", bind(true)));
        let error = client(&stale)
            .resume(docker_state(manifest, "container"))
            .await
            .failure("stale bind");
        assert!(
            error
                .message()
                .contains("not present in the current trusted manifest"),
            "{error}"
        );
    }
}

#[tokio::test]
async fn a_replacement_starts_unready_and_provisions_accounts() {
    let fake = Arc::new(FakeDocker::new().with_image(IMAGE));
    fake.on_create(|_, _| Ok("replacement".to_owned()));
    let state = docker_state(Manifest::new().with_root("/workspace"), "missing")
        .with_workspace_root_ready(true)
        .with_exposed_ports([8765])
        .expect("ports");

    let session = client(&fake).resume(state).await.expect("resumed");

    let resumed = session.state();
    assert_eq!(fields(&resumed).container_id(), "replacement");
    assert!(!resumed.workspace_root_ready());
    assert!(session.should_provision_accounts());
    let spec = creates(&fake).remove(0);
    assert_eq!(spec.ports().expect("ports")[0].container_port(), "8765/tcp");
    assert_eq!(spec.network_mode(), None);
}

#[tokio::test]
async fn a_replacement_carries_the_persisted_labels() {
    let labels = BTreeMap::from([("com.example.owner".to_owned(), "worker-123".to_owned())]);
    let fake = Arc::new(FakeDocker::new().with_image(IMAGE));
    let base = docker_state(Manifest::new().with_root("/workspace"), "missing");
    let state = fields(&base).with_labels(labels.clone()).apply(base);

    client(&fake).resume(state).await.expect("resumed");

    assert_eq!(creates(&fake)[0].labels(), Some(&labels));
}

#[tokio::test]
async fn a_container_is_reused_only_when_it_carries_the_persisted_labels() {
    let expected = BTreeMap::from([("com.example.owner".to_owned(), "worker-123".to_owned())]);
    let labelled =
        |labels: Value| json!({"State": {"Status": "running"}, "Config": {"Labels": labels}});
    let base = docker_state(Manifest::new().with_root("/workspace"), "container");
    let state = fields(&base).with_labels(expected.clone()).apply(base);

    let extra = Arc::new(FakeDocker::new().with_container(
        "container",
        labelled(json!({"com.example.owner": "worker-123", "com.example.extra": "preserved"})),
    ));
    client(&extra).resume(state.clone()).await.expect("reused");
    assert!(creates(&extra).is_empty());

    for actual in [json!({}), json!({"com.example.owner": "different"})] {
        let fake = Arc::new(FakeDocker::new().with_container("container", labelled(actual)));
        let error = client(&fake)
            .resume(state.clone())
            .await
            .failure("mismatched labels");
        assert!(error.message().contains("labels"), "{error}");
    }
}

#[tokio::test]
async fn the_first_command_after_a_resume_finds_the_workspace_and_runs_in_it() {
    let fake = Arc::new(FakeDocker::new().with_container("container", running_container()));
    fake.workspace_exists
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let state = docker_state(Manifest::new().with_root("/workspace"), "container");

    let session = client(&fake).resume(state).await.expect("resumed");
    let result = session
        .exec(
            ExecRequest::new(["find".to_owned(), ".".to_owned()])
                .with_shell(ShellInvocation::None)
                .with_timeout_s(0.01),
        )
        .await
        .expect("exec");

    assert!(result.ok());
    assert!(session.state().workspace_root_ready());
    let calls: Vec<_> = fake
        .execs
        .lock()
        .expect("execs")
        .iter()
        .map(|call| (call.cmd.join(" "), call.workdir.clone()))
        .collect();
    assert_eq!(
        calls,
        vec![
            ("test -d /workspace".to_owned(), None),
            ("find .".to_owned(), Some("/workspace".to_owned())),
        ]
    );
}

// --- network mode on resume ----------------------------------------------------------------

fn isolated_state(container_id: &str) -> SandboxSessionState {
    let base = docker_state(Manifest::new(), container_id);
    fields(&base)
        .with_network_mode(Some(DockerNetworkMode::None))
        .apply(base)
}

#[tokio::test]
async fn a_replacement_gets_the_persisted_network_mode() {
    let fake = Arc::new(FakeDocker::new().with_image(IMAGE));
    fake.on_create(|_, _| Ok("replacement-container".to_owned()));

    let session = client(&fake)
        .resume(isolated_state("missing-container"))
        .await
        .expect("resumed");

    assert_eq!(creates(&fake)[0].network_mode(), Some("none"));
    assert_eq!(
        fields(&session.state()).container_id(),
        "replacement-container"
    );
}

#[tokio::test]
async fn a_container_that_is_not_network_isolated_is_not_reused() {
    for attrs in [
        json!({"Mounts": [], "HostConfig": {"NetworkMode": "bridge"}, "NetworkSettings": {"Networks": {"bridge": {}}}}),
        json!({"Mounts": [], "HostConfig": {"NetworkMode": "none"}, "NetworkSettings": {"Networks": {"bridge": {}}}}),
        json!({"Mounts": [], "HostConfig": {"NetworkMode": "none"}, "NetworkSettings": {}}),
    ] {
        let fake = Arc::new(
            FakeDocker::new()
                .with_image(IMAGE)
                .with_container("existing-container", attrs),
        );

        let error = client(&fake)
            .resume(isolated_state("existing-container"))
            .await
            .failure("not isolated");

        assert!(error.message().contains("network"), "{error}");
        assert!(creates(&fake).is_empty());
    }
}

#[tokio::test]
async fn a_network_isolated_container_is_reused() {
    for networks in [json!({}), json!({"none": {}})] {
        let fake = Arc::new(FakeDocker::new().with_container(
            "existing-container",
            json!({
                "State": {"Status": "running"},
                "Mounts": [],
                "HostConfig": {"NetworkMode": "none"},
                "NetworkSettings": {"Networks": networks},
            }),
        ));

        client(&fake)
            .resume(isolated_state("existing-container"))
            .await
            .expect("reused");

        assert!(creates(&fake).is_empty());
    }
}

#[test]
fn image_references_split_into_repository_and_tag_as_docker_py_splits_them() {
    use ra_sandbox::docker::parse_repository_tag;
    assert_eq!(
        parse_repository_tag("localhost:5000/myimg:latest"),
        ("localhost:5000/myimg".to_owned(), Some("latest".to_owned()))
    );
    assert_eq!(
        parse_repository_tag("localhost:5000/myimg"),
        ("localhost:5000/myimg".to_owned(), None)
    );
    assert_eq!(
        parse_repository_tag("python@sha256:abc"),
        ("python".to_owned(), Some("sha256:abc".to_owned()))
    );
    assert_eq!(parse_repository_tag("python"), ("python".to_owned(), None));
}

#[tokio::test]
async fn a_reused_bind_mount_source_is_compared_after_normalization() {
    let tmp = tempfile::tempdir().expect("tmp");
    let host_path = std::fs::canonicalize(tmp.path())
        .expect("canonical")
        .join("shared-data");
    std::fs::create_dir(&host_path).expect("mkdir");
    let manifest = Manifest::new().with_path_grant(
        SandboxPathGrant::new("/mnt/shared-data")
            .expect("grant")
            .with_host_path(&host_path.to_string_lossy())
            .expect("host path"),
    );
    // A source reported with a redundant component and a trailing slash is the same path.
    let reported = format!("{}/./", host_path.to_string_lossy());
    let fake = Arc::new(FakeDocker::new().with_container(
        "container",
        json!({
            "State": {"Status": "running"},
            "Mounts": [{"Type": "bind", "Source": reported, "Destination": "/mnt/shared-data", "RW": true}],
        }),
    ));

    client(&fake)
        .resume(docker_state(manifest, "container"))
        .await
        .expect("the same host path");
}

#[tokio::test]
async fn every_state_serializer_scrubs_mount_provider_identity() {
    let fake = Arc::new(
        FakeDocker::new()
            .with_image(IMAGE)
            .with_container("surviving-container", running_container()),
    );
    let client = client(&fake);
    let state = docker_state(
        credentialed_manifest("previous-secret-key"),
        "surviving-container",
    )
    .with_session_id(session_id())
    .with_workspace_root_ready(true);
    let expected_id = Uuid::new_v5(
        &Uuid::NAMESPACE_URL,
        format!("openai-agents:mount-authority-redacted:{}", session_id()).as_bytes(),
    );
    let payloads = [
        state.to_json().expect("direct serialization"),
        serde_json::to_value(&state).expect("serde serialization"),
        client
            .serialize_session_state(&state)
            .expect("client serialization"),
    ];
    for mut payload in payloads {
        assert_eq!(payload["container_id"], "");
        assert_eq!(payload["session_id"], expected_id.to_string());
        assert_eq!(payload["workspace_root_ready"], false);
        assert!(!payload.to_string().contains("previous-secret-key"));
        let object = payload.as_object_mut().expect("object");
        object.remove(REDACTED_MOUNT_AUTHORITY_KEY);
        object["manifest"]["entries"]
            .as_object_mut()
            .expect("entries")
            .remove("data");
        let session = client
            .resume(deserialize(&client, payload).expect("state"))
            .await
            .expect("resume");
        assert_ne!(
            fields(&session.state()).container_id(),
            "surviving-container"
        );
    }
    assert_eq!(fields(&state).container_id(), "surviving-container");
    assert_eq!(state.session_id(), session_id());
    assert!(state.workspace_root_ready());
}

#[test]
fn deserialized_and_rebound_states_keep_the_serializer_policy() {
    let fake = Arc::new(FakeDocker::new());
    let client = client(&fake);
    let trusted = credentialed_manifest("trusted-secret");
    let state = docker_state(trusted.clone(), "original");
    let restored = round_trip(&client, &state);
    let rebound = restored
        .rebind_persisted_mount_authority(Some(&trusted), DOCKER_BACKEND_ID)
        .expect("rebound")
        .with_field("container_id", "replacement");
    assert_eq!(rebound.to_json().expect("serialize")["container_id"], "");
    assert_eq!(
        serde_json::to_value(&rebound).expect("serde")["container_id"],
        ""
    );
}

#[tokio::test]
async fn cancelled_create_collects_the_delayed_container_id_and_removes_it() {
    let fake = Arc::new(FakeDocker::new().with_image(IMAGE));
    fake.hang_creates
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let client = client(&fake);
    let outcome = tokio::time::timeout(
        Duration::from_millis(20),
        client.create(CreateRequest::new().with_options(options().to_payload())),
    )
    .await;
    assert!(outcome.is_err());
    assert_eq!(fake.containers.lock().expect("containers").len(), 1);
    fake.create_response.notify_one();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if fake.containers.lock().expect("containers").is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("cancelled create cleaned up");
    assert_eq!(
        *fake.container_removals.lock().expect("removals"),
        [("created-1".to_owned(), true)]
    );
}

#[tokio::test]
async fn cancelling_during_failure_cleanup_does_not_interrupt_removal() {
    let fake = Arc::new(FakeDocker::new().with_image(IMAGE));
    *fake.start_error.lock().expect("start") = Some(DockerApiError::api(500, "start failed"));
    fake.hang_removals
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let client = client(&fake);
    let outcome = tokio::time::timeout(
        Duration::from_millis(20),
        client.create(CreateRequest::new().with_options(options().to_payload())),
    )
    .await;
    assert!(outcome.is_err());
    assert_eq!(fake.container_removals.lock().expect("removals").len(), 1);
    fake.remove_response.notify_one();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if fake.containers.lock().expect("containers").is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("failure cleanup finished");
}

struct FailingBindSink;

#[async_trait]
impl ra_core::sandbox::EventSink for FailingBindSink {
    fn mode(&self) -> ra_core::sandbox::DeliveryMode {
        ra_core::sandbox::DeliveryMode::Sync
    }

    fn on_error(&self) -> ra_core::sandbox::OnErrorPolicy {
        ra_core::sandbox::OnErrorPolicy::Raise
    }

    fn bind(&self, _session: Arc<dyn SandboxSession>) -> SandboxResult<()> {
        Err(SandboxError::new(
            ErrorCode::SandboxConfigInvalid,
            ra_core::sandbox::OpName::Start,
            "binding failed with secret-key",
        )
        .with_context("detail", "secret-key")
        .with_cause(std::io::Error::other("secret-key")))
    }

    async fn handle(
        &self,
        _event: ra_core::sandbox::SandboxSessionEvent,
    ) -> Result<(), ra_core::sandbox::SinkError> {
        Ok(())
    }
}

#[tokio::test]
async fn binding_failures_are_redacted_on_create_and_resume_and_release_acquired_resources() {
    for resume in [false, true] {
        for protected in [false, true] {
            let fake = Arc::new(FakeDocker::new().with_image(IMAGE));
            let sink: Arc<dyn ra_core::sandbox::EventSink> = Arc::new(FailingBindSink);
            let instrumentation =
                Arc::new(ra_sandbox::instrumentation::Instrumentation::with_sinks([
                    sink,
                ]));
            let client = client(&fake).with_instrumentation(instrumentation);
            let manifest = if protected {
                credentialed_manifest("secret-key")
            } else {
                Manifest::new()
            };
            let result = if resume {
                client
                    .resume(docker_state(manifest, "missing-container"))
                    .await
            } else {
                client
                    .create(
                        CreateRequest::new()
                            .with_manifest(manifest)
                            .with_options(options().to_payload()),
                    )
                    .await
            };
            let error = result.failure("sink binding failed");
            assert_eq!(error.is_data_redacted(), protected);
            if protected {
                assert!(!format!("{error:?}").contains("secret-key"));
                assert!(error.context().is_empty());
                assert!(std::error::Error::source(&error).is_none());
            } else {
                assert_eq!(error.message(), "binding failed with secret-key");
                assert!(std::error::Error::source(&error).is_some());
            }
            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    if !fake.container_removals.lock().unwrap().is_empty() {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("failed acquisition is cleaned up");
        }
    }
}
