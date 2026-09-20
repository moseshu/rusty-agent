//! `ra-core::sandbox::manifest`: what a session declares its workspace should contain.
//!
//! The behavior pinned here is what a backend reads before it materializes anything:
//! - the defaults a manifest carries, including the commands a remote mount may run
//! - every entry path is checked against the workspace, nested paths included
//! - an ephemeral entry inside a persisted directory is still ephemeral
//! - a manifest is read through the host's registry, so an entry type nobody registered is refused
//! - a path grant is revalidated on the way in, because a grant is authority

use ra_core::sandbox::{
    DEFAULT_MANIFEST_ROOT, DEFAULT_REMOTE_MOUNT_COMMAND_ALLOWLIST, Entry, EnvValue, Environment,
    ErrorCode, Group, MANIFEST_VERSION, Manifest, ManifestParseError, ManifestRegistries, Mount,
    MountCredentialAuthority, MountExposureError, MountPattern, MountProvider, MountStrategy,
    RcloneOptions, S3Mount, SandboxPathGrant, User,
};
use serde_json::{Value, json};

fn parse(value: &Value) -> Manifest {
    Manifest::parse(&ManifestRegistries::builtin(), value).expect("manifest parses")
}

fn paths(manifest: &Manifest) -> Vec<String> {
    manifest
        .iter_entries()
        .expect("entry paths are valid")
        .into_iter()
        .map(|(path, _)| path.as_str().to_owned())
        .collect()
}

// --- defaults -------------------------------------------------------------------------------

#[test]
fn a_fresh_manifest_carries_the_reference_defaults() {
    let manifest = Manifest::new();

    assert_eq!(manifest.version, MANIFEST_VERSION);
    assert_eq!(manifest.root, DEFAULT_MANIFEST_ROOT);
    assert_eq!(manifest.root, "/workspace");
    assert!(manifest.entries.is_empty());
    assert!(manifest.users.is_empty());
    assert!(manifest.groups.is_empty());
    assert!(!manifest.grants_extra_paths());
}

#[test]
fn the_remote_mount_allowlist_reads_and_moves_but_does_not_fetch_or_execute() {
    let allowlist = &DEFAULT_REMOTE_MOUNT_COMMAND_ALLOWLIST;

    assert_eq!(allowlist.len(), 18);
    for expected in ["ls", "find", "stat", "cat", "grep", "cp", "mkdir", "rm"] {
        assert!(allowlist.contains(&expected), "{expected} must be allowed");
    }
    // Nothing that reaches the network or runs a program: widening this is a backend's own
    // manifest to declare, not a default anybody inherits.
    for refused in ["curl", "wget", "sh", "bash", "chmod", "sudo", "ssh"] {
        assert!(
            !allowlist.contains(&refused),
            "{refused} must not be allowed by default"
        );
    }
}

#[test]
fn a_manifest_reads_back_with_every_field_omitted() {
    // `{}` is a valid manifest: an empty workspace at the default root. Making any field required
    // would reject configuration the reference accepts.
    assert_eq!(parse(&json!({})), Manifest::new());
    assert_eq!(
        parse(&json!({"version": 1, "root": "/workspace"})),
        Manifest::new()
    );

    let rooted = parse(&json!({"root": "/srv"}));
    assert_eq!(rooted.version, MANIFEST_VERSION);
    assert_eq!(rooted.root, "/srv");
    assert_eq!(
        rooted.remote_mount_command_allowlist.len(),
        DEFAULT_REMOTE_MOUNT_COMMAND_ALLOWLIST.len()
    );
}

#[test]
fn a_manifest_version_this_does_not_understand_is_refused() {
    // Reading a later format with rules written for this one would materialize a workspace from
    // entries that no longer mean what they used to.
    for unsupported in [
        json!({"version": 2}),
        json!({"version": 0}),
        json!({"version": "1"}),
    ] {
        let error =
            Manifest::parse(&ManifestRegistries::builtin(), &unsupported).expect_err("refuse");
        assert!(
            error.to_string().contains("unsupported manifest version"),
            "{unsupported} must be refused, got {error}"
        );
    }
}

