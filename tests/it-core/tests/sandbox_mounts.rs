//! `ra-core::sandbox::entries::mounts`: external storage attached inside the workspace.
//!
//! The behavior pinned here is what decides whether a mount is usable and what it may reach:
//! - the support matrix, and that an unsupported strategy is refused while the manifest is read
//! - a mount is ephemeral and stays that way, whatever a caller or a payload says
//! - where a mount actually attaches, which is not always where it was declared
//! - which fields of which provider are authority, which is not the same question as "is it secret"

use ra_core::sandbox::{
    AzureBlobMount, BUILTIN_MOUNT_TYPES, BoxMount, DEFAULT_S3_PROVIDER, Entry, EntryContent,
    ErrorCode, FuseOptions, GcsMount, Manifest, ManifestRegistries, Mount, MountConfigError,
    MountPattern, MountProvider, MountStrategy, MountpointOptions, R2Mount, RcloneOptions,
    S3FilesMount, S3FilesOptions, S3Mount, authority_fields, authority_file_fields,
    configured_authority_fields, credential_set, url_carries_inline_authority, url_fields,
};
use serde_json::{Value, json};

fn s3() -> MountProvider {
    MountProvider::S3(S3Mount {
        bucket: "shared".to_owned(),
        s3_provider: "AWS".to_owned(),
        ..S3Mount::default()
    })
}

fn rclone() -> MountStrategy {
    MountStrategy::in_container(MountPattern::Rclone(RcloneOptions::default()))
}

fn mount(provider: MountProvider, strategy: MountStrategy) -> Mount {
    Mount::new(provider, strategy).expect("supported combination")
}

fn parse(value: &Value) -> Entry {
    Entry::parse(&ManifestRegistries::builtin(), value).expect("entry parses")
}

fn round_trip(entry: &Entry) -> Entry {
    parse(&entry.to_json().expect("entry renders"))
}

// --- the support matrix -----------------------------------------------------------------------

#[test]
fn every_modelled_provider_supports_at_least_one_way_of_attaching_it() {
    // A mount type that supports neither strategy could never be attached, so declaring one is a
    // mistake the manifest should not carry.
    let providers = [
        s3(),
        MountProvider::Gcs(GcsMount::default()),
        MountProvider::AzureBlob(AzureBlobMount::default()),
        MountProvider::Box(BoxMount::default()),
        MountProvider::R2(R2Mount::default()),
        MountProvider::S3Files(S3FilesMount::default()),
    ];

    assert_eq!(providers.len(), BUILTIN_MOUNT_TYPES.len());
    for provider in &providers {
        assert!(
            !provider.supported_patterns().is_empty() || !provider.supported_drivers().is_empty(),
            "{} must support some strategy",
            provider.type_name()
        );
        assert!(provider.is_modelled());
    }
}

#[test]
fn the_support_matrix_says_which_tool_can_attach_which_provider() {
    let cases: [(MountProvider, &[&str], &[&str]); 6] = [
        (s3(), &["rclone", "mountpoint"], &["mountpoint", "rclone"]),
        (
            MountProvider::Gcs(GcsMount::default()),
            &["rclone", "mountpoint"],
            &["mountpoint", "rclone"],
        ),
        (
            MountProvider::AzureBlob(AzureBlobMount::default()),
            &["rclone", "fuse"],
            &["rclone"],
        ),
        (
            MountProvider::Box(BoxMount::default()),
            &["rclone"],
            &["rclone"],
        ),
        (
            MountProvider::R2(R2Mount::default()),
            &["rclone"],
            &["rclone"],
        ),
        // S3 Files is attached by a helper inside the sandbox; there is no volume driver for it.
        (
            MountProvider::S3Files(S3FilesMount::default()),
            &["s3files"],
            &[],
        ),
    ];

    for (provider, patterns, drivers) in cases {
        assert_eq!(provider.supported_patterns(), patterns, "{provider:?}");
        assert_eq!(provider.supported_drivers(), drivers, "{provider:?}");
    }
}

