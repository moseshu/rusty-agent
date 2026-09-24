//! `ra-core::sandbox`: the wire shapes that already-written payloads depend on.
//!
//! Ported from the reference's `tests/sandbox/test_compatibility_guards.py`, for the parts this
//! crate owns. A discriminator string, a state field or a released payload shape is read by
//! whatever wrote it last — possibly the other implementation — so each is pinned literally rather
//! than derived from a variant name.
//!
//! Not ported, and why:
//! - the `__all__` export-surface checks: the `public-api` gate snapshots every public item, which
//!   is the same guard against a silent removal, and most of the reference's list belongs to parts
//!   not carried over yet (capabilities, the sandbox agent, run configuration, event sinks)
//! - constructor field order: Python lets callers construct these positionally, so the order is
//!   API there; here the limits types have private fields behind builders and nothing is positional
//! - hosted backends (E2B, Modal, Daytona, Blaxel, Cloudflare, Runloop, Vercel) and Docker: not
//!   ported; the unix-local counterparts are pinned in `it-sandbox/unix_local_client`
//! - key *order* in a rendered state: a JSON object's keys are unordered, and `serde_json` renders
//!   them sorted, so only the key set is pinned
//! - the run-state sandbox envelope: belongs to the run-state port, not to the session layer

use ra_core::sandbox::entries::mounts::{
    AZURE_BLOB_MOUNT_TYPE, GCS_MOUNT_TYPE, R2_MOUNT_TYPE, S3_FILES_MOUNT_TYPE, S3_MOUNT_TYPE,
};
use ra_core::sandbox::entries::{
    DIR_ENTRY_TYPE, FILE_ENTRY_TYPE, GIT_REPO_ENTRY_TYPE, LOCAL_DIR_ENTRY_TYPE,
    LOCAL_FILE_ENTRY_TYPE,
};
use ra_core::sandbox::{
    DEFAULT_REMOTE_MOUNT_COMMAND_ALLOWLIST, DOCKER_VOLUME_STRATEGY_TYPE, Entry, EntryContent,
    EnvValue, FuseOptions, IN_CONTAINER_STRATEGY_TYPE, LOCAL_SNAPSHOT_TYPE, Manifest,
    ManifestRegistries, Mount, MountPattern, MountProvider, MountStrategy, MountpointOptions,
    NOOP_SNAPSHOT_TYPE, REMOTE_SNAPSHOT_TYPE, RcloneOptions, S3FilesOptions, S3Mount,
    STR_ENV_VALUE_TYPE, SandboxSessionState, Snapshot, builtin_env_value_registry,
    builtin_snapshot_registry,
};
use serde_json::{Value, json};
use uuid::Uuid;

/// The fields every session state renders, whichever backend wrote it.
const STATE_FIELDS: [&str; 8] = [
    "type",
    "session_id",
    "snapshot",
    "manifest",
    "exposed_ports",
    "snapshot_fingerprint",
    "snapshot_fingerprint_version",
    "workspace_root_ready",
];