#[test]
fn a_manifest_round_trips_including_the_environment_not_yet_modelled() {
    let mut manifest = Manifest::new()
        .with_root("/srv/work")
        .with_user(User::new("agent"))
        .with_group(Group::new("staff", vec![User::new("agent")]))
        .with_entry("README.md", Entry::local_file("README.md"))
        .with_path_grant(
            SandboxPathGrant::new("/opt/toolchain")
                .expect("absolute")
                .read_only(true),
        );
    manifest.environment = Environment::new().with("PATH", "/usr/bin");

    let rendered = serde_json::to_value(&manifest).expect("serialize");
    let back = parse(&rendered);

    assert_eq!(back, manifest);
    assert_eq!(back.root, "/srv/work");
    assert_eq!(
        rendered["environment"],
        json!({"value": {"PATH": "/usr/bin"}})
    );
    assert_eq!(
        rendered["extra_path_grants"],
        json!([{"path": "/opt/toolchain", "read_only": true, "description": Value::Null}])
    );
}

#[test]
fn a_manifest_reads_its_entries_and_its_environment_through_their_own_registries() {
    // Three open families meet in one manifest, and each needs the registry for its own family.
    // Handing the entry registry to the environment makes a plain `{"type": "str"}` unreadable —
    // including one this implementation just wrote.
    let mut manifest = Manifest::new()
        .with_entry("README.md", Entry::local_file("README.md"))
        .with_entry(
            "data",
            Entry::mount(
                Mount::new(
                    MountProvider::S3(S3Mount {
                        bucket: "shared".to_owned(),
                        ..S3Mount::default()
                    }),
                    MountStrategy::in_container(MountPattern::Rclone(RcloneOptions::default())),
                )
                .expect("supported"),
            ),
        );
    manifest.environment = Environment::new()
        .with("PLAIN", "plain")
        .with_value("TYPED", EnvValue::literal("typed"));

    let rendered = serde_json::to_value(&manifest).expect("serialize");
    assert_eq!(
        rendered["environment"]["value"]["TYPED"],
        json!({"type": "str", "value": "typed"})
    );
    assert_eq!(parse(&rendered), manifest);
}

// --- entry paths ----------------------------------------------------------------------------

#[test]
fn entries_are_walked_parent_first() {
    // A directory has to be created before what is inside it, so it comes first.
    let manifest = Manifest::new()
        .with_entry(
            "repo",
            Entry::dir().with_child("README.md", Entry::file("hi")),
        )
        .with_entry("notes.txt", Entry::file("note"));

    assert_eq!(paths(&manifest), ["notes.txt", "repo", "repo/README.md"]);
}

#[test]
fn a_nested_path_that_escapes_the_workspace_is_refused() {
    // A child declared as `../outside.txt` escapes just as surely as a top-level one, and it is the
    // joined path that gets checked.
    let manifest = Manifest::new().with_entry(
        "safe",
        Entry::dir().with_child("../outside.txt", Entry::file("nope")),
    );

    let error = manifest.validated_entries().expect_err("refuse");
    assert_eq!(error.error_code(), ErrorCode::InvalidManifestPath);
    assert_eq!(
        error.message(),
        "manifest path must not escape root: safe/../outside.txt"
    );
    assert_eq!(error.context().get("reason"), Some(&json!("escape_root")));
}

#[test]
fn a_nested_absolute_path_is_refused() {
    let manifest = Manifest::new().with_entry(
        "safe",
        Entry::dir().with_child("/tmp/outside.txt", Entry::file("nope")),
    );

    let error = manifest.validated_entries().expect_err("refuse");
    assert_eq!(
        error.message(),
        "manifest path must be relative: /tmp/outside.txt"
    );
}

#[test]
fn a_windows_drive_entry_path_is_refused_as_absolute() {
    let manifest = Manifest::new().with_entry("C:\\tmp\\outside.txt", Entry::file("nope"));

    let error = manifest.validated_entries().expect_err("refuse");
    assert_eq!(
        error.message(),
        "manifest path must be relative: C:/tmp/outside.txt"
    );
    assert_eq!(
        error.context().get("rel"),
        Some(&json!("C:/tmp/outside.txt"))
    );
    assert_eq!(error.context().get("reason"), Some(&json!("absolute")));
}