#[test]
fn a_provider_refuses_a_tool_it_cannot_be_attached_with() {
    // Refused while the manifest is written, rather than as a mount command failing inside a
    // container minutes into a run.
    let error = Mount::new(
        s3(),
        MountStrategy::in_container(MountPattern::Fuse(FuseOptions::default())),
    )
    .unwrap_err();
    assert!(matches!(
        error,
        MountConfigError::UnsupportedPattern { ref pattern, .. } if pattern == "fuse"
    ));
    assert_eq!(error.to_string(), "invalid mount_pattern type");

    let error = Mount::new(
        MountProvider::S3Files(S3FilesMount::default()),
        MountStrategy::docker_volume("rclone"),
    )
    .unwrap_err();
    assert!(matches!(
        error,
        MountConfigError::UnsupportedDriver { ref driver, .. } if driver == "rclone"
    ));
    assert_eq!(error.to_string(), "invalid Docker volume driver");
}

#[test]
fn a_provider_this_crate_does_not_model_is_not_second_guessed() {
    // Refusing what it cannot evaluate would shut out every host-declared mount type.
    let provider = MountProvider::Extension(
        ra_core::sandbox::DiscriminatedPayload::new("modal_cloud_bucket")
            .with_field("bucket_name", "shared"),
    );

    assert!(!provider.is_modelled());
    assert!(provider.supported_patterns().is_empty());
    assert!(Mount::new(provider, MountStrategy::docker_volume("anything")).is_ok());
}

// --- ephemerality -----------------------------------------------------------------------------

#[test]
fn a_mount_is_ephemeral_and_cannot_be_talked_out_of_it() {
    // A mount is somebody else's storage attached at a path. A snapshot that captured one would be
    // recording their data as if it were the workspace's.
    let entry = Entry::mount(mount(s3(), rclone()));
    assert!(entry.is_ephemeral());
    assert!(entry.clone().ephemeral(false).is_ephemeral());
    assert!(entry.is_dir());
    assert!(entry.permissions().directory);

    // Including when a payload written by something that did not enforce this says otherwise.
    let from_payload = parse(&json!({
        "type": "s3_mount",
        "bucket": "shared",
        "ephemeral": false,
        "mount_strategy": {"type": "in_container", "pattern": {"type": "rclone"}},
    }));
    assert!(from_payload.is_ephemeral());
}

#[test]
fn a_mount_reads_only_unless_it_is_told_otherwise() {
    let read_only = mount(s3(), rclone());
    assert!(read_only.is_read_only());
    assert!(!read_only.writable(true).is_read_only());
}

// --- where a mount lands ----------------------------------------------------------------------

#[test]
fn a_mount_attaches_where_it_was_declared_unless_it_says_otherwise() {
    let manifest = Manifest::new()
        .with_root("/workspace")
        .with_entry("data", Entry::mount(mount(s3(), rclone())));

    let targets = manifest.mount_targets().expect("resolvable");
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0].1.as_str(), "/workspace/data");
}

#[test]
fn an_explicit_relative_mount_path_is_measured_from_the_workspace_root() {
    // Not from where the entry was declared: a manifest stays portable across backends whose
    // concrete roots differ.
    let manifest = Manifest::new()
        .with_root("/workspace")
        .with_entry("logical", Entry::mount(mount(s3(), rclone()).at("actual")));

    let targets = manifest.mount_targets().expect("resolvable");
    assert_eq!(targets[0].1.as_str(), "/workspace/actual");

    // Both names have to be excluded from a snapshot: the declaration is what a reapply looks up,
    // and the attach path is what a snapshot would otherwise walk into.
    let skip: Vec<String> = manifest
        .ephemeral_persistence_paths()
        .expect("resolvable")
        .into_iter()
        .map(|path| path.as_str().to_owned())
        .collect();
    assert_eq!(skip, ["actual", "logical"]);
}

