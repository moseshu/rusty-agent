//! `ra-core::sandbox::mount_security`: where mount credentials may go, and keeping them out of
//! durable state.
//!
//! Ported from the reference's `tests/sandbox/test_mount_security.py`. What is pinned here:
//! - the credential boundary: which credentials may reach a helper inside a model-controlled
//!   sandbox, only for the exact acknowledged path, and only for supported combinations
//! - custom mounts and strategies are refused before any of their configuration is read
//! - a persisted state carries no mount authority, and gets it back only from a trusted manifest
//!   whose credential-free topology matches exactly
//! - a persisted payload that fails to read is never quoted back
//!
//! The reference's tests about interpreter frames, exception graphs, descriptors and import order
//! have no Rust counterpart and are not ported; neither are those for hosted-backend strategies
//! (Modal, Daytona, Blaxel, ...) that are not ported themselves.

use async_trait::async_trait;
use ra_core::sandbox::{
    AzureBlobMount, BoxMount, CREDENTIALLESS_MOUNT_AUTHORITY_KEY, CreateRequest,
    DiscriminatedPayload, Entry, EntryContent, Environment, ErrorCode, FuseOptions, GcsMount,
    Manifest, ManifestRegistries, Mount, MountCredentialAuthority, MountExposureError,
    MountPattern, MountProvider, MountStrategy, MountpointOptions, R2Mount,
    REDACTED_MOUNT_AUTHORITY_KEY, RcloneOptions, S3FilesMount, S3FilesOptions, S3Mount,
    SandboxClient, SandboxError, SandboxResult, SandboxSession, SandboxSessionState, Snapshot,
    builtin_entry_registry, builtin_snapshot_registry, configured_authority_fields,
    sanitize_manifest_mount_authority, sanitize_raw_session_state_mount_authority,
    url_carries_inline_authority, validate_manifest_mount_credential_boundaries,
    validate_mount_activation_credential_boundary,
};
use serde_json::{Value, json};

// --- fixtures ------------------------------------------------------------------------------------

fn rclone() -> MountStrategy {
    MountStrategy::in_container(MountPattern::Rclone(RcloneOptions::default()))
}

fn rclone_with(options: RcloneOptions) -> MountStrategy {
    MountStrategy::in_container(MountPattern::Rclone(options))
}

fn mountpoint(endpoint_url: Option<&str>) -> MountStrategy {
    MountStrategy::in_container(MountPattern::Mountpoint(MountpointOptions {
        endpoint_url: endpoint_url.map(str::to_owned),
        ..MountpointOptions::default()
    }))
}

fn fuse() -> MountStrategy {
    MountStrategy::in_container(MountPattern::Fuse(FuseOptions::default()))
}

fn s3files(extra_options: &[(&str, &str)]) -> MountStrategy {
    MountStrategy::in_container(MountPattern::S3Files(S3FilesOptions {
        extra_options: extra_options
            .iter()
            .map(|(key, value)| ((*key).to_owned(), Some((*value).to_owned())))
            .collect(),
        ..S3FilesOptions::default()
    }))
}

fn docker() -> MountStrategy {
    MountStrategy::docker_volume("rclone")
}

fn docker_with(options: &[(&str, &str)]) -> MountStrategy {
    MountStrategy::DockerVolume {
        driver: "rclone".to_owned(),
        driver_options: options
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect(),
    }
}

fn s3_bucket() -> S3Mount {
    S3Mount {
        bucket: "example-bucket".to_owned(),
        ..S3Mount::default()
    }
}

fn s3(credentialed: bool) -> MountProvider {
    MountProvider::S3(S3Mount {
        access_key_id: credentialed.then(|| "example-access-key".to_owned()),
        secret_access_key: credentialed.then(|| "example-secret-key".to_owned()),
        ..s3_bucket()
    })
}

fn gcs(configure: impl FnOnce(&mut GcsMount)) -> MountProvider {
    let mut mount = GcsMount {
        bucket: "bucket".to_owned(),
        ..GcsMount::default()
    };
    configure(&mut mount);
    MountProvider::Gcs(mount)
}

fn azure(configure: impl FnOnce(&mut AzureBlobMount)) -> MountProvider {
    let mut mount = AzureBlobMount {
        account: "example".to_owned(),
        container: "private".to_owned(),
        ..AzureBlobMount::default()
    };
    configure(&mut mount);
    MountProvider::AzureBlob(mount)
}

fn boxed(configure: impl FnOnce(&mut BoxMount)) -> MountProvider {
    let mut mount = BoxMount::default();
    configure(&mut mount);
    MountProvider::Box(mount)
}

fn r2(configure: impl FnOnce(&mut R2Mount)) -> MountProvider {
    let mut mount = R2Mount {
        bucket: "bucket".to_owned(),
        account_id: "example-account".to_owned(),
        ..R2Mount::default()
    };
    configure(&mut mount);
    MountProvider::R2(mount)
}

fn s3_files() -> MountProvider {
    MountProvider::S3Files(S3FilesMount {
        file_system_id: "fs-123".to_owned(),
        ..S3FilesMount::default()
    })
}

fn mount(provider: MountProvider, strategy: MountStrategy) -> Mount {
    Mount::new(provider, strategy).expect("a supported combination")
}

fn entry(provider: MountProvider, strategy: MountStrategy) -> Entry {
    Entry::mount(mount(provider, strategy))
}

fn manifest(entries: Vec<(&str, Entry)>) -> Manifest {
    entries
        .into_iter()
        .fold(Manifest::new(), |manifest, (path, entry)| {
            manifest.with_entry(path, entry)
        })
}

fn scoped(manifest: Manifest, path: &str) -> Manifest {
    manifest
        .with_in_container_mount_credential_exposure_acknowledged(&[path])
        .expect("acknowledged")
}

fn broad(manifest: Manifest, path: &str) -> Manifest {
    manifest
        .with_in_container_mount_broad_credential_exposure_acknowledged(&[path])
        .expect("acknowledged")
}

fn validate(manifest: &Manifest) -> Result<(), SandboxError> {
    validate_manifest_mount_credential_boundaries(manifest, None)
}

fn validate_on(manifest: &Manifest, backend: &str) -> Result<(), SandboxError> {
    validate_manifest_mount_credential_boundaries(manifest, Some(backend))
}

#[track_caller]
fn refused(result: Result<(), SandboxError>, fragment: &str) -> SandboxError {
    let error = result.expect_err("refuse");
    assert_eq!(error.error_code(), ErrorCode::MountConfigInvalid, "{error}");
    assert!(
        error.message().contains(fragment),
        "expected {fragment:?} in {:?}",
        error.message()
    );
    error
}