fn keys(value: &Value) -> Vec<&str> {
    let mut keys: Vec<&str> = value
        .as_object()
        .expect("an object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    keys
}

fn sorted(names: &[&'static str]) -> Vec<&'static str> {
    let mut names = names.to_vec();
    names.sort_unstable();
    names
}

#[test]
fn core_discriminator_type_strings_are_stable() {
    let expected = [
        (LOCAL_SNAPSHOT_TYPE, "local"),
        (NOOP_SNAPSHOT_TYPE, "noop"),
        (REMOTE_SNAPSHOT_TYPE, "remote"),
        (DIR_ENTRY_TYPE, "dir"),
        (FILE_ENTRY_TYPE, "file"),
        (LOCAL_FILE_ENTRY_TYPE, "local_file"),
        (LOCAL_DIR_ENTRY_TYPE, "local_dir"),
        (GIT_REPO_ENTRY_TYPE, "git_repo"),
        (S3_MOUNT_TYPE, "s3_mount"),
        (R2_MOUNT_TYPE, "r2_mount"),
        (GCS_MOUNT_TYPE, "gcs_mount"),
        (AZURE_BLOB_MOUNT_TYPE, "azure_blob_mount"),
        (S3_FILES_MOUNT_TYPE, "s3_files_mount"),
        (MountPattern::Fuse(FuseOptions::default()).as_str(), "fuse"),
        (
            MountPattern::Mountpoint(MountpointOptions::default()).as_str(),
            "mountpoint",
        ),
        (
            MountPattern::Rclone(RcloneOptions::default()).as_str(),
            "rclone",
        ),
        (
            MountPattern::S3Files(S3FilesOptions::default()).as_str(),
            "s3files",
        ),
        (IN_CONTAINER_STRATEGY_TYPE, "in_container"),
        (DOCKER_VOLUME_STRATEGY_TYPE, "docker_volume"),
        (STR_ENV_VALUE_TYPE, "str"),
    ];

    for (actual, wire) in expected {
        assert_eq!(actual, wire);
    }
}

#[test]
fn a_session_state_renders_every_modelled_field() {
    // Including the fingerprint pair when there is none, as `null`: a reader checking the released
    // shape field by field finds the same keys whichever implementation wrote the state.
    let state = SandboxSessionState::new(
        "stub",
        Snapshot::new(NOOP_SNAPSHOT_TYPE, "snapshot-123"),
        Manifest::new(),
    )
    .with_exposed_ports([8000])
    .expect("ports")
    .with_workspace_root_ready(true);

    let rendered = state.to_json().expect("render");

    assert_eq!(keys(&rendered), sorted(&STATE_FIELDS));
    assert_eq!(rendered["snapshot_fingerprint"], Value::Null);
    assert_eq!(rendered["snapshot_fingerprint_version"], Value::Null);
}

#[test]
fn a_state_in_the_references_released_shape_reads_back() {
    // What the reference writes for a fresh state, spelled out: every manifest collection written
    // out empty, and the fingerprint pair as `null`.
    let payload = json!({
        "type": "stub",
        "session_id": "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
        "snapshot": {"type": "noop", "id": "snapshot-123"},
        "manifest": {
            "version": 1,
            "root": "/workspace",
            "entries": {},
            "environment": {"value": {}},
            "users": [],
            "groups": [],
            "extra_path_grants": [],
            "remote_mount_command_allowlist": DEFAULT_REMOTE_MOUNT_COMMAND_ALLOWLIST,
        },
        "exposed_ports": [8000],
        "snapshot_fingerprint": null,
        "snapshot_fingerprint_version": null,
        "workspace_root_ready": true,
    });

    let state = SandboxSessionState::parse(
        payload.clone(),
        &builtin_snapshot_registry(),
        &ManifestRegistries::builtin(),
    )
    .expect("parse");

    assert_eq!(
        state.session_id(),
        Uuid::parse_str("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa").expect("uuid")
    );
    assert_eq!(state.snapshot().id(), "snapshot-123");
    assert_eq!(state.manifest(), &Manifest::new());
    assert_eq!(state.exposed_ports(), [8000]);
    assert_eq!(state.snapshot_fingerprint(), None);
    assert!(state.workspace_root_ready());

    // Written back, everything but the manifest is the same payload. The manifest leaves its empty
    // collections out, which each side reads as the same thing.
    let mut rendered = state.to_json().expect("render");
    let mut expected = payload;
    rendered["manifest"] = Value::Null;
    expected["manifest"] = Value::Null;
    assert_eq!(rendered, expected);
}

#[test]
fn mount_strategy_type_strings_round_trip_through_the_registry() {
    let registries = ManifestRegistries::builtin();
    for (strategy, wire) in [
        (
            MountStrategy::in_container(MountPattern::Mountpoint(MountpointOptions::default())),
            "in_container",
        ),
        (MountStrategy::docker_volume("rclone"), "docker_volume"),
    ] {
        let provider = MountProvider::S3(S3Mount {
            bucket: "bucket".to_owned(),
            ..S3Mount::default()
        });
        let manifest = Manifest::new().with_entry(
            "data",
            Entry::mount(Mount::new(provider, strategy).expect("supported")),
        );

        let rendered = serde_json::to_value(&manifest).expect("render");
        let restored = Manifest::parse(&registries, &rendered).expect("parse");

        assert_eq!(
            rendered["entries"]["data"]["mount_strategy"]["type"],
            json!(wire)
        );
        assert_eq!(restored, manifest);
        assert_eq!(serde_json::to_value(&restored).expect("render"), rendered);
    }
}

#[test]
fn core_discriminator_registries_parse_released_payload_shapes() {
    let snapshot = Snapshot::parse(
        &builtin_snapshot_registry(),
        &json!({"type": "noop", "id": "snapshot-123"}),
    )
    .expect("snapshot");
    assert!(snapshot.is_noop());

    let registries = ManifestRegistries::builtin();
    let dir = Entry::parse(
        &registries,
        &json!({"type": "dir", "permissions": {"directory": true}}),
    )
    .expect("dir");
    assert!(matches!(dir.content(), EntryContent::Dir { .. }));

    // A pattern and a strategy are read as part of the mount that carries them, each in the
    // smallest shape the reference accepts.
    let mountpoint = Entry::parse(
        &registries,
        &json!({
            "type": "s3_mount",
            "bucket": "bucket",
            "mount_strategy": {"type": "in_container", "pattern": {"type": "mountpoint"}},
        }),
    )
    .expect("mountpoint");
    let EntryContent::Mount(mount) = mountpoint.content() else {
        panic!("a mount");
    };
    assert!(matches!(
        mount.strategy(),
        MountStrategy::InContainer {
            pattern: MountPattern::Mountpoint(_)
        }
    ));

    let docker = Entry::parse(
        &registries,
        &json!({
            "type": "s3_mount",
            "bucket": "bucket",
            "mount_strategy": {"type": "docker_volume", "driver": "rclone"},
        }),
    )
    .expect("docker volume");
    let EntryContent::Mount(mount) = docker.content() else {
        panic!("a mount");
    };
    assert!(matches!(
        mount.strategy(),
        MountStrategy::DockerVolume { driver, .. } if driver == "rclone"
    ));

    let value = EnvValue::parse(
        &builtin_env_value_registry(),
        &json!({"type": "str", "value": "env-value"}),
    )
    .expect("env value");
    assert_eq!(value.payload().type_name(), STR_ENV_VALUE_TYPE);
    assert_eq!(value.field("value"), Some(&json!("env-value")));
}