#[test]
fn a_mount_path_that_resolves_back_inside_the_workspace_is_normalized() {
    let manifest = Manifest::new().with_root("/workspace").with_entry(
        "logical",
        Entry::mount(mount(s3(), rclone()).at("/workspace/repo/../actual")),
    );

    assert_eq!(
        manifest.mount_targets().expect("resolvable")[0].1.as_str(),
        "/workspace/actual"
    );
    let skip: Vec<String> = manifest
        .ephemeral_persistence_paths()
        .expect("resolvable")
        .into_iter()
        .map(|path| path.as_str().to_owned())
        .collect();
    assert_eq!(skip, ["actual", "logical"]);
}

#[test]
fn a_mount_path_that_climbs_out_of_the_workspace_is_refused() {
    let manifest = Manifest::new().with_root("/workspace").with_entry(
        "logical",
        Entry::mount(mount(s3(), rclone()).at("/workspace/../../tmp")),
    );

    let error = manifest.mount_targets().unwrap_err();
    assert_eq!(error.error_code(), ErrorCode::InvalidManifestPath);
    assert!(
        error.message().contains("must not escape root"),
        "{}",
        error.message()
    );
    assert!(manifest.ephemeral_persistence_paths().is_err());
}

#[test]
fn a_mount_may_attach_outside_the_workspace_when_it_says_so_outright() {
    // `/mnt/data` is not a workspace path that escaped — it names somewhere else to begin with, and
    // a mount is allowed to attach there. Whether a target is inside the workspace is a question
    // the persistence exclusions ask separately, not a reason to refuse the declaration.
    let manifest = Manifest::new().with_root("/workspace").with_entry(
        "external",
        Entry::mount(mount(s3(), rclone()).at("/mnt/data")),
    );

    let targets = manifest.mount_targets().expect("resolvable");
    assert_eq!(targets[0].1.as_str(), "/mnt/data");

    // It is outside the workspace, so there is nothing under the root to leave out of a snapshot.
    let skip: Vec<String> = manifest
        .ephemeral_persistence_paths()
        .expect("resolvable")
        .into_iter()
        .map(|path| path.as_str().to_owned())
        .collect();
    assert_eq!(skip, ["external"]);
}

#[test]
fn a_windows_drive_mount_path_is_refused_as_absolute() {
    let manifest = Manifest::new().with_root("/workspace").with_entry(
        "logical",
        Entry::mount(mount(s3(), rclone()).at("C:\\tmp\\mount")),
    );

    let error = manifest.mount_targets().unwrap_err();
    assert_eq!(
        error.message(),
        "manifest path must be relative: C:/tmp/mount"
    );
}

#[test]
fn a_pattern_field_of_the_wrong_type_is_refused_rather_than_read_as_unset() {
    // Reading `mode: 123` as "unset" would silently pick FUSE, which is the sort of typo that turns
    // into a mount nobody asked for rather than an error somebody can fix.
    let cases = [
        json!({"type": "rclone", "mode": 123}),
        json!({"type": "rclone", "remote_name": 7}),
        json!({"type": "fuse", "allow_other": "yes"}),
        json!({"type": "fuse", "log_type": 1}),
        json!({"type": "fuse", "cache_size_mb": "large"}),
        json!({"type": "fuse", "block_cache_block_size_mb": -1}),
        json!({"type": "mountpoint", "options": "none"}),
        json!({"type": "mountpoint", "options": {"region": 1}}),
    ];

    for pattern in cases {
        let payload = json!({
            "type": "s3_mount",
            "bucket": "shared",
            "mount_strategy": {"type": "in_container", "pattern": pattern.clone()},
        });
        assert!(
            Entry::parse(&ManifestRegistries::builtin(), &payload).is_err(),
            "{pattern} must be refused"
        );
    }

    // An absent field still falls back to its default.
    let defaulted = parse(&json!({
        "type": "azure_blob_mount",
        "account": "a",
        "container": "c",
        "mount_strategy": {"type": "in_container", "pattern": {"type": "fuse"}},
    }));
    let EntryContent::Mount(mount) = defaulted.content() else {
        panic!("a mount entry holds a mount");
    };
    let MountStrategy::InContainer {
        pattern: MountPattern::Fuse(options),
    } = mount.strategy()
    else {
        panic!("the pattern is FUSE");
    };
    assert_eq!(options, &FuseOptions::default());
}