fn context_names(error: &SandboxError, key: &str) -> Vec<String> {
    error
        .context()
        .get(key)
        .and_then(Value::as_array)
        .map(|names| {
            names
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

#[track_caller]
fn assert_absent(haystack: &str, needle: &str) {
    assert!(
        !haystack.contains(needle),
        "{needle:?} leaked into {haystack}"
    );
}

/// A client for one backend, used only for its state round trip.
struct Client(&'static str);

#[async_trait]
impl SandboxClient for Client {
    fn backend_id(&self) -> &str {
        self.0
    }

    async fn create(&self, _request: CreateRequest) -> SandboxResult<Box<dyn SandboxSession>> {
        panic!("create is not used by these tests")
    }

    async fn resume(&self, _state: SandboxSessionState) -> SandboxResult<Box<dyn SandboxSession>> {
        panic!("resume is not used by these tests")
    }

    async fn delete(&self, _session: &dyn SandboxSession) -> SandboxResult<()> {
        panic!("delete is not used by these tests")
    }
}

const DOCKER: Client = Client("docker");

fn state(manifest: Manifest) -> SandboxSessionState {
    SandboxSessionState::new("docker", Snapshot::noop(), manifest)
}

fn round_trip(manifest: Manifest) -> (Value, SandboxSessionState) {
    let payload = DOCKER
        .serialize_session_state(&state(manifest))
        .expect("serialize");
    let restored = deserialize(payload.clone()).expect("deserialize");
    (payload, restored)
}

fn deserialize(payload: Value) -> SandboxResult<SandboxSessionState> {
    DOCKER.deserialize_session_state(
        payload,
        &builtin_snapshot_registry(),
        &ManifestRegistries::builtin(),
    )
}

fn raw_state(manifest: &Manifest) -> Value {
    json!({
        "type": "docker",
        "manifest": serde_json::to_value(manifest).expect("render"),
        "snapshot": Value::from(Snapshot::noop()),
    })
}

fn sanitize_state(payload: &Value) -> Result<(Value, bool), ra_core::sandbox::InvalidRawManifest> {
    sanitize_raw_session_state_mount_authority(payload, &builtin_entry_registry())
}

// --- the boundary ----------------------------------------------------------------------------

#[test]
fn rejects_explicit_credentials_for_in_container_mounts() {
    let manifest = manifest(vec![("data", entry(s3(true), rclone()))]);

    let error = refused(validate(&manifest), "mount-scoped credentials");

    assert_eq!(
        context_names(&error, "credential_fields"),
        ["access_key_id", "secret_access_key"]
    );
    assert_absent(&error.to_string(), "example-secret-key");
    assert_absent(&format!("{:?}", error.context()), "example-secret-key");
}

#[test]
fn exact_path_acknowledgement_allows_supported_mount_scoped_credentials() {
    let acknowledged = scoped(manifest(vec![("data", entry(s3(true), rclone()))]), "data");
    validate(&acknowledged).expect("acknowledged path");

    // The acknowledgement names a path, not a mount: the same mount somewhere else is not covered.
    let sibling = scoped(manifest(vec![("other", entry(s3(true), rclone()))]), "data");
    refused(validate(&sibling), "mount-scoped credentials");

    let data = mount(s3(true), rclone());
    validate_mount_activation_credential_boundary(
        &data,
        data.strategy(),
        Some(&acknowledged),
        Some("/workspace/data"),
        Some("docker"),
    )
    .expect("activation at the acknowledged path");
    refused(
        validate_mount_activation_credential_boundary(
            &data,
            data.strategy(),
            Some(&acknowledged),
            Some("/workspace/other"),
            Some("docker"),
        ),
        "mount-scoped credentials",
    );
}

#[test]
fn acknowledgement_rejects_incomplete_in_container_s3_credentials() {
    let cases: [(&[(&str, &str)], &[&str]); 6] = [
        (&[("access_key_id", "access-key")], &["secret_access_key"]),
        (&[("secret_access_key", "secret-key")], &["access_key_id"]),
        (
            &[("session_token", "session-token")],
            &["access_key_id", "secret_access_key"],
        ),
        (
            &[("access_key_id", "access-key"), ("secret_access_key", "")],
            &["secret_access_key"],
        ),
        (
            &[("access_key_id", " "), ("secret_access_key", "secret-key")],
            &["access_key_id"],
        ),
        (
            &[
                ("access_key_id", "access-key"),
                ("secret_access_key", "secret-key"),
                ("session_token", " "),
            ],
            &["session_token"],
        ),
    ];
    for (credentials, invalid) in cases {
        let mut provider = s3_bucket();
        for (field, value) in credentials {
            let value = Some((*value).to_owned());
            match *field {
                "access_key_id" => provider.access_key_id = value,
                "secret_access_key" => provider.secret_access_key = value,
                _ => provider.session_token = value,
            }
        }
        let acknowledged = scoped(
            manifest(vec![("data", entry(MountProvider::S3(provider), rclone()))]),
            "data",
        );

        let error = refused(validate(&acknowledged), "complete non-empty credential set");
        assert_eq!(context_names(&error, "credential_fields"), invalid);
    }
}

#[test]
fn acknowledgement_rejects_incomplete_in_container_gcs_hmac_credentials() {
    let cases: [(fn(&mut GcsMount), &[&str]); 5] = [
        (
            |gcs| gcs.access_id = Some("access-id".to_owned()),
            &["secret_access_key"],
        ),
        (
            |gcs| gcs.secret_access_key = Some("secret-key".to_owned()),
            &["access_id"],
        ),
        (
            |gcs| {
                gcs.access_id = Some("access-id".to_owned());
                gcs.secret_access_key = Some(String::new());
            },
            &["secret_access_key"],
        ),
        (
            |gcs| {
                gcs.access_id = Some(" ".to_owned());
                gcs.secret_access_key = Some("secret-key".to_owned());
            },
            &["access_id"],
        ),
        (
            |gcs| {
                gcs.access_id = Some("access-id".to_owned());
                gcs.service_account_credentials = Some(r#"{"type":"service_account"}"#.to_owned());
            },
            &["secret_access_key"],
        ),
    ];
    for (configure, invalid) in cases {
        let acknowledged = scoped(
            manifest(vec![("data", entry(gcs(configure), rclone()))]),
            "data",
        );
        let error = refused(validate(&acknowledged), "complete non-empty credential set");
        assert_eq!(context_names(&error, "credential_fields"), invalid);
    }

    let complete = scoped(
        manifest(vec![(
            "data",
            entry(
                gcs(|gcs| {
                    gcs.access_id = Some("access-id".to_owned());
                    gcs.secret_access_key = Some("secret-key".to_owned());
                }),
                rclone(),
            ),
        )]),
        "data",
    );
    validate(&complete).expect("a complete HMAC pair");
}

#[test]
fn acknowledgement_rejects_empty_in_container_scalar_authority() {
    type Case = (fn(String) -> MountProvider, bool, &'static str);
    let cases: [Case; 5] = [
        (
            |value| gcs(|gcs| gcs.access_token = Some(value)),
            false,
            "access_token",
        ),
        (
            |value| gcs(|gcs| gcs.service_account_credentials = Some(value)),
            false,
            "service_account_credentials",
        ),
        (
            |value| gcs(|gcs| gcs.service_account_file = Some(value)),
            true,
            "service_account_file",
        ),
        (
            |value| azure(|azure| azure.account_key = Some(value)),
            false,
            "account_key",
        ),
        (
            |value| azure(|azure| azure.identity_client_id = Some(value)),
            true,
            "identity_client_id",
        ),
    ];
    for blank in ["", "   "] {
        for (provider, is_broad, field) in cases {
            let declared = manifest(vec![("data", entry(provider(blank.to_owned()), rclone()))]);
            let acknowledged = if is_broad {
                broad(declared, "data")
            } else {
                scoped(declared, "data")
            };
            let error = refused(
                validate(&acknowledged),
                "must not be empty or whitespace-only",
            );
            assert_eq!(context_names(&error, "credential_fields"), [field]);
        }
    }
}

#[test]
fn acknowledgement_rejects_incomplete_in_container_r2_credentials() {
    let cases: [(fn(&mut R2Mount), &[&str]); 3] = [
        (
            |r2| r2.access_key_id = Some("access-key".to_owned()),
            &["secret_access_key"],
        ),
        (
            |r2| r2.secret_access_key = Some("secret-key".to_owned()),
            &["access_key_id"],
        ),
        (
            |r2| {
                r2.access_key_id = Some("access-key".to_owned());
                r2.secret_access_key = Some(String::new());
            },
            &["secret_access_key"],
        ),
    ];
    for (configure, invalid) in cases {
        let acknowledged = scoped(
            manifest(vec![("data", entry(r2(configure), rclone()))]),
            "data",
        );
        let error = refused(validate(&acknowledged), "complete non-empty credential set");
        assert_eq!(context_names(&error, "credential_fields"), invalid);
    }
}

#[test]
fn incomplete_credentials_remain_external_provider_configuration() {
    // Outside the sandbox, what the provider makes of an incomplete set is the provider's business.
    let providers = [
        MountProvider::S3(S3Mount {
            access_key_id: Some("access-key".to_owned()),
            ..s3_bucket()
        }),
        gcs(|gcs| gcs.access_id = Some("access-id".to_owned())),
        r2(|r2| r2.access_key_id = Some("access-key".to_owned())),
        gcs(|gcs| gcs.access_token = Some(String::new())),
        azure(|azure| azure.identity_client_id = Some(" ".to_owned())),
    ];
    for provider in providers {
        let manifest = manifest(vec![("data", entry(provider, docker()))]);
        validate_on(&manifest, "docker").expect("external strategy");
    }
}

#[test]
fn mount_credential_acknowledgement_is_not_a_path_prefix() {
    let declared = manifest(vec![(
        "parent",
        Entry::dir().with_child("data", entry(s3(true), rclone())),
    )]);

    refused(
        validate(&scoped(declared.clone(), "parent")),
        "mount-scoped credentials",
    );
    validate(&scoped(declared, "parent/data")).expect("the exact path");
}

#[test]
fn mount_credential_acknowledgement_preserves_path_whitespace() {
    let declared = manifest(vec![("data ", entry(s3(true), rclone()))]);

    refused(
        validate(&scoped(declared.clone(), "data")),
        "mount-scoped credentials",
    );
    validate(&scoped(declared, "data ")).expect("the exact path");
}

#[test]
fn ignores_environment_values_already_exposed_to_the_sandbox() {
    let mut declared = manifest(vec![("data", entry(s3(false), mountpoint(None)))]);
    declared.environment = Environment::new()
        .with("AWS_SECRET_ACCESS_KEY", "secret")
        .with("GITHUB_TOKEN", "unrelated");

    validate(&declared).expect("the environment is not mount authority");
}

// --- custom mounts and strategies --------------------------------------------------------------

fn custom_mount() -> Mount {
    Mount::new(
        MountProvider::Extension(
            DiscriminatedPayload::new("direct_custom_mount")
                .with_field("bucket", "bucket")
                .with_field("api_token", "custom-mount-secret"),
        ),
        rclone(),
    )
    .expect("an extension is not checked against a matrix")
}

fn custom_strategy() -> MountStrategy {
    MountStrategy::Extension(
        DiscriminatedPayload::new("custom_pattern_strategy")
            .with_field("pattern", json!({"type": "custom_pattern"}))
            .with_field("api_token", "custom-strategy-secret"),
    )
}

#[test]
fn custom_mounts_and_strategies_are_rejected_at_the_credential_boundary() {
    let with_mount = manifest(vec![("data", Entry::mount(custom_mount()))]);
    let error = refused(validate(&with_mount), "custom mount implementations");
    assert_absent(&format!("{error:?}"), "custom-mount-secret");

    let with_strategy = manifest(vec![("data", entry(s3(false), custom_strategy()))]);
    let error = refused(validate(&with_strategy), "custom mount strategies");
    assert_absent(&format!("{error:?}"), "custom-strategy-secret");

    // Activation checks the strategy about to run, not only the declared one.
    let declared = mount(s3(false), rclone());
    refused(
        validate_mount_activation_credential_boundary(
            &declared,
            &custom_strategy(),
            None,
            None,
            None,
        ),
        "custom mount strategies",
    );
}

#[test]
fn an_entry_that_only_names_a_builtin_mount_type_is_a_custom_mount() {
    // The counterpart of a subclass of a canonical mount: something claiming to be an S3 mount
    // without being one would have its fields read by rules written for a shape it does not have.
    let forged = manifest(vec![(
        "data",
        Entry::new(EntryContent::Extension(
            DiscriminatedPayload::new("s3_mount").with_field("secret_access_key", "forged-secret"),
        )),
    )]);

    let error = refused(validate(&forged), "custom mount implementations");
    assert_absent(&format!("{error:?}"), "forged-secret");
    let error = refused(
        sanitize_manifest_mount_authority(&forged).map(|_| ()),
        "custom mount implementations",
    );
    assert_absent(&format!("{error:?}"), "forged-secret");
}

#[test]
fn an_acknowledgement_is_refused_for_a_manifest_with_custom_mounts() {
    let with_mount = manifest(vec![("data", Entry::mount(custom_mount()))]);
    assert_eq!(
        with_mount
            .clone()
            .with_in_container_mount_credential_exposure_acknowledged(&["data"])
            .expect_err("refuse"),
        MountExposureError::CustomMount
    );
    assert_eq!(
        with_mount
            .with_in_container_mount_broad_credential_exposure_acknowledged(&["data"])
            .expect_err("refuse")
            .to_string(),
        "custom mount implementations are not supported at the sandbox credential boundary"
    );

    let with_strategy = manifest(vec![("data", entry(s3(true), custom_strategy()))]);
    assert_eq!(
        with_strategy
            .with_in_container_mount_credential_exposure_acknowledged(&["data"])
            .expect_err("refuse"),
        MountExposureError::CustomStrategy
    );
}

#[test]
fn a_strategy_owned_by_another_backend_is_refused() {
    let declared = manifest(vec![("data", entry(s3(false), docker()))]);

    let error = refused(
        validate_on(&declared, "unix_local"),
        "docker-volume mounts are not supported by this sandbox backend",
    );
    assert_eq!(
        error.context().get("sandbox_backend"),
        Some(&json!("unix_local"))
    );
    validate_on(&declared, "docker").expect("its own backend");
    // A caller that does not know the backend yet skips only this check.
    validate(&declared).expect("no backend named");
}

// --- in-container capabilities -----------------------------------------------------------------

#[test]
fn blobfuse_mounts_require_broad_acknowledgement() {
    let declared = manifest(vec![("data", entry(azure(|_| {}), fuse()))]);

    refused(validate(&declared), "broad credential authority");
    validate(&broad(declared, "data")).expect("broad acknowledgement");
}

#[test]
fn blobfuse_account_key_requires_mount_scoped_and_broad_acknowledgement() {
    let declared = manifest(vec![(
        "data",
        entry(
            azure(|azure| azure.account_key = Some("account-key".to_owned())),
            fuse(),
        ),
    )]);

    let mount_scoped = scoped(declared.clone(), "data");
    refused(validate(&mount_scoped), "broad credential authority");
    refused(
        validate(&broad(declared, "data")),
        "mount-scoped credentials",
    );
    validate(&broad(mount_scoped, "data")).expect("both acknowledgements");
}

#[test]
fn s3_files_require_broad_acknowledgement_before_ambient_iam_can_be_used() {
    let provider = MountProvider::S3Files(S3FilesMount {
        file_system_id: "fs-123".to_owned(),
        extra_options: [("tlsport".to_owned(), Some("4049".to_owned()))]
            .into_iter()
            .collect(),
        ..S3FilesMount::default()
    });
    let declared = manifest(vec![("data", entry(provider, s3files(&[])))]);

    refused(validate(&declared), "broad credential authority");
    validate(&broad(declared, "data")).expect("broad acknowledgement");
}

#[test]
fn rejects_rclone_credential_source_overrides() {
    let cases: [&[&str]; 5] = [
        &["--config=/workspace/credentials.conf"],
        &["--s3-env-auth=true"],
        &["--s3-profile=production"],
        &["--azureblob-use-msi=true"],
        &["--header", "Authorization: Bearer secret"],
    ];
    for extra_args in cases {
        let strategy = rclone_with(RcloneOptions {
            extra_args: extra_args.iter().map(|arg| (*arg).to_owned()).collect(),
            ..RcloneOptions::default()
        });
        let declared = manifest(vec![("data", entry(s3(false), strategy))]);
        refused(validate(&declared), "does not support exposing");
    }
}

#[test]
fn preserves_supported_credentialless_rclone_extra_args() {
    let strategy = rclone_with(RcloneOptions {
        extra_args: [
            "--allow-other",
            "--uid",
            "123",
            "--gid=456",
            "--buffer-size",
            "0",
        ]
        .map(str::to_owned)
        .to_vec(),
        ..RcloneOptions::default()
    });

    validate(&manifest(vec![("data", entry(s3(false), strategy))])).expect("known-safe flags");
}

#[test]
fn box_mounts_with_direct_credentials_require_exact_acknowledgement() {
    let declared = manifest(vec![(
        "data",
        entry(
            boxed(|mount| mount.access_token = Some("box-access-token".to_owned())),
            rclone(),
        ),
    )]);

    refused(validate(&declared), "mount-scoped credentials");
    validate(&scoped(declared, "data")).expect("acknowledged");
}

#[test]
fn box_config_file_requires_broad_acknowledgement() {
    let declared = manifest(vec![(
        "data",
        entry(
            boxed(|mount| mount.box_config_file = Some("/run/secrets/box.json".to_owned())),
            rclone(),
        ),
    )]);

    refused(validate(&declared), "broad credential authority");
    refused(
        validate(&scoped(declared.clone(), "data")),
        "broad credential authority",
    );
    validate(&broad(declared, "data")).expect("broad acknowledgement");
}

#[test]
fn box_in_container_mount_requires_non_interactive_authentication() {
    let providers = [
        boxed(|_| {}),
        boxed(|mount| mount.client_id = Some("client-id".to_owned())),
        boxed(|mount| mount.client_secret = Some("client-secret".to_owned())),
    ];
    for provider in providers {
        let declared = manifest(vec![("data", entry(provider, rclone()))]);
        refused(validate(&declared), "non-interactive authentication source");
    }
}

#[test]
fn box_in_container_mount_rejects_empty_authentication_sources() {
    for blank in ["", "   "] {
        let cases: [(MountProvider, bool); 4] = [
            (
                boxed(|mount| mount.access_token = Some(blank.to_owned())),
                false,
            ),
            (boxed(|mount| mount.token = Some(blank.to_owned())), false),
            (
                boxed(|mount| mount.config_credentials = Some(blank.to_owned())),
                false,
            ),
            (
                boxed(|mount| mount.box_config_file = Some(blank.to_owned())),
                true,
            ),
        ];
        for (provider, is_broad) in cases {
            let declared = manifest(vec![("data", entry(provider, rclone()))]);
            let acknowledged = if is_broad {
                broad(declared, "data")
            } else {
                scoped(declared, "data")
            };
            refused(
                validate(&acknowledged),
                "authentication values must not be empty",
            );
        }
    }
}

#[test]
fn box_in_container_mount_rejects_mixed_usable_and_empty_authentication_sources() {
    for blank in ["", "   "] {
        let with_blank_file = scoped(
            manifest(vec![(
                "data",
                entry(
                    boxed(|mount| {
                        mount.access_token = Some("box-access-token".to_owned());
                        mount.box_config_file = Some(blank.to_owned());
                    }),
                    rclone(),
                ),
            )]),
            "data",
        );
        let error = refused(
            validate(&with_blank_file),
            "authentication values must not be empty",
        );
        assert_eq!(
            context_names(&error, "credential_fields"),
            ["box_config_file"]
        );

        let with_blank_token = broad(
            manifest(vec![(
                "data",
                entry(
                    boxed(|mount| {
                        mount.access_token = Some(blank.to_owned());
                        mount.box_config_file = Some("/run/secrets/box.json".to_owned());
                    }),
                    rclone(),
                ),
            )]),
            "data",
        );
        let error = refused(
            validate(&with_blank_token),
            "authentication values must not be empty",
        );
        assert_eq!(context_names(&error, "credential_fields"), ["access_token"]);
    }
}

#[test]
fn external_strategies_keep_their_credentials_as_configured() {
    let with_token = manifest(vec![(
        "data",
        entry(
            boxed(|mount| mount.access_token = Some("box-access-token".to_owned())),
            docker(),
        ),
    )]);
    validate_on(&with_token, "docker").expect("box through a volume driver");

    let multiline = manifest(vec![(
        "data",
        entry(
            gcs(|gcs| {
                gcs.service_account_credentials =
                    Some("{\"private_key\":\"line-1\nline-2\"}".to_owned());
            }),
            docker(),
        ),
    )]);
    validate_on(&multiline, "docker").expect("a line break outside rclone configuration");
}

// --- configuration injection -------------------------------------------------------------------

#[test]
fn rejects_and_redacts_rclone_config_line_injection() {
    let cases: [(MountProvider, &str); 3] = [
        (
            MountProvider::S3(S3Mount {
                s3_provider: "AWS\naccess_key_id = injected-value".to_owned(),
                ..s3_bucket()
            }),
            "s3_provider",
        ),
        (
            azure(|azure| azure.account = "account\nkey = injected-value".to_owned()),
            "account",
        ),
        (
            r2(|r2| r2.account_id = "account\nsecret_access_key = injected-value".to_owned()),
            "account_id",
        ),
    ];
    for (provider, field) in cases {
        let declared = manifest(vec![("data", entry(provider, rclone()))]);

        let error = refused(validate(&declared), "must not contain line breaks");
        assert_eq!(context_names(&error, "configuration_fields"), [field]);
        assert_absent(&error.to_string(), "injected-value");

        let (durable, redacted) = sanitize_manifest_mount_authority(&declared).expect("sanitize");
        assert!(redacted);
        assert_eq!(durable["entries"]["data"][field], json!(""));
        assert_absent(&durable.to_string(), "injected-value");
    }
}

#[test]
fn rejects_rclone_on_the_fly_remote_name() {
    let sentinel = "remote-name-secret";
    let strategy = rclone_with(RcloneOptions {
        remote_name: Some(format!(
            ":s3,access_key_id=access,secret_access_key={sentinel}"
        )),
        ..RcloneOptions::default()
    });

    let error = refused(
        validate(&manifest(vec![("data", entry(s3(false), strategy))])),
        "does not support exposing",
    );
    assert_absent(&format!("{error:?}"), sentinel);
}

#[test]
fn serialization_redacts_rclone_on_the_fly_remote_name() {
    let sentinel = "serialized-remote-name-secret";
    let strategy = rclone_with(RcloneOptions {
        remote_name: Some(format!(":s3,secret_access_key={sentinel}")),
        ..RcloneOptions::default()
    });

    let payload = DOCKER
        .serialize_session_state(&state(manifest(vec![("data", entry(s3(false), strategy))])))
        .expect("serialize");

    assert_eq!(
        payload["manifest"]["entries"]["data"]["mount_strategy"]["pattern"]["remote_name"],
        Value::Null
    );
    assert_eq!(payload[REDACTED_MOUNT_AUTHORITY_KEY], json!(true));
    assert_absent(&payload.to_string(), sentinel);
}

#[test]
fn preserves_ordinary_rclone_remote_name() {
    let strategy = rclone_with(RcloneOptions {
        remote_name: Some("public bucket-1".to_owned()),
        ..RcloneOptions::default()
    });
    let declared = manifest(vec![("data", entry(s3(false), strategy))]);

    validate(&declared).expect("an ordinary name");
    let (sanitized, redacted) = sanitize_state(&raw_state(&declared)).expect("sanitize");
    assert!(!redacted);
    assert_eq!(
        sanitized["manifest"]["entries"]["data"]["mount_strategy"]["pattern"]["remote_name"],
        json!("public bucket-1")
    );
}

#[test]
fn rejects_malformed_inline_credential_url_without_mutating_the_manifest() {
    for endpoint_url in [
        "https://user:malformed-secret@[invalid",
        "https:user:malformed-secret@example.test",
    ] {
        let provider = MountProvider::S3(S3Mount {
            endpoint_url: Some(endpoint_url.to_owned()),
            ..s3_bucket()
        });
        let declared = manifest(vec![("data", entry(provider, rclone()))]);

        refused(validate(&declared), "does not support exposing");
        let EntryContent::Mount(mount) = declared.entries["data"].content() else {
            panic!("a mount");
        };
        let MountProvider::S3(s3) = mount.provider() else {
            panic!("an S3 mount");
        };
        assert_eq!(s3.endpoint_url.as_deref(), Some(endpoint_url));
    }
}

#[test]
fn rejects_mountpoint_endpoint_authority() {
    for endpoint_url in [
        "https://user:pattern-secret@example.test",
        "https://example.test?signature=pattern-secret",
    ] {
        let declared = manifest(vec![(
            "data",
            entry(s3(false), mountpoint(Some(endpoint_url))),
        )]);

        let error = refused(validate(&declared), "does not support exposing");
        assert_eq!(
            context_names(&error, "credential_fields"),
            ["mount_strategy.pattern.options.endpoint_url"]
        );
        assert_absent(&error.to_string(), "pattern-secret");
    }
}

#[test]
fn a_url_is_read_the_way_urlsplit_reads_it() {
    assert!(url_carries_inline_authority(
        "https://example.test?signature=x"
    ));
    assert!(url_carries_inline_authority("https://[invalid"));
    assert!(url_carries_inline_authority(
        "https://[not-an-address]/path"
    ));
    // A `?` inside the fragment is not a query, and an empty query carries nothing.
    assert!(!url_carries_inline_authority(
        "https://example.test/path#frag?x"
    ));
    assert!(!url_carries_inline_authority("https://example.test/?"));
    assert!(!url_carries_inline_authority("https://[::1]:9000/bucket"));
}

#[test]
fn strategy_and_pattern_authority_is_named_by_its_path_from_the_mount() {
    let with_driver_options = mount(s3(false), docker_with(&[("password", "secret")]));
    assert!(
        configured_authority_fields(&with_driver_options).contains("mount_strategy.driver_options")
    );

    let with_config_file = mount(
        s3(false),
        rclone_with(RcloneOptions {
            config_file_path: Some("/run/rclone.conf".to_owned()),
            ..RcloneOptions::default()
        }),
    );
    assert!(
        configured_authority_fields(&with_config_file)
            .contains("mount_strategy.pattern.config_file_path")
    );

    let with_pattern_options = mount(s3_files(), s3files(&[("tlsport", "4049")]));
    assert!(
        configured_authority_fields(&with_pattern_options)
            .contains("mount_strategy.pattern.options.extra_options")
    );
}

// --- credential files --------------------------------------------------------------------------

#[test]
fn rejects_manifest_backed_credential_files() {
    let cases = [
        gcs(|gcs| gcs.service_account_file = Some("/workspace/credentials.json".to_owned())),
        boxed(|mount| mount.box_config_file = Some("credentials.json".to_owned())),
    ];
    for provider in cases {
        let declared = manifest(vec![
            ("credentials.json", Entry::file("credential-file-secret")),
            ("data", entry(provider, docker())),
        ]);
        refused(
            validate_on(&declared, "docker"),
            "credential files stored in the manifest",
        );
    }
}

#[test]
fn broad_acknowledgement_does_not_allow_manifest_backed_rclone_config() {
    let strategy = rclone_with(RcloneOptions {
        config_file_path: Some("credentials.conf".to_owned()),
        ..RcloneOptions::default()
    });
    let declared = broad(
        manifest(vec![
            ("credentials.conf", Entry::file("credential-file-secret")),
            ("data", entry(s3(false), strategy)),
        ]),
        "data",
    );

    refused(
        validate(&declared),
        "credential files stored in the manifest",
    );
}

#[test]
fn rejects_credential_files_from_manifest_materialization_sources() {
    let cases = [
        (
            "/workspace/credentials.json",
            "credentials.json",
            Entry::local_file("credentials.json"),
        ),
        (
            "/workspace/imported/credentials.json",
            "imported",
            Entry::local_dir(Some("imported".to_owned())),
        ),
        (
            "/workspace/repository/credentials.json",
            "repository",
            Entry::git_repo("example/repository", "main"),
        ),
        (
            "/workspace/secrets/credentials.json",
            "secrets",
            entry(
                MountProvider::S3(S3Mount {
                    bucket: "secret-bucket".to_owned(),
                    ..S3Mount::default()
                }),
                docker(),
            ),
        ),
        (
            "/workspace/credentials.json",
            "credentials.json",
            Entry::new(EntryContent::Extension(
                DiscriminatedPayload::new("custom_token_entry")
                    .with_field("token", "custom-source"),
            )),
        ),
    ];
    for (credential_path, source_path, source) in cases {
        let declared = manifest(vec![
            (source_path, source),
            (
                "data",
                entry(
                    gcs(|gcs| gcs.service_account_file = Some(credential_path.to_owned())),
                    docker(),
                ),
            ),
        ]);
        refused(
            validate_on(&declared, "docker"),
            "credential files stored in the manifest",
        );
    }
}

// --- durable state -----------------------------------------------------------------------------

#[test]
fn session_state_serialization_redacts_complete_opaque_authority_fields() {
    let declared = manifest(vec![(
        "data",
        entry(
            s3(true),
            docker_with(&[
                ("vfs-cache-mode", "off"),
                ("s3-secret-access-key", "driver-secret"),
                ("s3-env-auth", "true"),
                ("config", "/host/rclone.conf"),
            ]),
        ),
    )]);

    let (payload, restored) = round_trip(declared.clone());
    let serialized = &payload["manifest"]["entries"]["data"];
    assert_eq!(payload[REDACTED_MOUNT_AUTHORITY_KEY], json!(true));
    assert_eq!(serialized["access_key_id"], Value::Null);
    assert_eq!(serialized["secret_access_key"], Value::Null);
    assert_eq!(serialized["mount_strategy"]["driver_options"], json!({}));
    assert_absent(&payload.to_string(), "example-secret-key");
    assert_absent(&payload.to_string(), "driver-secret");
    assert!(restored.mount_authority_redacted());

    let rebound = restored
        .rebind_persisted_mount_authority(Some(&declared), "docker")
        .expect("rebind");
    let EntryContent::Mount(mount) = rebound.manifest().entries["data"].content() else {
        panic!("a mount");
    };
    let MountProvider::S3(provider) = mount.provider() else {
        panic!("an S3 mount");
    };
    assert_eq!(
        provider.access_key_id.as_deref(),
        Some("example-access-key")
    );
    assert_eq!(
        provider.secret_access_key.as_deref(),
        Some("example-secret-key")
    );
    assert!(!rebound.mount_authority_redacted());
    assert!(rebound.mount_authority_rebound());
    validate_on(rebound.manifest(), "docker").expect("rebound");
}

#[test]
fn session_state_round_trip_preserves_credentialless_external_mount() {
    let declared = manifest(vec![("data", entry(s3(false), docker()))]);

    let (payload, restored) = round_trip(declared.clone());

    assert!(payload.get(CREDENTIALLESS_MOUNT_AUTHORITY_KEY).is_none());
    assert!(payload.get(REDACTED_MOUNT_AUTHORITY_KEY).is_none());
    assert_eq!(restored.manifest(), &declared);
    assert!(!restored.mount_authority_redacted());
    assert!(!restored.mount_authority_rebound());
}

#[test]
fn credentialless_marker_does_not_override_configured_mount_authority() {
    let sentinel = "configured-secret-access-key";
    let provider = MountProvider::S3(S3Mount {
        access_key_id: Some("access-key".to_owned()),
        secret_access_key: Some(sentinel.to_owned()),
        ..s3_bucket()
    });
    let mut payload = raw_state(&manifest(vec![("data", entry(provider, docker()))]));
    payload[CREDENTIALLESS_MOUNT_AUTHORITY_KEY] = json!(true);

    let restored = deserialize(payload).expect("deserialize");

    assert!(restored.mount_authority_redacted());
    assert_absent(&format!("{restored:?}"), sentinel);
}

#[test]
fn session_state_serialization_preserves_custom_non_mount_fields() {
    let mut registries = ManifestRegistries::builtin();
    registries
        .entries_mut()
        .register("custom_token_entry", "tests")
        .expect("register");
    let declared = manifest(vec![(
        "custom",
        Entry::new(EntryContent::Extension(
            DiscriminatedPayload::new("custom_token_entry")
                .with_field("token", "ordinary-token-value"),
        )),
    )]);

    let payload = DOCKER
        .serialize_session_state(&state(declared.clone()))
        .expect("serialize");
    let restored = DOCKER
        .deserialize_session_state(payload.clone(), &builtin_snapshot_registry(), &registries)
        .expect("deserialize");

    assert!(payload.get(REDACTED_MOUNT_AUTHORITY_KEY).is_none());
    assert_eq!(
        payload["manifest"]["entries"]["custom"]["token"],
        json!("ordinary-token-value")
    );
    assert_eq!(restored.manifest(), &declared);
}

#[test]
fn session_state_serialization_preserves_custom_non_dir_children() {
    // A registered type's `children` is its own field, not nested entries, however much it looks
    // like a mount.
    let mut registries = ManifestRegistries::builtin();
    registries
        .entries_mut()
        .register("custom_children_entry", "tests")
        .expect("register");
    for children in [
        json!("ordinary-metadata"),
        json!({"nested": {
            "type": "s3_mount",
            "access_key_id": "ordinary-access-metadata",
            "secret_access_key": "ordinary-secret-metadata",
        }}),
    ] {
        let declared = manifest(vec![(
            "custom",
            Entry::new(EntryContent::Extension(
                DiscriminatedPayload::new("custom_children_entry")
                    .with_field("children", children.clone()),
            )),
        )]);

        let payload = DOCKER
            .serialize_session_state(&state(declared.clone()))
            .expect("serialize");
        let restored = DOCKER
            .deserialize_session_state(payload.clone(), &builtin_snapshot_registry(), &registries)
            .expect("deserialize");

        assert!(payload.get(REDACTED_MOUNT_AUTHORITY_KEY).is_none());
        assert_eq!(
            payload["manifest"]["entries"]["custom"]["children"],
            children
        );
        assert_eq!(restored.manifest(), &declared);
    }
}

#[test]
fn session_state_serialization_rejects_custom_mounts_and_strategies() {
    for declared in [
        manifest(vec![("data", entry(s3(false), custom_strategy()))]),
        manifest(vec![("data", Entry::mount(custom_mount()))]),
    ] {
        let error = DOCKER
            .serialize_session_state(&state(declared))
            .expect_err("refuse");
        assert_eq!(error.error_code(), ErrorCode::MountConfigInvalid);
        assert_absent(&format!("{error:?}"), "custom-strategy-secret");
        assert_absent(&format!("{error:?}"), "custom-mount-secret");
    }
}

#[test]
fn session_state_serialization_rejects_custom_credential_file_materializer() {
    let sentinel = "custom-source-secondary-secret";
    let declared = manifest(vec![
        (
            "credentials.json",
            Entry::new(EntryContent::Extension(
                DiscriminatedPayload::new("custom_credential_source_entry")
                    .with_field("content", "ordinary-content")
                    .with_field("source_token", sentinel),
            )),
        ),
        (
            "data",
            entry(
                gcs(|gcs| {
                    gcs.service_account_file = Some("/workspace/credentials.json".to_owned())
                }),
                docker(),
            ),
        ),
    ]);

    let error = DOCKER
        .serialize_session_state(&state(declared))
        .expect_err("refuse");

    assert_eq!(error.error_code(), ErrorCode::MountConfigInvalid);
    assert_absent(&format!("{error:?}"), sentinel);
}

#[test]
fn structural_local_dir_credential_path_remains_serializable() {
    let sentinel = "structural-local-dir-secret";
    let declared = manifest(vec![
        ("credentials", Entry::local_dir(None)),
        (
            "data",
            entry(
                gcs(|gcs| {
                    gcs.service_account_file = Some("/workspace/credentials/key.json".to_owned());
                    gcs.service_account_credentials = Some(sentinel.to_owned());
                }),
                docker(),
            ),
        ),
    ]);

    validate_on(&declared, "docker").expect("a structural directory");
    let payload = DOCKER
        .serialize_session_state(&state(declared))
        .expect("serialize");

    assert_eq!(payload[REDACTED_MOUNT_AUTHORITY_KEY], json!(true));
    assert_absent(&payload.to_string(), sentinel);
}

#[test]
fn opaque_external_authority_remains_resumable_through_trusted_rebind() {
    let declared = manifest(vec![(
        "data",
        entry(s3(false), docker_with(&[("vfs-cache-mode", "off")])),
    )]);

    let (payload, restored) = round_trip(declared.clone());
    let rebound = restored
        .rebind_persisted_mount_authority(Some(&declared), "docker")
        .expect("rebind");

    assert_eq!(payload[REDACTED_MOUNT_AUTHORITY_KEY], json!(true));
    assert_eq!(rebound.manifest(), &declared);
    assert!(!rebound.mount_authority_redacted());
}

#[test]
fn in_container_acknowledgement_is_rebound_only_from_trusted_manifest() {
    let declared = scoped(manifest(vec![("data", entry(s3(true), rclone()))]), "data");

    let (payload, restored) = round_trip(declared.clone());

    assert_absent(&payload.to_string(), "credential_exposure");
    let error = restored.assert_path_grants_rebound().expect_err("refuse");
    assert!(error.message().contains("cannot be resumed"), "{error}");

    let rebound = restored
        .rebind_persisted_mount_authority(Some(&declared), "docker")
        .expect("rebind");
    validate_on(rebound.manifest(), "docker").expect("rebound");
    assert!(
        rebound
            .manifest()
            .acknowledges_in_container_mount_credential_exposure(
                "/workspace/data",
                MountCredentialAuthority::MountScoped,
            )
    );
    rebound.assert_path_grants_rebound().expect("resumable");
}

#[test]
fn implicit_broad_authority_is_rebound_only_from_trusted_manifest() {
    for (provider, strategy) in [(azure(|_| {}), fuse()), (s3_files(), s3files(&[]))] {
        let declared = broad(manifest(vec![("data", entry(provider, strategy))]), "data");

        let (payload, restored) = round_trip(declared.clone());

        // Nothing to strip, and still the runtime-only acknowledgement has to come back.
        assert_eq!(payload[REDACTED_MOUNT_AUTHORITY_KEY], json!(true));
        assert_absent(&payload.to_string(), "credential_exposure");
        assert!(restored.mount_authority_redacted());
        let rebound = restored
            .rebind_persisted_mount_authority(Some(&declared), "docker")
            .expect("rebind");
        validate_on(rebound.manifest(), "docker").expect("rebound");
        assert!(
            rebound
                .manifest()
                .acknowledges_in_container_mount_credential_exposure(
                    "/workspace/data",
                    MountCredentialAuthority::Broad,
                )
        );
    }
}

#[test]
fn mount_authority_rebind_requires_exact_credential_free_topology() {
    let original = manifest(vec![("data", entry(s3(true), docker()))]);
    let (_, restored) = round_trip(original.clone());

    let other_bucket = manifest(vec![(
        "data",
        entry(
            MountProvider::S3(S3Mount {
                bucket: "different-bucket".to_owned(),
                access_key_id: Some("example-access-key".to_owned()),
                secret_access_key: Some("example-secret-key".to_owned()),
                ..S3Mount::default()
            }),
            docker(),
        ),
    )]);
    refused(
        restored
            .rebind_persisted_mount_authority(Some(&other_bucket), "docker")
            .map(|_| ()),
        "exactly matching",
    );

    let other_root = original.clone().with_root("/different-workspace");
    refused(
        restored
            .rebind_persisted_mount_authority(Some(&other_root), "docker")
            .map(|_| ()),
        "exactly matching",
    );

    let error = restored
        .rebind_persisted_mount_authority(None, "docker")
        .expect_err("refuse");
    assert!(
        error
            .message()
            .contains("requires a current trusted manifest"),
        "{error}"
    );
}

#[test]
fn session_state_serialization_redacts_pattern_authority() {
    let declared = manifest(vec![
        ("credentials.conf", Entry::file("credential-file-secret")),
        (
            "rclone",
            entry(
                s3(false),
                rclone_with(RcloneOptions {
                    extra_args: vec![
                        "--vfs-cache-mode=off".to_owned(),
                        "--config=/workspace/credentials.conf".to_owned(),
                    ],
                    ..RcloneOptions::default()
                }),
            ),
        ),
        (
            "s3files",
            entry(
                s3_files(),
                s3files(&[("tlsport", "4049"), ("secret_access_key", "pattern-secret")]),
            ),
        ),
        (
            "mountpoint",
            entry(
                s3(false),
                mountpoint(Some("https://example.test?signature=pattern-secret")),
            ),
        ),
    ]);

    let payload = DOCKER
        .serialize_session_state(&state(declared))
        .expect("serialize");
    let entries = &payload["manifest"]["entries"];

    assert_eq!(payload[REDACTED_MOUNT_AUTHORITY_KEY], json!(true));
    assert_eq!(entries["credentials.conf"]["content"], json!(""));
    assert_eq!(
        entries["rclone"]["mount_strategy"]["pattern"]["extra_args"],
        json!([])
    );
    assert_eq!(
        entries["s3files"]["mount_strategy"]["pattern"]["options"]["extra_options"],
        json!({})
    );
    assert_eq!(
        entries["mountpoint"]["mount_strategy"]["pattern"]["options"]["endpoint_url"],
        Value::Null
    );
    assert_absent(&payload.to_string(), "credential-file-secret");
    assert_absent(&payload.to_string(), "pattern-secret");
}

// --- raw payloads ------------------------------------------------------------------------------

fn gcs_with_credential_file(strategy: MountStrategy) -> Manifest {
    manifest(vec![
        ("credentials.json", Entry::file("credential-file-secret")),
        (
            "data",
            entry(
                gcs(|gcs| {
                    gcs.service_account_file = Some("/workspace/credentials.json".to_owned())
                }),
                strategy,
            ),
        ),
    ])
}

#[test]
fn raw_state_sanitization_preserves_explicit_sandbox_environment() {
    let mut payload = raw_state(&gcs_with_credential_file(mountpoint(None)));
    payload["base_envs"] = json!({
        "AWS_SECRET_ACCESS_KEY": "ambient-secret",
        "GITHUB_TOKEN": "unrelated",
    });

    let (sanitized, redacted) = sanitize_state(&payload).expect("sanitize");

    assert!(redacted);
    assert_eq!(sanitized[REDACTED_MOUNT_AUTHORITY_KEY], json!(true));
    assert_eq!(
        sanitized["manifest"]["entries"]["credentials.json"]["content"],
        json!("")
    );
    // A backend's own fields are its business, including an environment it chose to expose.
    assert_eq!(sanitized["base_envs"], payload["base_envs"]);
    assert_absent(&sanitized.to_string(), "credential-file-secret");
}

#[test]
fn raw_state_sanitization_rejects_credential_content_without_file_discriminator() {
    let mut payload = raw_state(&gcs_with_credential_file(docker()));
    payload["manifest"]["entries"]["credentials.json"]["type"] = json!("unknown_file");

    let error = sanitize_state(&payload).expect_err("refuse");

    assert_absent(&error.to_string(), "credential-file-secret");
}

#[test]
fn legacy_non_inline_credential_file_source_cannot_survive_deserialization() {
    let declared = manifest(vec![
        (
            "credentials.json",
            Entry::local_file("trusted/credentials.json"),
        ),
        (
            "data",
            entry(
                gcs(|gcs| {
                    gcs.service_account_file = Some("/workspace/credentials.json".to_owned())
                }),
                docker(),
            ),
        ),
    ]);

    let error = deserialize(raw_state(&declared)).expect_err("refuse");

    assert_eq!(error.error_code(), ErrorCode::SandboxConfigInvalid);
    assert_eq!(error.message(), "sandbox session state payload is invalid");
}

#[test]
fn raw_state_sanitization_rejects_unknown_nested_discriminators() {
    let declared = manifest(vec![
        (
            "docker",
            entry(s3(false), docker_with(&[("password", "driver-secret")])),
        ),
        (
            "s3files",
            entry(s3_files(), s3files(&[("password", "pattern-secret")])),
        ),
    ]);
    for location in ["strategy", "pattern"] {
        let sentinel = format!("unknown-{location}-secret");
        let mut payload = raw_state(&declared);
        if location == "strategy" {
            payload["manifest"]["entries"]["docker"]["mount_strategy"]["type"] = json!(sentinel);
        } else {
            payload["manifest"]["entries"]["s3files"]["mount_strategy"]["pattern"]["type"] =
                json!(sentinel);
        }

        let error = sanitize_state(&payload).expect_err("refuse");

        assert!(error.to_string().contains("unknown type"), "{error}");
        assert_absent(&error.to_string(), &sentinel);
    }
}

#[test]
fn raw_state_rejects_an_unregistered_mount_like_entry() {
    let payload = json!({
        "type": "docker",
        "manifest": {"entries": {"data": {
            "type": "custom_validated_s3_mount",
            "bucket": "example-bucket",
            "access_key_id": "access-key",
            "secret_access_key": "custom-validator-secret",
            "mount_strategy": {"type": "in_container", "pattern": {"type": "rclone"}},
        }}},
        "snapshot": Value::from(Snapshot::noop()),
    });

    let error = sanitize_state(&payload).expect_err("refuse");
    assert_eq!(
        error.message(),
        "sandbox manifest contains an unknown mount-like entry"
    );

    let error = deserialize(payload).expect_err("refuse");
    assert_absent(&format!("{error:?}"), "custom-validator-secret");
}

#[test]
fn raw_state_sanitization_strips_opaque_fields() {
    let mut payload = raw_state(&manifest(vec![("data", entry(s3(false), docker()))]));
    payload["manifest"]["entries"]["data"]["mount_strategy"]["api_token"] =
        json!("raw-strategy-secret");
    payload["manifest"]["entries"]["data"]["api_token"] = json!("raw-mount-secret");

    let (sanitized, redacted) = sanitize_state(&payload).expect("sanitize");

    assert!(redacted);
    assert!(
        sanitized["manifest"]["entries"]["data"]["mount_strategy"]
            .get("api_token")
            .is_none()
    );
    assert!(
        sanitized["manifest"]["entries"]["data"]
            .get("api_token")
            .is_none()
    );

    let mut payload = raw_state(&manifest(vec![("data", entry(s3(false), rclone()))]));
    let pattern = &mut payload["manifest"]["entries"]["data"]["mount_strategy"]["pattern"];
    pattern["api_token"] = json!("nested-pattern-secret");
    pattern["options"] = json!({"authorization": "nested-options-secret"});

    let (sanitized, redacted) = sanitize_state(&payload).expect("sanitize");

    let pattern = &sanitized["manifest"]["entries"]["data"]["mount_strategy"]["pattern"];
    assert!(redacted);
    assert!(pattern.get("api_token").is_none());
    assert!(pattern.get("options").is_none());
    assert_absent(&sanitized.to_string(), "nested-pattern-secret");
    assert_absent(&sanitized.to_string(), "nested-options-secret");
}

#[test]
fn deserialization_sanitizes_input_before_validation_errors() {
    let mut payload = raw_state(&manifest(vec![("data", entry(s3(true), docker()))]));
    payload["session_id"] = json!("not-a-uuid");
    let error = deserialize(payload).expect_err("refuse");
    assert_absent(&format!("{error:?}"), "example-secret-key");

    let sentinel = "raw-endpoint-secret";
    let mut payload = raw_state(&manifest(vec![("data", entry(s3(false), rclone()))]));
    payload["manifest"]["entries"]["data"]["endpoint_url"] = json!({"credential": sentinel});
    payload["session_id"] = json!("not-a-uuid");
    let error = deserialize(payload).expect_err("refuse");
    assert_absent(&format!("{error:?}"), sentinel);
}

#[test]
fn deserialization_scrubs_authority_before_invalid_strategy_discriminator() {
    let sentinel = "malformed-strategy-secret";
    let mut payload = raw_state(&manifest(vec![(
        "data",
        entry(s3(false), docker_with(&[("password", sentinel)])),
    )]));
    payload["manifest"]["entries"]["data"]["mount_strategy"]["type"] =
        json!({"invalid": "discriminator"});

    let error = deserialize(payload).expect_err("refuse");

    assert_absent(&format!("{error:?}"), sentinel);
}

#[test]
fn deserialization_rejects_unknown_discriminators_and_malformed_containers_without_values() {
    for location in ["strategy", "pattern"] {
        let sentinel = format!("unknown-{location}-secret");
        let mut payload = raw_state(&manifest(vec![("data", entry(s3(false), rclone()))]));
        let strategy = &mut payload["manifest"]["entries"]["data"]["mount_strategy"];
        if location == "strategy" {
            strategy["type"] = json!(sentinel);
        } else {
            strategy["pattern"]["type"] = json!(sentinel);
        }

        let error = deserialize(payload).expect_err("refuse");
        assert!(error.message().contains("payload is invalid"), "{error}");
        assert_absent(&format!("{error:?}"), &sentinel);
    }

    let sentinel = "malformed-entry-container-secret";
    let payload = json!({
        "type": "docker",
        "manifest": {"version": 1, "root": "/workspace", "entries": [sentinel]},
        "snapshot": Value::from(Snapshot::noop()),
    });
    let error = deserialize(payload).expect_err("refuse");
    assert!(error.message().contains("payload is invalid"), "{error}");
    assert_absent(&format!("{error:?}"), sentinel);
}

#[test]
fn the_generic_parser_sanitizes_legacy_mount_authority() {
    let sentinel = "legacy-session-state-secret";
    let provider = MountProvider::S3(S3Mount {
        access_key_id: Some("access-key".to_owned()),
        secret_access_key: Some(sentinel.to_owned()),
        ..s3_bucket()
    });
    let payload = raw_state(&manifest(vec![("data", entry(provider, docker()))]));

    let restored = SandboxSessionState::parse(
        payload,
        &builtin_snapshot_registry(),
        &ManifestRegistries::builtin(),
    )
    .expect("parse");

    assert!(restored.mount_authority_redacted());
    let EntryContent::Mount(mount) = restored.manifest().entries["data"].content() else {
        panic!("a mount");
    };
    let MountProvider::S3(provider) = mount.provider() else {
        panic!("an S3 mount");
    };
    assert_eq!(provider.access_key_id, None);
    assert_eq!(provider.secret_access_key, None);
    assert_absent(&format!("{restored:?}"), sentinel);
}

#[test]
fn a_direct_state_round_trip_redacts_mount_authority() {
    let sentinel = "direct-state-secret";
    let provider = MountProvider::S3(S3Mount {
        access_key_id: Some("access-key".to_owned()),
        secret_access_key: Some(sentinel.to_owned()),
        ..s3_bucket()
    });
    let original = state(manifest(vec![("data", entry(provider, docker()))]));

    let written = serde_json::to_string(&original).expect("serialize");
    let restored = SandboxSessionState::parse(
        serde_json::from_str(&written).expect("json"),
        &builtin_snapshot_registry(),
        &ManifestRegistries::builtin(),
    )
    .expect("parse");

    assert_absent(&written, sentinel);
    assert!(written.contains(REDACTED_MOUNT_AUTHORITY_KEY));
    assert!(restored.mount_authority_redacted());
}

#[test]
fn a_state_for_another_backend_is_refused() {
    let payload = Client("unix_local")
        .serialize_session_state(&SandboxSessionState::new(
            "unix_local",
            Snapshot::noop(),
            Manifest::new(),
        ))
        .expect("serialize");

    let error = deserialize(payload).expect_err("refuse");

    assert_eq!(error.message(), "sandbox session state payload is invalid");
}