#[test]
fn an_ephemeral_entry_inside_a_persisted_directory_is_still_ephemeral() {
    // Reading only the top level would carry it into a snapshot, which is the one place it must
    // not be.
    let manifest = Manifest::new().with_entry(
        "dir",
        Entry::dir()
            .with_child("keep.txt", Entry::file("keep"))
            .with_child("tmp.txt", Entry::file("tmp").ephemeral(true)),
    );

    let ephemeral: Vec<String> = manifest
        .ephemeral_entry_paths()
        .expect("entry paths are valid")
        .into_iter()
        .map(|path| path.as_str().to_owned())
        .collect();
    assert_eq!(ephemeral, ["dir/tmp.txt"]);
}

#[test]
fn a_manifest_with_nothing_ephemeral_has_nothing_to_reapply() {
    let manifest = Manifest::new().with_entry("keep.txt", Entry::file("keep"));

    assert!(manifest.ephemeral_entry_paths().expect("valid").is_empty());
}

// --- reading one back -------------------------------------------------------------------------

#[test]
fn a_manifest_is_read_through_the_registry_the_host_assembled() {
    // Which entry types a payload may name is the host's assembly, not a property of this crate.
    let payload = json!({
        "entries": {"data": {"type": "blaxel_drive_mount", "drive_name": "shared"}},
    });

    let refused = Manifest::parse(&ManifestRegistries::builtin(), &payload).expect_err("refuse");
    assert!(matches!(
        refused,
        ManifestParseError::InvalidEntry { ref path, .. } if path == "data"
    ));
    assert!(
        refused.to_string().contains("blaxel_drive_mount"),
        "{refused}"
    );

    let mut registries = ManifestRegistries::builtin();
    registries
        .entries_mut()
        .register("blaxel_drive_mount", "test")
        .expect("type is free");
    let manifest = Manifest::parse(&registries, &payload).expect("host knows the type");
    assert_eq!(manifest.entries["data"].entry_type(), "blaxel_drive_mount");
}

#[test]
fn a_grant_is_revalidated_on_the_way_in() {
    // A grant is the one thing in a manifest that widens what a session may reach, so reading one
    // back applies the same rules as writing one.
    for refused in [
        json!({"path": "/"}),
        json!({"path": "tmp"}),
        json!({"path": "/mnt/data", "host_path": "/srv/../secret"}),
        json!({"path": "/mnt/data", "read_only": "yes"}),
    ] {
        let payload = json!({"extra_path_grants": [refused.clone()]});
        let error = Manifest::parse(&ManifestRegistries::builtin(), &payload).expect_err("refuse");
        assert!(
            matches!(error, ManifestParseError::InvalidGrant(_)),
            "{refused} must be refused, got {error}"
        );
    }

    let accepted = parse(&json!({
        "extra_path_grants": [{"path": "/tmp", "description": "scratch"}],
    }));
    assert!(accepted.grants_extra_paths());
    assert_eq!(accepted.extra_path_grants[0].path(), "/tmp");
    assert!(!accepted.extra_path_grants[0].is_read_only());
}

#[test]
fn a_manifest_field_of_the_wrong_shape_is_refused() {
    for refused in [
        json!([]),
        json!({"root": 1}),
        json!({"entries": []}),
        json!({"users": "agent"}),
        json!({"extra_path_grants": {"path": "/tmp"}}),
    ] {
        assert!(
            Manifest::parse(&ManifestRegistries::builtin(), &refused).is_err(),
            "{refused} must be refused"
        );
    }
}

// --- credential exposure acknowledgements -------------------------------------------------------

#[test]
fn a_mount_exposure_acknowledgement_is_recorded_per_authority() {
    // Agreeing that a bucket key is visible inside the container is not agreeing that a managed
    // identity is, so the two are acknowledged separately.
    let manifest = Manifest::new()
        .with_in_container_mount_credential_exposure_acknowledged(&["secrets"])
        .expect("valid path");

    assert!(
        manifest.acknowledges_in_container_mount_credential_exposure(
            "secrets",
            MountCredentialAuthority::MountScoped
        )
    );
    assert!(
        !manifest.acknowledges_in_container_mount_credential_exposure(
            "secrets",
            MountCredentialAuthority::Broad
        )
    );
    assert!(
        !manifest.acknowledges_in_container_mount_credential_exposure(
            "other",
            MountCredentialAuthority::MountScoped
        )
    );

    let broad = Manifest::new()
        .with_in_container_mount_broad_credential_exposure_acknowledged(&["identity"])
        .expect("valid path");
    assert!(broad.acknowledges_in_container_mount_credential_exposure(
        "identity",
        MountCredentialAuthority::Broad
    ));
}