#[test]
fn an_s3_mount_names_the_same_vendor_however_it_was_built() {
    // A derived `Default` would leave `s3_provider` empty, so a mount built in code and the same
    // mount read from JSON would disagree about which vendor's S3 they are talking to.
    assert_eq!(S3Mount::default().s3_provider, DEFAULT_S3_PROVIDER);

    let parsed = parse(&json!({
        "type": "s3_mount",
        "bucket": "shared",
        "mount_strategy": {"type": "in_container", "pattern": {"type": "rclone"}},
    }));
    let EntryContent::Mount(mount) = parsed.content() else {
        panic!("a mount entry holds a mount");
    };
    let MountProvider::S3(provider) = mount.provider() else {
        panic!("the provider is S3");
    };
    assert_eq!(provider.s3_provider, DEFAULT_S3_PROVIDER);
    assert_eq!(
        provider,
        &S3Mount {
            bucket: "shared".to_owned(),
            ..S3Mount::default()
        }
    );
}

#[test]
fn mount_targets_come_back_deepest_first() {
    // A mount nested inside another has to be detached before the one it sits in.
    let parent = mount(s3(), rclone()).at("repo");
    let child = mount(s3(), rclone()).at("repo/sub");
    let manifest = Manifest::new()
        .with_root("/workspace")
        .with_entry("parent", Entry::mount(parent))
        .with_entry(
            "nested",
            Entry::dir().with_child("child", Entry::mount(child)),
        );

    let targets = manifest.mount_targets().expect("resolvable");
    let paths: Vec<&str> = targets.iter().map(|(_, path)| path.as_str()).collect();
    assert_eq!(paths, ["/workspace/repo/sub", "/workspace/repo"]);
}

// --- the wire -----------------------------------------------------------------------------------

#[test]
fn a_mount_round_trips_with_its_provider_strategy_and_pattern() {
    let entry = Entry::mount(
        mount(
            MountProvider::S3(S3Mount {
                bucket: "shared".to_owned(),
                access_key_id: Some("AKIA".to_owned()),
                secret_access_key: Some("secret".to_owned()),
                prefix: Some("data/".to_owned()),
                region: Some("us-east-1".to_owned()),
                s3_provider: "AWS".to_owned(),
                ..S3Mount::default()
            }),
            MountStrategy::in_container(MountPattern::Mountpoint(MountpointOptions {
                region: Some("us-east-1".to_owned()),
                ..MountpointOptions::default()
            })),
        )
        .at("data")
        .writable(true),
    );

    let rendered = entry.to_json().expect("renders");
    assert_eq!(rendered["type"], json!("s3_mount"));
    assert_eq!(rendered["bucket"], json!("shared"));
    assert_eq!(rendered["mount_path"], json!("data"));
    assert_eq!(rendered["read_only"], json!(false));
    assert_eq!(rendered["ephemeral"], json!(true));
    assert_eq!(rendered["mount_strategy"]["type"], json!("in_container"));
    assert_eq!(
        rendered["mount_strategy"]["pattern"]["type"],
        json!("mountpoint")
    );
    assert_eq!(
        rendered["mount_strategy"]["pattern"]["options"]["region"],
        json!("us-east-1")
    );
    assert_eq!(round_trip(&entry), entry);
}

#[test]
fn a_mount_renders_the_fields_the_reference_renders() {
    // Checked against the reference's own output for the same manifest: a mount written here has
    // to be readable there, and a state written there has to be readable here.
    let entry = Entry::mount(
        mount(
            MountProvider::Gcs(GcsMount {
                bucket: "b".to_owned(),
                ..GcsMount::default()
            }),
            MountStrategy::in_container(MountPattern::Mountpoint(MountpointOptions::default())),
        )
        .at("actual"),
    );

    assert_eq!(
        entry.to_json().expect("renders"),
        json!({
            "type": "gcs_mount",
            "bucket": "b",
            "access_id": Value::Null,
            "secret_access_key": Value::Null,
            "prefix": Value::Null,
            "region": Value::Null,
            "endpoint_url": Value::Null,
            "service_account_file": Value::Null,
            "service_account_credentials": Value::Null,
            "access_token": Value::Null,
            "description": Value::Null,
            "ephemeral": true,
            "group": Value::Null,
            "is_dir": true,
            "permissions": {"owner": 7, "group": 5, "other": 5, "directory": true},
            "mount_path": "actual",
            "read_only": true,
            "mount_strategy": {
                "type": "in_container",
                "pattern": {
                    "type": "mountpoint",
                    "options": {
                        "prefix": Value::Null,
                        "region": Value::Null,
                        "endpoint_url": Value::Null,
                    },
                },
            },
        })
    );
}

#[test]
fn a_docker_volume_mount_round_trips_with_its_driver_options() {
    let entry = Entry::mount(mount(
        s3(),
        MountStrategy::DockerVolume {
            driver: "rclone".to_owned(),
            driver_options: [("s3-region".to_owned(), "us-east-1".to_owned())]
                .into_iter()
                .collect(),
        },
    ));

    let rendered = entry.to_json().expect("renders");
    assert_eq!(rendered["mount_strategy"]["driver"], json!("rclone"));
    assert_eq!(
        rendered["mount_strategy"]["driver_options"]["s3-region"],
        json!("us-east-1")
    );
    assert_eq!(round_trip(&entry), entry);
}

#[test]
fn every_modelled_provider_round_trips() {
    let providers = [
        s3(),
        MountProvider::Gcs(GcsMount {
            bucket: "shared".to_owned(),
            access_id: Some("id".to_owned()),
            secret_access_key: Some("secret".to_owned()),
            ..GcsMount::default()
        }),
        MountProvider::AzureBlob(AzureBlobMount {
            account: "account".to_owned(),
            container: "container".to_owned(),
            account_key: Some("key".to_owned()),
            ..AzureBlobMount::default()
        }),
        MountProvider::Box(BoxMount {
            path: Some("/folder".to_owned()),
            access_token: Some("token".to_owned()),
            ..BoxMount::default()
        }),
        MountProvider::R2(R2Mount {
            bucket: "shared".to_owned(),
            account_id: "account".to_owned(),
            access_key_id: Some("id".to_owned()),
            secret_access_key: Some("secret".to_owned()),
            ..R2Mount::default()
        }),
    ];

    for provider in providers {
        let entry = Entry::mount(mount(provider, rclone()));
        assert_eq!(round_trip(&entry), entry, "{:?}", entry.entry_type());
    }

    // S3 Files takes its own pattern, so it is built separately rather than with rclone.
    let s3_files = Entry::mount(mount(
        MountProvider::S3Files(S3FilesMount {
            file_system_id: "fs-1".to_owned(),
            region: Some("us-east-1".to_owned()),
            extra_options: [("tls".to_owned(), None)].into_iter().collect(),
            ..S3FilesMount::default()
        }),
        MountStrategy::in_container(MountPattern::S3Files(S3FilesOptions::default())),
    ));
    assert_eq!(round_trip(&s3_files), s3_files);
}

#[test]
fn a_mount_pattern_naming_a_tool_nothing_can_run_is_refused() {
    // The reference's pattern field is a closed discriminated union. A mount whose tool nothing
    // knows how to run is not a mount.
    let payload = json!({
        "type": "s3_mount",
        "bucket": "shared",
        "mount_strategy": {"type": "in_container", "pattern": {"type": "sshfs"}},
    });

    let error = Entry::parse(&ManifestRegistries::builtin(), &payload).unwrap_err();
    assert!(error.to_string().contains("sshfs"), "{error}");
}