#[test]
fn an_acknowledgement_matches_the_same_mount_written_either_way() {
    // Which form the caller happens to hold should not decide whether the acknowledgement counts.
    let relative = Manifest::new()
        .with_in_container_mount_credential_exposure_acknowledged(&["secrets"])
        .expect("valid path");
    assert!(
        relative.acknowledges_in_container_mount_credential_exposure(
            "/workspace/secrets",
            MountCredentialAuthority::MountScoped
        )
    );

    let absolute = Manifest::new()
        .with_in_container_mount_credential_exposure_acknowledged(&["/workspace/secrets"])
        .expect("valid path");
    assert!(
        absolute.acknowledges_in_container_mount_credential_exposure(
            "secrets",
            MountCredentialAuthority::MountScoped
        )
    );

    // A path outside the workspace has no relative form, and does not match one.
    let outside = Manifest::new()
        .with_in_container_mount_credential_exposure_acknowledged(&["/mnt/secrets"])
        .expect("valid path");
    assert!(outside.acknowledges_in_container_mount_credential_exposure(
        "/mnt/secrets",
        MountCredentialAuthority::MountScoped
    ));
    assert!(
        !outside.acknowledges_in_container_mount_credential_exposure(
            "secrets",
            MountCredentialAuthority::MountScoped
        )
    );
}

#[test]
fn an_acknowledgement_names_exact_paths() {
    let cases = [
        (vec![], MountExposureError::NoPaths),
        (vec!["/"], MountExposureError::RootPath),
        (vec!["/workspace"], MountExposureError::RootPath),
        (vec![""], MountExposureError::RootPath),
        (vec!["secrets\\keys"], MountExposureError::Separators),
        // A pattern would quietly widen as the manifest grew, which is the opposite of what an
        // explicit security decision is for.
        (vec!["secrets/*"], MountExposureError::Wildcard),
        (vec!["../secrets"], MountExposureError::ParentSegments),
    ];

    for (paths, expected) in cases {
        assert_eq!(
            Manifest::new()
                .with_in_container_mount_credential_exposure_acknowledged(&paths)
                .expect_err("refuse"),
            expected,
            "{paths:?}"
        );
    }
}

#[test]
fn an_acknowledgement_never_travels_with_the_manifest() {
    // It is a decision this application made about this process. Writing it out would let it reach
    // a host that never agreed to it.
    let manifest = Manifest::new()
        .with_in_container_mount_credential_exposure_acknowledged(&["secrets"])
        .expect("valid path");

    let rendered = serde_json::to_value(&manifest).expect("serializes");
    assert!(
        !rendered.to_string().contains("secrets"),
        "the acknowledgement leaked into the wire: {rendered}"
    );
    assert!(
        !parse(&rendered).acknowledges_in_container_mount_credential_exposure(
            "secrets",
            MountCredentialAuthority::MountScoped
        )
    );
}

#[test]
fn a_payload_cannot_grant_itself_an_acknowledgement() {
    // Every spelling is refused rather than one canonical name, because the point is to catch a
    // payload trying to set the policy at all.
    for key in [
        "mount_credential_exposure_policy",
        "_mount_credential_exposure_policy",
        "in_container_mount_credential_exposure_acknowledged_paths",
        "inContainerMountBroadCredentialExposureAcknowledgedPaths",
    ] {
        let payload = json!({key: ["secrets"]});
        let error = Manifest::parse(&ManifestRegistries::builtin(), &payload).expect_err("refuse");
        assert!(
            matches!(error, ManifestParseError::ExposurePolicyInInput { .. }),
            "{key} must be refused, got {error}"
        );
    }
}