#[test]
fn a_blobfuse_cache_outside_the_workspace_is_refused_when_the_manifest_is_read() {
    // The reference refuses it when the pattern is built, so a manifest carrying one never loads.
    for cache_path in [
        "/tmp/blobfuse-cache",
        "../blobfuse-cache",
        "C:\\blobfuse-cache",
    ] {
        let payload = json!({
            "type": "azure_blob_mount",
            "account": "acct",
            "container": "container",
            "mount_strategy": {
                "type": "in_container",
                "pattern": {"type": "fuse", "cache_path": cache_path},
            },
        });

        let error = Entry::parse(&ManifestRegistries::builtin(), &payload).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("blobfuse cache_path must be relative to the workspace root"),
            "{cache_path}: {error}"
        );
    }
}

#[test]
fn a_mount_strategy_a_host_registered_is_carried_verbatim() {
    // Strategies are open upstream, so a platform with its own attach mechanism keeps working —
    // once the host has said it has one.
    let mut registries = ManifestRegistries::builtin();
    registries
        .mount_strategies_mut()
        .register("modal_cloud_bucket", "test")
        .expect("the type is free");
    let payload = json!({
        "type": "s3_mount",
        "bucket": "shared",
        "mount_strategy": {"type": "modal_cloud_bucket", "secret_name": "aws"},
    });

    let entry = Entry::parse(&registries, &payload).expect("host knows the strategy");
    let EntryContent::Mount(mount) = entry.content() else {
        panic!("a mount entry holds a mount");
    };
    assert_eq!(mount.strategy().type_name(), "modal_cloud_bucket");
    assert_eq!(
        Entry::parse(&registries, &entry.to_json().expect("renders")).expect("re-parses"),
        entry
    );
}

#[test]
fn a_registered_mount_strategy_can_reject_its_configuration() {
    let mut registries = ManifestRegistries::builtin();
    registries
        .mount_strategies_mut()
        .register_with("custom", "test", |_| {
            Err("required config missing".to_owned())
        })
        .expect("the type is free");
    let error = Entry::parse(
        &registries,
        &json!({
            "type": "s3_mount",
            "bucket": "shared",
            "mount_strategy": {"type": "custom"},
        }),
    )
    .expect_err("the strategy owner rejected the configuration");
    assert!(
        error.to_string().contains("required config missing"),
        "{error}"
    );
}

#[test]
fn a_registered_mount_strategy_preserves_its_normalized_configuration() {
    let mut registries = ManifestRegistries::builtin();
    registries
        .mount_strategies_mut()
        .register_with("custom", "test", |payload| {
            Ok(payload.with_field("region", "normalized-region"))
        })
        .expect("the type is free");
    let entry = Entry::parse(
        &registries,
        &json!({
            "type": "s3_mount",
            "bucket": "shared",
            "mount_strategy": {"type": "custom", "region": "raw-region"},
        }),
    )
    .expect("the strategy owner normalizes the configuration");
    let rendered = entry.to_json().expect("renders");
    assert_eq!(rendered["mount_strategy"]["region"], "normalized-region");
    assert_eq!(
        Entry::parse(&registries, &rendered).expect("re-parses"),
        entry
    );
}

#[test]
fn a_mount_strategy_nobody_registered_is_refused() {
    // A typo in the discriminator would otherwise become an extension nothing knows how to attach:
    // the mount would be declared, accepted, and then simply never appear.
    let error = Entry::parse(
        &ManifestRegistries::builtin(),
        &json!({
            "type": "s3_mount",
            "bucket": "shared",
            "mount_strategy": {"type": "in_containr"},
        }),
    )
    .unwrap_err();

    assert!(error.to_string().contains("in_containr"), "{error}");
}

#[test]
fn the_built_in_strategy_registry_holds_the_two_this_crate_models() {
    let registries = ManifestRegistries::builtin();

    assert_eq!(
        registries
            .mount_strategies()
            .registered_types()
            .collect::<Vec<_>>(),
        ["docker_volume", "in_container"]
    );
}

#[test]
fn an_unsupported_combination_is_refused_when_a_manifest_is_read() {
    let payload = json!({
        "entries": {"data": {
            "type": "s3_files_mount",
            "file_system_id": "fs-1",
            "mount_strategy": {"type": "docker_volume", "driver": "rclone"},
        }},
    });

    assert!(Manifest::parse(&ManifestRegistries::builtin(), &payload).is_err());
}

// --- authority ----------------------------------------------------------------------------------

#[test]
fn authority_is_not_the_same_question_as_whether_a_field_is_secret() {
    // An Azure managed-identity client id is not a secret and still selects an identity the sandbox
    // may borrow; a path to a service-account file names authority the sandbox can then read.
    assert!(authority_fields("azure_blob_mount").contains(&"identity_client_id"));
    assert!(authority_fields("gcs_mount").contains(&"service_account_file"));
    assert_eq!(authority_file_fields("gcs_mount"), ["service_account_file"]);
    assert_eq!(authority_file_fields("box_mount"), ["box_config_file"]);
    assert_eq!(
        authority_fields("s3_mount"),
        ["access_key_id", "secret_access_key", "session_token"]
    );
    // A bucket name is not authority: knowing which bucket does not let anyone reach it.
    assert!(!authority_fields("s3_mount").contains(&"bucket"));
    assert!(authority_fields("no_such_mount").is_empty());
}

#[test]
fn only_the_authority_a_mount_actually_carries_is_reported() {
    // The question a host asks before letting a manifest cross a trust boundary.
    let ambient = mount(s3(), rclone());
    assert!(configured_authority_fields(&ambient).is_empty());

    let with_keys = mount(
        MountProvider::S3(S3Mount {
            bucket: "shared".to_owned(),
            access_key_id: Some("AKIA".to_owned()),
            secret_access_key: Some("secret".to_owned()),
            ..S3Mount::default()
        }),
        rclone(),
    );
    let configured = configured_authority_fields(&with_keys);
    assert!(configured.contains("access_key_id"));
    assert!(configured.contains("secret_access_key"));
    assert!(!configured.contains("session_token"));
}

#[test]
fn a_url_field_counts_as_authority_when_the_url_carries_credentials() {
    assert_eq!(url_fields("s3_mount"), ["endpoint_url"]);
    assert!(url_carries_inline_authority("https://user:pw@example.com"));
    assert!(url_carries_inline_authority(
        "https://example.com/?token=abc"
    ));
    assert!(!url_carries_inline_authority("https://example.com/path"));

    let with_inline_credentials = mount(
        MountProvider::S3(S3Mount {
            bucket: "shared".to_owned(),
            endpoint_url: Some("https://key:secret@s3.example.com".to_owned()),
            ..S3Mount::default()
        }),
        rclone(),
    );
    assert!(configured_authority_fields(&with_inline_credentials).contains("endpoint_url"));
}

#[test]
fn third_party_driver_options_count_as_authority_as_a_whole() {
    // Option names from somebody else's driver cannot be classified, so the field is authority
    // whenever it holds anything at all.
    let with_options = mount(
        s3(),
        MountStrategy::DockerVolume {
            driver: "rclone".to_owned(),
            driver_options: [("s3-secret-access-key".to_owned(), "secret".to_owned())]
                .into_iter()
                .collect(),
        },
    );

    assert!(configured_authority_fields(&with_options).contains("mount_strategy.driver_options"));
    assert!(
        configured_authority_fields(&mount(s3(), MountStrategy::docker_volume("rclone")))
            .is_empty()
    );
}

#[test]
fn a_credential_set_names_what_must_be_complete_once_any_of_it_is_set() {
    // An S3 mount given only a session token has no key to use it with, and the mount command would
    // fall back to whatever ambient credentials the sandbox happens to have — reaching storage the
    // manifest never named.
    let (activates, required) = credential_set("s3_mount").expect("s3 has a credential set");
    assert_eq!(
        activates,
        ["access_key_id", "secret_access_key", "session_token"]
    );
    assert_eq!(required, ["access_key_id", "secret_access_key"]);

    assert!(credential_set("r2_mount").is_some());
    assert!(credential_set("gcs_mount").is_some());
    // Azure and Box authenticate through a single field, so there is no set to complete.
    assert!(credential_set("azure_blob_mount").is_none());
    assert!(credential_set("box_mount").is_none());
}
