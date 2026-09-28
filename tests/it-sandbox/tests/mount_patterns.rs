//! `ra-sandbox::mounts::{config, patterns}`: what each provider tells its tool, and what each tool
//! is asked to run.
//!
//! Follows the reference's `test_mounts.py`. The session underneath records commands, directories
//! and writes, so the assertions are about exact command lines and file contents — the places a
//! mount goes wrong quietly, by mounting the wrong prefix or leaving a key on a command line.
//!
//! One difference in what is recorded: the reference's test session creates directories by running
//! `mkdir -p`, so its command lists include those; here directories are their own record.

use std::collections::BTreeMap;
use std::sync::Mutex;

use async_trait::async_trait;
use ra_core::sandbox::{
    AsUser, AzureBlobMount, BoxMount, ErrorCode, ExecRequest, ExecResult, FileEntry, FuseCacheType,
    FuseOptions, GcsMount, Manifest, Mount, MountPattern, MountProvider, MountStrategy,
    MountpointOptions, OpName, PosixPath, R2Mount, RcloneMode, RcloneOptions, S3FilesMount,
    S3FilesOptions, S3Mount, SandboxResult, SandboxSession, SandboxSessionState, SessionPath,
    SessionResources, ShellInvocation, Snapshot,
};
use ra_sandbox::mounts::config::{
    FuseMountConfig, MountpointMountConfig, RcloneMountConfig, S3FilesMountConfig,
    resolve_remote_name,
};
use ra_sandbox::mounts::{
    BuiltinMountLifecycle, MountLifecycle, MountPatternConfig, apply_pattern,
    build_in_container_mount_config, docker_volume_driver_config,
};
use serde_json::json;
use uuid::Uuid;

/// The session id the reference's generated-path tests use.
const SESSION: &str = "12345678-1234-5678-1234-567812345678";
const SESSION_HEX: &str = "12345678123456781234567812345678";

/// A session that records what it is asked to do.
struct Recorder {
    state: SandboxSessionState,
    resources: SessionResources,
    commands: Mutex<Vec<Vec<String>>>,
    requests: Mutex<Vec<ExecRequest>>,
    mkdirs: Mutex<Vec<String>>,
    writes: Mutex<Vec<(String, Vec<u8>)>>,
    /// What a read of any file returns.
    file_text: Option<String>,
    /// A script containing this, and not a tool check, exits 1 with `failing_stderr`.
    failing: Option<&'static str>,
    failing_stderr: &'static str,
}

impl Recorder {
    fn new() -> Self {
        Self {
            state: SandboxSessionState::new("recording", Snapshot::noop(), Manifest::new())
                .with_session_id(Uuid::parse_str(SESSION).expect("uuid")),
            resources: SessionResources::new(),
            commands: Mutex::new(Vec::new()),
            requests: Mutex::new(Vec::new()),
            mkdirs: Mutex::new(Vec::new()),
            writes: Mutex::new(Vec::new()),
            file_text: None,
            failing: None,
            failing_stderr: "",
        }
    }

    fn with_manifest(manifest: Manifest) -> Self {
        Self {
            state: SandboxSessionState::new("recording", Snapshot::noop(), manifest)
                .with_session_id(Uuid::parse_str(SESSION).expect("uuid")),
            ..Self::new()
        }
    }

    fn reading(text: &str) -> Self {
        Self {
            file_text: Some(text.to_owned()),
            ..Self::new()
        }
    }

    fn commands(&self) -> Vec<Vec<String>> {
        self.commands.lock().expect("commands").clone()
    }

    fn mkdirs(&self) -> Vec<String> {
        self.mkdirs.lock().expect("mkdirs").clone()
    }

    fn writes(&self) -> Vec<(String, Vec<u8>)> {
        self.writes.lock().expect("writes").clone()
    }
}

#[async_trait]
impl SandboxSession for Recorder {
    fn backend_id(&self) -> &str {
        "recording"
    }

    fn state(&self) -> SandboxSessionState {
        self.state.clone()
    }

    fn resources(&self) -> &SessionResources {
        &self.resources
    }

    async fn exec(&self, request: ExecRequest) -> SandboxResult<ExecResult> {
        self.requests
            .lock()
            .expect("requests")
            .push(request.clone());
        let script = request.command.join(" ");
        let command = match request.shell {
            ShellInvocation::Login => vec!["sh".to_owned(), "-lc".to_owned(), script.clone()],
            ShellInvocation::None => request.command,
            ShellInvocation::Prefix(_) => panic!("unexpected custom shell"),
        };
        self.commands.lock().expect("commands").push(command);
        if let Some(needle) = self.failing
            && script.contains(needle)
            && !script.contains("command -v ")
        {
            return Ok(ExecResult::new(
                Vec::new(),
                self.failing_stderr.as_bytes().to_vec(),
                1,
            ));
        }
        Ok(ExecResult::new(Vec::new(), Vec::new(), 0))
    }

    async fn running(&self) -> SandboxResult<bool> {
        Ok(true)
    }

    async fn ls(&self, _path: SessionPath<'_>, _user: AsUser) -> SandboxResult<Vec<FileEntry>> {
        Ok(Vec::new())
    }

    async fn rm(
        &self,
        _path: SessionPath<'_>,
        _recursive: bool,
        _user: AsUser,
    ) -> SandboxResult<()> {
        Ok(())
    }

    async fn mkdir(
        &self,
        path: SessionPath<'_>,
        _parents: bool,
        _user: AsUser,
    ) -> SandboxResult<()> {
        let path = path.as_str();
        self.mkdirs.lock().expect("mkdirs").push(path.to_owned());
        Ok(())
    }

    async fn read(&self, path: SessionPath<'_>, _user: AsUser) -> SandboxResult<Vec<u8>> {
        let path = path.as_str();
        self.file_text
            .clone()
            .map(String::into_bytes)
            .ok_or_else(|| ra_core::sandbox::SandboxError::workspace_read_not_found(path))
    }

    async fn write(
        &self,
        path: SessionPath<'_>,
        data: Vec<u8>,
        _user: AsUser,
    ) -> SandboxResult<()> {
        let path = path.as_str();
        self.writes
            .lock()
            .expect("writes")
            .push((path.to_owned(), data));
        Ok(())
    }

    async fn persist_workspace(&self) -> SandboxResult<Vec<u8>> {
        Ok(Vec::new())
    }

    async fn hydrate_workspace(&self, _data: Vec<u8>) -> SandboxResult<()> {
        Ok(())
    }
}

fn strings(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|part| (*part).to_owned()).collect()
}

fn sh(script: &str) -> Vec<String> {
    strings(&["sh", "-lc", script])
}

fn in_container(provider: MountProvider, pattern: MountPattern) -> Mount {
    Mount::new(provider, MountStrategy::in_container(pattern)).expect("supported")
}

fn rclone() -> MountPattern {
    MountPattern::Rclone(RcloneOptions::default())
}

fn rclone_from_file() -> MountPattern {
    MountPattern::Rclone(RcloneOptions {
        config_file_path: Some("rclone.conf".to_owned()),
        ..RcloneOptions::default()
    })
}

async fn rclone_config(mount: &Mount, session: &Recorder, include: bool) -> RcloneMountConfig {
    let pattern = match mount.strategy() {
        MountStrategy::InContainer { pattern } => pattern.clone(),
        _ => unreachable!("in-container"),
    };
    match build_in_container_mount_config(mount, &pattern, session, include)
        .await
        .expect("config")
    {
        MountPatternConfig::Rclone(config) => config,
        other => panic!("expected rclone, got {other:?}"),
    }
}

fn remote(kind: &str) -> String {
    resolve_remote_name(&RcloneOptions::default(), SESSION_HEX, kind, "mount").expect("name")
}

fn text(config: &RcloneMountConfig) -> &str {
    config.config_text.as_deref().expect("config text")
}

// --- provider configuration: rclone -------------------------------------------------------------

#[tokio::test]
async fn an_azure_container_extends_the_configuration_file_it_was_pointed_at() {
    let name = remote("azureblob");
    let session = Recorder::reading(&format!("[{name}]\ntype = azureblob\n"));
    let mount = in_container(
        MountProvider::AzureBlob(AzureBlobMount {
            account: "acct".to_owned(),
            container: "container".to_owned(),
            ..AzureBlobMount::default()
        }),
        rclone_from_file(),
    );

    let applying = rclone_config(&mount, &session, true).await;
    let detaching = rclone_config(&mount, &session, false).await;

    assert_eq!(applying.remote_name, name);
    assert_eq!(applying.remote_path, "container");
    assert!(text(&applying).contains("account = acct"));
    // Detaching only needs to know which remote it was: nothing is read or synthesized.
    assert_eq!(detaching.remote_name, name);
    assert_eq!(detaching.config_text, None);
}

#[tokio::test]
async fn an_azure_container_with_a_managed_identity_turns_msi_on() {
    let mount = in_container(
        MountProvider::AzureBlob(AzureBlobMount {
            account: "acct".to_owned(),
            container: "container".to_owned(),
            identity_client_id: Some("managed-identity-client-id".to_owned()),
            ..AzureBlobMount::default()
        }),
        rclone(),
    );

    let config = rclone_config(&mount, &Recorder::new(), true).await;

    assert!(text(&config).contains("use_msi = true"));
    assert!(text(&config).contains("msi_client_id = managed-identity-client-id"));
    assert!(!text(&config).contains("use_msi = false"));
}

#[tokio::test]
async fn a_box_folder_carries_its_auth_options_into_the_configuration() {
    let name = remote("box");
    let session = Recorder::reading(&format!("[{name}]\ntype = box\n"));
    let mount = in_container(
        MountProvider::Box(BoxMount {
            path: Some("/Shared/Finance".to_owned()),
            client_id: Some("client-id".to_owned()),
            client_secret: Some("client-secret".to_owned()),
            token: Some(r#"{"access_token":"token"}"#.to_owned()),
            root_folder_id: Some("12345".to_owned()),
            impersonate: Some("user-42".to_owned()),
            ..BoxMount::default()
        }),
        rclone_from_file(),
    )
    .writable(true);

    let applying = rclone_config(&mount, &session, true).await;
    let detaching = rclone_config(&mount, &session, false).await;

    assert_eq!(applying.remote_name, name);
    assert_eq!(applying.remote_path, "Shared/Finance");
    assert!(!applying.read_only);
    for line in [
        "type = box",
        "client_id = client-id",
        "client_secret = client-secret",
        r#"token = {"access_token":"token"}"#,
        "root_folder_id = 12345",
        "impersonate = user-42",
    ] {
        assert!(text(&applying).contains(line), "{line}");
    }
    assert_eq!(detaching.remote_path, "Shared/Finance");
    assert_eq!(detaching.config_text, None);
}

#[tokio::test]
async fn a_public_gcs_bucket_is_mounted_anonymously_through_the_native_backend() {
    let mount = in_container(
        MountProvider::Gcs(GcsMount {
            bucket: "public-bucket".to_owned(),
            ..GcsMount::default()
        }),
        rclone(),
    );

    let config = rclone_config(&mount, &Recorder::new(), true).await;

    assert_eq!(
        text(&config),
        format!(
            "[{}]\ntype = google cloud storage\nanonymous = true\nenv_auth = false\n",
            remote("gcs")
        )
    );
}

#[tokio::test]
async fn a_gcs_bucket_with_a_service_account_writes_each_credential_it_was_given() {
    let mount = in_container(
        MountProvider::Gcs(GcsMount {
            bucket: "bucket".to_owned(),
            prefix: Some("nested/prefix/".to_owned()),
            service_account_file: Some("/data/config/gcs.json".to_owned()),
            service_account_credentials: Some(r#"{"type":"service_account"}"#.to_owned()),
            access_token: Some("token".to_owned()),
            ..GcsMount::default()
        }),
        rclone(),
    );

    let config = rclone_config(&mount, &Recorder::new(), true).await;

    assert_eq!(config.remote_name, remote("gcs"));
    assert_eq!(config.remote_path, "bucket/nested/prefix/");
    assert_eq!(
        text(&config),
        format!(
            "[{}]\ntype = google cloud storage\nservice_account_file = /data/config/gcs.json\n\
             service_account_credentials = {{\"type\":\"service_account\"}}\naccess_token = token\n\
             env_auth = false\n",
            remote("gcs")
        )
    );
}

#[tokio::test]
async fn a_gcs_bucket_with_an_hmac_pair_goes_through_the_s3_backend() {
    let mount = in_container(
        MountProvider::Gcs(GcsMount {
            bucket: "bucket".to_owned(),
            access_id: Some("access-id".to_owned()),
            secret_access_key: Some("secret-key".to_owned()),
            prefix: Some("nested/prefix/".to_owned()),
            region: Some("auto".to_owned()),
            ..GcsMount::default()
        }),
        rclone(),
    );

    let config = rclone_config(&mount, &Recorder::new(), true).await;

    assert_eq!(config.remote_name, remote("gcs_s3"));
    assert_eq!(config.remote_path, "bucket/nested/prefix/");
    assert_eq!(
        text(&config),
        format!(
            "[{}]\ntype = s3\nprovider = GCS\nenv_auth = false\naccess_key_id = access-id\n\
             secret_access_key = secret-key\nendpoint = https://storage.googleapis.com\n\
             region = auto\n",
            remote("gcs_s3")
        )
    );
}

#[tokio::test]
async fn an_hmac_gcs_remote_and_an_s3_remote_in_one_session_do_not_share_a_name() {
    let session = Recorder::new();
    let s3 = in_container(
        MountProvider::S3(S3Mount {
            bucket: "s3-bucket".to_owned(),
            ..S3Mount::default()
        }),
        rclone(),
    );
    let gcs = in_container(
        MountProvider::Gcs(GcsMount {
            bucket: "gcs-bucket".to_owned(),
            access_id: Some("access-id".to_owned()),
            secret_access_key: Some("secret-key".to_owned()),
            ..GcsMount::default()
        }),
        rclone(),
    );

    let s3 = rclone_config(&s3, &session, true).await;
    let gcs = rclone_config(&gcs, &session, true).await;

    assert_eq!(s3.remote_name, format!("sandbox_s3_{SESSION_HEX}"));
    assert_eq!(gcs.remote_name, format!("sandbox_gcs_s3_{SESSION_HEX}"));
}

#[tokio::test]
async fn an_s3_prefix_is_part_of_the_remote_path() {
    let mount = in_container(
        MountProvider::S3(S3Mount {
            bucket: "bucket".to_owned(),
            prefix: Some("nested/prefix/".to_owned()),
            ..S3Mount::default()
        }),
        rclone(),
    );

    let config = rclone_config(&mount, &Recorder::new(), true).await;

    assert_eq!(config.remote_name, remote("s3"));
    assert_eq!(config.remote_path, "bucket/nested/prefix/");
}

#[tokio::test]
async fn an_s3_configuration_names_its_endpoint_and_region_only_when_given_them() {
    let keys = S3Mount {
        bucket: "my-bucket".to_owned(),
        access_key_id: Some("ak".to_owned()),
        secret_access_key: Some("sk".to_owned()),
        ..S3Mount::default()
    };
    let cases = [
        (
            S3Mount {
                endpoint_url: Some("http://localhost:9000".to_owned()),
                region: Some("us-west-2".to_owned()),
                ..keys.clone()
            },
            "type = s3\nprovider = AWS\nendpoint = http://localhost:9000\nregion = us-west-2\n\
             env_auth = false\naccess_key_id = ak\nsecret_access_key = sk\n",
        ),
        (
            keys.clone(),
            "type = s3\nprovider = AWS\nenv_auth = false\naccess_key_id = ak\nsecret_access_key = sk\n",
        ),
        // An S3-compatible service that needs path-style addressing names its own provider.
        (
            S3Mount {
                endpoint_url: Some("http://localhost:9000".to_owned()),
                s3_provider: "Other".to_owned(),
                ..keys
            },
            "type = s3\nprovider = Other\nendpoint = http://localhost:9000\nenv_auth = false\n\
             access_key_id = ak\nsecret_access_key = sk\n",
        ),
    ];
    for (s3, expected) in cases {
        let mount = in_container(MountProvider::S3(s3), rclone());

        let config = rclone_config(&mount, &Recorder::new(), true).await;

        assert_eq!(text(&config), format!("[{}]\n{expected}", remote("s3")));
    }
}

#[tokio::test]
async fn an_r2_bucket_is_reached_through_its_account_endpoint_or_a_custom_domain() {
    let cases = [
        (
            R2Mount {
                bucket: "bucket".to_owned(),
                account_id: "abc123accountid".to_owned(),
                access_key_id: Some("r2-access".to_owned()),
                secret_access_key: Some("r2-secret".to_owned()),
                ..R2Mount::default()
            },
            "type = s3\nprovider = Cloudflare\nendpoint = https://abc123accountid.r2.cloudflarestorage.com\n\
             acl = private\nenv_auth = false\naccess_key_id = r2-access\nsecret_access_key = r2-secret\n",
        ),
        (
            R2Mount {
                bucket: "bucket".to_owned(),
                account_id: "abc123accountid".to_owned(),
                custom_domain: Some("https://eu.r2.cloudflarestorage.com".to_owned()),
                ..R2Mount::default()
            },
            "type = s3\nprovider = Cloudflare\nendpoint = https://eu.r2.cloudflarestorage.com\n\
             acl = private\nenv_auth = false\n",
        ),
    ];
    for (r2, expected) in cases {
        let mount = in_container(MountProvider::R2(r2), rclone());

        let config = rclone_config(&mount, &Recorder::new(), true).await;

        assert_eq!(config.remote_name, remote("r2"));
        assert_eq!(config.remote_path, "bucket");
        assert_eq!(text(&config), format!("[{}]\n{expected}", remote("r2")));
    }
}

#[tokio::test]
async fn an_existing_section_is_extended_in_place_and_the_rest_of_the_file_kept() {
    let name = remote("r2");
    let session = Recorder::reading(&format!(
        "[{name}]\ntype = s3\nregion = auto\n\n[other]\ntype = memory\n"
    ));
    let mount = in_container(
        MountProvider::R2(R2Mount {
            bucket: "bucket".to_owned(),
            account_id: "abc123accountid".to_owned(),
            access_key_id: Some("r2-access".to_owned()),
            secret_access_key: Some("r2-secret".to_owned()),
            ..R2Mount::default()
        }),
        rclone_from_file(),
    );

    let config = rclone_config(&mount, &session, true).await;

    assert_eq!(
        text(&config),
        format!(
            "[{name}]\ntype = s3\nregion = auto\ntype = s3\nprovider = Cloudflare\n\
             endpoint = https://abc123accountid.r2.cloudflarestorage.com\nacl = private\n\
             env_auth = false\naccess_key_id = r2-access\nsecret_access_key = r2-secret\n\
             \n[other]\ntype = memory\n"
        )
    );
}

#[tokio::test]
async fn a_configuration_file_without_the_remote_is_refused_naming_what_was_missing() {
    let session = Recorder::reading("[someone-else]\ntype = s3\n");
    let mount = in_container(
        MountProvider::S3(S3Mount {
            bucket: "bucket".to_owned(),
            ..S3Mount::default()
        }),
        rclone_from_file(),
    );

    let error = build_in_container_mount_config(&mount, &rclone_from_file(), &session, true)
        .await
        .expect_err("no section for the remote");

    assert_eq!(error.error_code(), ErrorCode::MountConfigInvalid);
    assert_eq!(
        error.to_string(),
        "rclone config missing required remote section"
    );
    assert_eq!(
        error.context().get("path"),
        Some(&json!("/workspace/rclone.conf"))
    );
    assert_eq!(
        error.context().get("remote_name"),
        Some(&json!(remote("s3")))
    );
}

#[tokio::test]
async fn an_r2_mount_with_half_a_credential_pair_is_refused_for_either_strategy() {
    let message = "r2 credentials must include both access_key_id and secret_access_key";
    let in_container_mount = in_container(
        MountProvider::R2(R2Mount {
            bucket: "bucket".to_owned(),
            account_id: "abc123accountid".to_owned(),
            access_key_id: Some("r2-access".to_owned()),
            ..R2Mount::default()
        }),
        rclone(),
    );
    let error =
        build_in_container_mount_config(&in_container_mount, &rclone(), &Recorder::new(), true)
            .await
            .expect_err("half a pair");
    assert_eq!(error.to_string(), message);

    let docker = Mount::new(
        MountProvider::R2(R2Mount {
            bucket: "bucket".to_owned(),
            account_id: "abc123accountid".to_owned(),
            secret_access_key: Some("r2-secret".to_owned()),
            ..R2Mount::default()
        }),
        MountStrategy::docker_volume("rclone"),
    )
    .expect("supported");
    let error = docker_volume_driver_config(&docker, docker.strategy()).expect_err("half a pair");
    assert_eq!(error.to_string(), message);
}

// --- provider configuration: Mountpoint and S3 Files ---------------------------------------------

fn mountpoint_config(mount: MountPatternConfig) -> MountpointMountConfig {
    match mount {
        MountPatternConfig::Mountpoint(config) => config,
        other => panic!("expected mountpoint, got {other:?}"),
    }
}

#[tokio::test]
async fn a_gcs_bucket_mounted_with_mountpoint_defaults_to_the_google_endpoint() {
    let pattern = MountPattern::Mountpoint(MountpointOptions::default());
    let mount = in_container(
        MountProvider::Gcs(GcsMount {
            bucket: "bucket".to_owned(),
            ..GcsMount::default()
        }),
        pattern.clone(),
    )
    .writable(true);

    let config = mountpoint_config(
        build_in_container_mount_config(&mount, &pattern, &Recorder::new(), false)
            .await
            .expect("config"),
    );

    assert_eq!(
        config.endpoint_url.as_deref(),
        Some("https://storage.googleapis.com")
    );
    assert!(!config.read_only);
}

#[tokio::test]
async fn an_s3_mounts_own_fields_win_over_the_patterns_defaults() {
    let pattern = MountPattern::Mountpoint(MountpointOptions {
        prefix: Some("pattern-prefix/".to_owned()),
        region: Some("pattern-region".to_owned()),
        endpoint_url: Some("https://pattern.example.test".to_owned()),
    });
    let mount = in_container(
        MountProvider::S3(S3Mount {
            bucket: "bucket".to_owned(),
            prefix: Some("direct-prefix/".to_owned()),
            region: Some("direct-region".to_owned()),
            endpoint_url: Some("https://direct.example.test".to_owned()),
            ..S3Mount::default()
        }),
        pattern.clone(),
    );

    let config = mountpoint_config(
        build_in_container_mount_config(&mount, &pattern, &Recorder::new(), false)
            .await
            .expect("config"),
    );

    assert_eq!(config.prefix.as_deref(), Some("direct-prefix/"));
    assert_eq!(config.region.as_deref(), Some("direct-region"));
    assert_eq!(
        config.endpoint_url.as_deref(),
        Some("https://direct.example.test")
    );
}

#[tokio::test]
async fn an_s3_files_mount_falls_back_to_the_patterns_defaults_field_by_field() {
    let pattern = MountPattern::S3Files(S3FilesOptions {
        mount_target_ip: Some("10.99.1.209".to_owned()),
        access_point: Some("fsap-pattern".to_owned()),
        region: Some("us-east-1".to_owned()),
        extra_options: BTreeMap::from([("tlsport".to_owned(), Some("3049".to_owned()))]),
    });
    let mount = in_container(
        MountProvider::S3Files(S3FilesMount {
            file_system_id: "fs-1234567890abcdef0".to_owned(),
            subpath: Some("/datasets".to_owned()),
            access_point: Some("fsap-direct".to_owned()),
            extra_options: BTreeMap::from([
                ("tlsport".to_owned(), Some("4049".to_owned())),
                ("iam".to_owned(), None),
            ]),
            ..S3FilesMount::default()
        }),
        pattern.clone(),
    );

    let MountPatternConfig::S3Files(config) =
        build_in_container_mount_config(&mount, &pattern, &Recorder::new(), false)
            .await
            .expect("config")
    else {
        panic!("expected s3files");
    };

    assert_eq!(config.file_system_id, "fs-1234567890abcdef0");
    assert_eq!(config.subpath.as_deref(), Some("/datasets"));
    assert_eq!(config.mount_target_ip.as_deref(), Some("10.99.1.209"));
    assert_eq!(config.access_point.as_deref(), Some("fsap-direct"));
    assert_eq!(config.region.as_deref(), Some("us-east-1"));
    assert_eq!(
        config.extra_options,
        BTreeMap::from([
            ("iam".to_owned(), None),
            ("tlsport".to_owned(), Some("4049".to_owned())),
        ])
    );
}

// --- Mountpoint commands -----------------------------------------------------------------------

fn mountpoint(bucket: &str, mount_type: &str) -> MountpointMountConfig {
    MountpointMountConfig {
        bucket: bucket.to_owned(),
        access_key_id: Some("access".to_owned()),
        secret_access_key: Some("secret".to_owned()),
        session_token: None,
        prefix: None,
        region: None,
        endpoint_url: None,
        mount_type: mount_type.to_owned(),
        read_only: true,
    }
}

async fn apply_mountpoint(session: &Recorder, config: MountpointMountConfig) -> SandboxResult<()> {
    apply_pattern(
        &MountPattern::Mountpoint(MountpointOptions::default()),
        session,
        &PosixPath::new("/workspace/remote"),
        &MountPatternConfig::Mountpoint(config),
    )
    .await
}

/// The script the final `sh -lc` ran.
fn last_script(session: &Recorder) -> String {
    let commands = session.commands();
    let last = commands.last().expect("a command");
    assert_eq!(last[..2], ["sh", "-lc"]);
    last[2].clone()
}

#[tokio::test]
async fn mountpoint_keys_go_into_an_owner_only_file_not_onto_the_command_line() {
    for (mount_type, region, endpoint, read_only) in [
        (
            "gcs_mount",
            "us-east1",
            Some("https://storage.googleapis.com"),
            true,
        ),
        (
            "gcs_mount",
            "us-east1",
            Some("https://storage.googleapis.com"),
            false,
        ),
    ] {
        let session = Recorder::new();

        apply_mountpoint(
            &session,
            MountpointMountConfig {
                region: Some(region.to_owned()),
                endpoint_url: endpoint.map(str::to_owned),
                read_only,
                ..mountpoint("bucket", mount_type)
            },
        )
        .await
        .expect("mounts");

        assert_eq!(
            session.commands()[0],
            sh("command -v mount-s3 >/dev/null 2>&1")
        );
        assert_eq!(session.mkdirs()[0], "/workspace/remote");
        let writes = session.writes();
        assert_eq!(writes.len(), 1);
        let (env_path, env_payload) = &writes[0];
        assert!(
            env_path.starts_with(&format!(
                "/workspace/.sandbox-mountpoint-env/{SESSION_HEX}/"
            )),
            "{env_path}"
        );
        assert!(env_path.ends_with(".env"));
        assert_eq!(
            env_payload.as_slice(),
            b"export AWS_ACCESS_KEY_ID=access\nexport AWS_SECRET_ACCESS_KEY=secret\n"
        );
        assert!(
            session
                .commands()
                .contains(&strings(&["chmod", "0600", env_path]))
        );

        let script = last_script(&session);
        assert!(script.contains("mount-s3"));
        assert!(!script.contains("AWS_ACCESS_KEY_ID=access"));
        assert!(!script.contains("AWS_SECRET_ACCESS_KEY=secret"));
        assert!(script.contains(".sandbox-mountpoint-env"));
        assert!(script.contains(&format!("--region {region}")));
        assert!(script.contains("--endpoint-url https://storage.googleapis.com"));
        assert!(script.contains("--upload-checksums off"));
        assert!(script.contains("bucket /workspace/remote"));
        assert_eq!(script.contains("--read-only"), read_only);
        assert_eq!(script.contains("--allow-overwrite"), !read_only);
        assert_eq!(script.contains("--allow-delete"), !read_only);
    }
}

#[tokio::test]
async fn a_writable_s3_mountpoint_may_overwrite_and_delete_and_carries_its_session_token() {
    let session = Recorder::new();

    apply_mountpoint(
        &session,
        MountpointMountConfig {
            session_token: Some("token".to_owned()),
            region: Some("us-east-1".to_owned()),
            read_only: false,
            ..mountpoint("bucket", "s3_mount")
        },
    )
    .await
    .expect("mounts");

    assert_eq!(
        session.writes()[0].1.as_slice(),
        b"export AWS_ACCESS_KEY_ID=access\nexport AWS_SECRET_ACCESS_KEY=secret\nexport AWS_SESSION_TOKEN=token\n"
    );
    let script = last_script(&session);
    assert!(!script.contains("--read-only"));
    assert!(script.contains("--allow-overwrite"));
    assert!(script.contains("--allow-delete"));
    assert!(script.contains("--region us-east-1"));
    assert!(!script.contains("AWS_SESSION_TOKEN=token"));
    assert!(script.contains("bucket /workspace/remote"));
}

#[tokio::test]
async fn a_mountpoint_mount_without_keys_does_not_sign_its_requests() {
    let session = Recorder::new();

    apply_mountpoint(
        &session,
        MountpointMountConfig {
            access_key_id: None,
            secret_access_key: None,
            region: Some("us-east-1".to_owned()),
            ..mountpoint("public-bucket", "s3_mount")
        },
    )
    .await
    .expect("mounts");

    assert!(last_script(&session).contains("--no-sign-request"));
    assert!(session.writes().is_empty());
}

#[tokio::test]
async fn a_failed_mountpoint_command_says_nothing_about_what_it_was_given() {
    let session = Recorder {
        failing: Some("mount-s3 "),
        failing_stderr: "bad credentials: access secret token",
        ..Recorder::new()
    };

    let error = apply_mountpoint(
        &session,
        MountpointMountConfig {
            session_token: Some("token".to_owned()),
            region: Some("us-east-1".to_owned()),
            endpoint_url: Some("https://user:inline-endpoint-secret@example.test".to_owned()),
            read_only: false,
            ..mountpoint("bucket", "s3_mount")
        },
    )
    .await
    .expect_err("the mount command failed");

    assert_eq!(error.error_code(), ErrorCode::MountFailed);
    assert_eq!(error.op(), OpName::Materialize);
    assert_eq!(error.retryable(), Some(false));
    assert!(error.context().is_empty(), "{:?}", error.context());
    let command = session.commands().last().expect("ran").join(" ");
    assert!(command.contains(".sandbox-mountpoint-env"));
    assert!(
        session
            .persist_workspace_skip_relpaths()
            .expect("skip paths")
            .iter()
            .any(|path| path.as_str().starts_with(".sandbox-mountpoint-env/")),
    );
    for sensitive in ["access", "secret", "token", "inline-endpoint-secret"] {
        assert!(!command.contains(sensitive), "{sensitive} in {command}");
        assert!(!format!("{error:?}").contains(sensitive), "{sensitive}");
        assert!(!error.to_string().contains(sensitive), "{sensitive}");
    }
}

/// The event half of `test_s3_mountpoint_failure_redacts_credentials_from_errors_and_events`: the
/// same failure, applied through an instrumented session, leaves nothing sensitive in its events.
#[tokio::test]
async fn a_failed_mountpoint_command_puts_nothing_it_was_given_into_the_events() {
    use std::sync::Arc;

    use ra_core::sandbox::{EventSink, SandboxSessionEvent};
    use ra_sandbox::instrumentation::{Instrumentation, InstrumentedSession};
    use ra_sandbox::sinks::CallbackSink;

    let events: Arc<Mutex<Vec<SandboxSessionEvent>>> = Arc::default();
    let seen = Arc::clone(&events);
    let sink = CallbackSink::new(move |event, _session| {
        seen.lock().expect("events").push(event);
        Ok(())
    });
    let session = InstrumentedSession::new(
        Arc::new(Recorder {
            failing: Some("mount-s3 "),
            failing_stderr: "bad credentials: access secret token",
            ..Recorder::new()
        }),
        Some(Arc::new(Instrumentation::with_sinks([
            Arc::new(sink) as Arc<dyn EventSink>
        ]))),
        None,
    )
    .expect("wrap");

    let error = apply_pattern(
        &MountPattern::Mountpoint(MountpointOptions::default()),
        &session,
        &PosixPath::new("/workspace/remote"),
        &MountPatternConfig::Mountpoint(MountpointMountConfig {
            session_token: Some("token".to_owned()),
            region: Some("us-east-1".to_owned()),
            endpoint_url: Some("https://user:inline-endpoint-secret@example.test".to_owned()),
            read_only: false,
            ..mountpoint("bucket", "s3_mount")
        }),
    )
    .await
    .expect_err("the mount command failed");
    assert_eq!(error.error_code(), ErrorCode::MountFailed);

    let events = events.lock().expect("events").clone();
    assert!(
        events
            .iter()
            .any(|event| event.op() == OpName::Exec && event.as_finish().is_some_and(|f| !f.ok())),
        "the failing mount command was recorded"
    );
    let serialized: Vec<String> = events
        .iter()
        .map(|event| serde_json::to_string(event).expect("json"))
        .collect();
    let serialized = serialized.join("\n");
    for sensitive in ["access", "secret", "token", "inline-endpoint-secret"] {
        assert!(
            !serialized.contains(sensitive),
            "{sensitive} in {serialized}"
        );
    }
}

// --- S3 Files commands -------------------------------------------------------------------------

#[tokio::test]
async fn an_s3_files_mount_hands_its_helper_every_option_in_order() {
    let session = Recorder::new();

    apply_pattern(
        &MountPattern::S3Files(S3FilesOptions::default()),
        &session,
        &PosixPath::new("/workspace/remote"),
        &MountPatternConfig::S3Files(S3FilesMountConfig {
            file_system_id: "fs-1234567890abcdef0".to_owned(),
            subpath: Some("/datasets".to_owned()),
            mount_target_ip: Some("10.99.1.209".to_owned()),
            access_point: Some("fsap-123".to_owned()),
            region: Some("us-east-1".to_owned()),
            extra_options: BTreeMap::from([("tlsport".to_owned(), Some("4049".to_owned()))]),
            mount_type: "s3_files_mount".to_owned(),
            read_only: true,
        }),
    )
    .await
    .expect("mounts");

    assert_eq!(
        session.commands(),
        [
            sh("command -v mount.s3files >/dev/null 2>&1"),
            strings(&[
                "mount",
                "-t",
                "s3files",
                "-o",
                "tlsport=4049,ro,mounttargetip=10.99.1.209,accesspoint=fsap-123,region=us-east-1",
                "fs-1234567890abcdef0:/datasets",
                "/workspace/remote",
            ]),
        ]
    );
    assert_eq!(session.mkdirs(), ["/workspace/remote"]);
}

// --- rclone commands ---------------------------------------------------------------------------

fn rclone_runtime(read_only: bool) -> RcloneMountConfig {
    RcloneMountConfig {
        remote_name: "remote".to_owned(),
        remote_path: "bucket".to_owned(),
        remote_kind: "s3".to_owned(),
        mount_type: "s3_mount".to_owned(),
        config_text: Some("[remote]\ntype = s3\n".to_owned()),
        read_only,
    }
}

#[tokio::test]
async fn a_generated_rclone_configuration_is_its_own_owner_only_file() {
    let session = Recorder::new();
    let config_path = format!("/workspace/.sandbox-rclone-config/{SESSION_HEX}/remote.conf");

    apply_pattern(
        &rclone(),
        &session,
        &PosixPath::new("/workspace/mnt"),
        &MountPatternConfig::Rclone(rclone_runtime(true)),
    )
    .await
    .expect("mounts");

    assert_eq!(
        session.writes(),
        [(config_path.clone(), b"[remote]\ntype = s3\n".to_vec())]
    );
    assert_eq!(
        session.mkdirs(),
        [
            "/workspace/mnt".to_owned(),
            format!("/workspace/.sandbox-rclone-config/{SESSION_HEX}"),
        ]
    );
    assert_eq!(
        session.commands(),
        [
            sh("command -v rclone >/dev/null 2>&1 || test -x /usr/local/bin/rclone"),
            strings(&["chmod", "0600", &config_path]),
            strings(&[
                "rclone",
                "mount",
                "remote:bucket",
                "/workspace/mnt",
                "--read-only",
                "--config",
                &config_path,
                "--daemon",
            ]),
        ]
    );
}

#[tokio::test]
async fn an_rclone_nfs_server_is_read_only_when_the_mount_is() {
    let session = Recorder::new();
    let config_path = format!("/workspace/.sandbox-rclone-config/{SESSION_HEX}/remote.conf");

    apply_pattern(
        &MountPattern::Rclone(RcloneOptions {
            mode: RcloneMode::Nfs,
            ..RcloneOptions::default()
        }),
        &session,
        &PosixPath::new("/workspace/mnt"),
        &MountPatternConfig::Rclone(rclone_runtime(true)),
    )
    .await
    .expect("mounts");

    let commands = session.commands();
    let server = commands
        .iter()
        .position(|command| {
            *command
                == sh("/usr/local/bin/rclone serve nfs --help >/dev/null 2>&1 || rclone serve nfs --help >/dev/null 2>&1")
        })
        .expect("the server's tool is checked");
    assert_eq!(
        commands[server + 1],
        sh(&format!(
            "rclone serve nfs remote:bucket --addr 127.0.0.1:2049 --config {config_path} --read-only &"
        ))
    );
}

// --- blobfuse commands -------------------------------------------------------------------------

fn blobfuse_runtime(account_key: Option<&str>) -> FuseMountConfig {
    FuseMountConfig {
        account: "acct".to_owned(),
        container: "container".to_owned(),
        endpoint: None,
        identity_client_id: None,
        account_key: account_key.map(str::to_owned),
        mount_type: "azure_blob_mount".to_owned(),
        read_only: true,
    }
}

async fn apply_blobfuse(session: &Recorder, options: FuseOptions, at: &str) -> SandboxResult<()> {
    apply_pattern(
        &MountPattern::Fuse(options),
        session,
        &PosixPath::new(at),
        &MountPatternConfig::Fuse(blobfuse_runtime(Some("secret"))),
    )
    .await
}

fn cache_dir() -> String {
    format!("/workspace/.sandbox-blobfuse-cache/{SESSION_HEX}/acct/container")
}

#[tokio::test]
async fn a_generated_blobfuse_configuration_is_its_own_owner_only_file() {
    let session = Recorder::new();
    let config_path =
        format!("/workspace/.sandbox-blobfuse-config/{SESSION_HEX}/acct_container.yaml");

    apply_blobfuse(&session, FuseOptions::default(), "/workspace/mnt")
        .await
        .expect("mounts");

    let expected = format!(
        "allow-other: true\n\nlogging:\n  type: syslog\n  level: log_debug\n\ncomponents:\n  - libfuse\n\
         \x20 - block_cache\n  - attr_cache\n  - azstorage\n\nblock_cache:\n  block-size-mb: 16\n\
         \x20 mem-size-mb: 50000\n  path: {}\n  disk-size-mb: 50000\n  disk-timeout-sec: 3600\n\n\
         attr_cache:\n  timeout-sec: 7200\n\nazstorage:\n  type: block\n  account-name: acct\n\
         \x20 container: container\n  endpoint: https://acct.blob.core.windows.net\n  auth-type: key\n\
         \x20 account-key: secret\n",
        cache_dir()
    );
    assert_eq!(
        session.writes(),
        [(config_path.clone(), expected.into_bytes())]
    );
    assert_eq!(
        session.mkdirs(),
        [
            "/workspace/mnt".to_owned(),
            cache_dir(),
            format!("/workspace/.sandbox-blobfuse-config/{SESSION_HEX}"),
        ]
    );
    assert_eq!(
        session.commands(),
        [
            sh("command -v blobfuse2 >/dev/null 2>&1"),
            strings(&["chmod", "0600", &config_path]),
            strings(&[
                "blobfuse2",
                "mount",
                "--read-only",
                "--config-file",
                &config_path,
                "/workspace/mnt",
            ]),
        ]
    );
}

#[tokio::test]
async fn a_zero_attribute_cache_timeout_is_kept_as_zero() {
    let session = Recorder::new();

    apply_blobfuse(
        &session,
        FuseOptions {
            attr_cache_timeout_sec: Some(0),
            ..FuseOptions::default()
        },
        "/workspace/mnt",
    )
    .await
    .expect("mounts");

    let written = String::from_utf8(session.writes()[0].1.clone()).expect("utf-8");
    assert!(
        written.contains("attr_cache:\n  timeout-sec: 0\n"),
        "{written}"
    );
}

#[tokio::test]
async fn a_zero_cache_size_takes_the_default_for_its_cache_type() {
    let cases = [
        (
            FuseOptions {
                cache_size_mb: Some(0),
                ..FuseOptions::default()
            },
            "block_cache:\n  block-size-mb: 16\n  mem-size-mb: 50000\n".to_owned(),
        ),
        (
            FuseOptions {
                cache_type: FuseCacheType::FileCache,
                cache_size_mb: Some(0),
                file_cache_max_size_mb: Some(0),
                ..FuseOptions::default()
            },
            format!(
                "file_cache:\n  path: {}\n  timeout-sec: 120\n  max-size-mb: 4096\n",
                cache_dir()
            ),
        ),
    ];
    for (options, expected) in cases {
        let session = Recorder::new();

        apply_blobfuse(&session, options, "/workspace/mnt")
            .await
            .expect("mounts");

        let written = String::from_utf8(session.writes()[0].1.clone()).expect("utf-8");
        assert!(written.contains(&expected), "{written}");
    }
}

#[tokio::test]
async fn a_blobfuse_cache_path_is_relative_to_the_workspace() {
    for (declared, reported) in [
        ("/tmp/blobfuse-cache", "/tmp/blobfuse-cache"),
        ("../blobfuse-cache", "../blobfuse-cache"),
        ("C:\\blobfuse-cache", "C:/blobfuse-cache"),
    ] {
        let options = FuseOptions {
            cache_path: Some(declared.to_owned()),
            ..FuseOptions::default()
        };
        let error = options.checked_cache_path().expect_err("not relative");
        assert_eq!(
            error.to_string(),
            "blobfuse cache_path must be relative to the workspace root"
        );
        assert_eq!(error.context().get("cache_path"), Some(&json!(reported)));

        // Applied anyway, it is refused before anything runs.
        let session = Recorder::new();
        let error = apply_blobfuse(&session, options, "/workspace/mnt")
            .await
            .expect_err("refused");
        assert_eq!(error.error_code(), ErrorCode::MountConfigInvalid);
        assert!(session.commands().is_empty());
    }
}

#[tokio::test]
async fn a_blobfuse_cache_inside_the_mount_is_refused_before_anything_is_written() {
    let session = Recorder::new();

    let error = apply_pattern(
        &MountPattern::Fuse(FuseOptions::default()),
        &session,
        &PosixPath::new("/workspace"),
        &MountPatternConfig::Fuse(blobfuse_runtime(None)),
    )
    .await
    .expect_err("the cache would be inside the mount");

    assert_eq!(
        error.to_string(),
        "blobfuse cache_path must be outside the mount path"
    );
    assert_eq!(
        error.context().get("mount_path"),
        Some(&json!("/workspace"))
    );
    assert_eq!(error.context().get("cache_path"), Some(&json!(cache_dir())));
    assert_eq!(
        session.commands(),
        [sh("command -v blobfuse2 >/dev/null 2>&1")]
    );
    assert!(session.writes().is_empty());
}

// --- a configuration built for another pattern, missing tools, and the lifecycle boundary -------

#[tokio::test]
async fn a_configuration_built_for_another_pattern_is_refused() {
    let error = apply_pattern(
        &rclone(),
        &Recorder::new(),
        &PosixPath::new("/workspace/mnt"),
        &MountPatternConfig::Fuse(blobfuse_runtime(None)),
    )
    .await
    .expect_err("wrong configuration");

    assert_eq!(
        error.to_string(),
        "mount pattern received incompatible runtime config"
    );
    assert_eq!(
        error.context().get("expected"),
        Some(&json!("RcloneMountConfig"))
    );
    assert_eq!(
        error.context().get("actual"),
        Some(&json!("FuseMountConfig"))
    );
}

#[tokio::test]
async fn a_failed_rclone_command_keeps_its_details_when_nothing_about_the_mount_is_secret() {
    let session = Recorder {
        failing: Some("rclone mount"),
        failing_stderr: "mount helper failed",
        ..Recorder::new()
    };
    let mount = in_container(
        MountProvider::S3(S3Mount {
            bucket: "bucket".to_owned(),
            ..S3Mount::default()
        }),
        rclone(),
    );

    let error = BuiltinMountLifecycle
        .apply(
            &mount,
            &session,
            &PosixPath::new("/workspace/data"),
            std::path::Path::new("/"),
        )
        .await
        .expect_err("the mount command failed");

    assert_eq!(error.error_code(), ErrorCode::MountFailed);
    assert_eq!(
        error.context().get("stderr"),
        Some(&json!("mount helper failed"))
    );
    assert_eq!(error.context().get("type"), Some(&json!("s3_mount")));
}

#[tokio::test]
async fn a_failure_of_a_mount_that_carries_credentials_is_replaced_at_the_boundary() {
    let session = Recorder {
        failing: Some("rclone mount"),
        failing_stderr: "denied for secret-key",
        ..Recorder::new()
    };
    let mount = in_container(
        MountProvider::S3(S3Mount {
            bucket: "bucket".to_owned(),
            access_key_id: Some("access-key".to_owned()),
            secret_access_key: Some("secret-key".to_owned()),
            ..S3Mount::default()
        }),
        rclone(),
    );
    // The credentials reach a helper inside the sandbox, so the path has to be acknowledged.
    let session = Recorder {
        failing: session.failing,
        failing_stderr: session.failing_stderr,
        ..Recorder::with_manifest(
            Manifest::new()
                .with_entry("data", ra_core::sandbox::Entry::mount(mount.clone()))
                .with_in_container_mount_credential_exposure_acknowledged(&["data"])
                .expect("acknowledged"),
        )
    };

    let error = BuiltinMountLifecycle
        .activate(
            &mount,
            mount.strategy(),
            &session,
            &PosixPath::new("/workspace/data"),
            std::path::Path::new("/"),
        )
        .await
        .expect_err("the mount command failed");

    // The code survives for a caller to branch on; the command, its output and the cause do not.
    assert_eq!(error.error_code(), ErrorCode::MountFailed);
    assert_eq!(
        error.to_string(),
        "sandbox operation failed while using a protected mount configuration"
    );
    assert!(error.context().is_empty());
    assert!(std::error::Error::source(&error).is_none());
    assert!(!format!("{error:?}").contains("secret-key"));
}

// --- volume driver configuration -----------------------------------------------------------------

#[test]
fn a_volume_driver_is_given_the_providers_options_with_the_strategys_own_on_top() {
    let mount = Mount::new(
        MountProvider::S3(S3Mount {
            bucket: "bucket".to_owned(),
            prefix: Some("/nested".to_owned()),
            region: Some("us-east-1".to_owned()),
            ..S3Mount::default()
        }),
        MountStrategy::DockerVolume {
            driver: "rclone".to_owned(),
            driver_options: BTreeMap::from([
                ("s3-region".to_owned(), "eu-west-1".to_owned()),
                ("vfs-cache-mode".to_owned(), "off".to_owned()),
            ]),
        },
    )
    .expect("supported");

    let config = docker_volume_driver_config(&mount, mount.strategy())
        .expect("config")
        .expect("a volume strategy");

    assert_eq!(config.driver, "rclone");
    assert!(config.read_only);
    assert_eq!(
        config.options,
        BTreeMap::from([
            ("path".to_owned(), "bucket/nested".to_owned()),
            ("s3-provider".to_owned(), "AWS".to_owned()),
            ("s3-region".to_owned(), "eu-west-1".to_owned()),
            ("type".to_owned(), "s3".to_owned()),
            ("vfs-cache-mode".to_owned(), "off".to_owned()),
        ])
    );
}

#[test]
fn a_mountpoint_volume_and_an_in_container_strategy_ask_for_different_things() {
    let gcs = Mount::new(
        MountProvider::Gcs(GcsMount {
            bucket: "bucket".to_owned(),
            ..GcsMount::default()
        }),
        MountStrategy::docker_volume("mountpoint"),
    )
    .expect("supported");
    let config = docker_volume_driver_config(&gcs, gcs.strategy())
        .expect("config")
        .expect("a volume strategy");
    assert_eq!(
        config.options,
        BTreeMap::from([
            ("bucket".to_owned(), "bucket".to_owned()),
            (
                "endpoint_url".to_owned(),
                "https://storage.googleapis.com".to_owned()
            ),
        ])
    );

    // The sandbox attaches an in-container mount itself, so there is nothing to hand a driver.
    let in_container = in_container(
        MountProvider::Gcs(GcsMount {
            bucket: "bucket".to_owned(),
            ..GcsMount::default()
        }),
        rclone(),
    );
    assert!(
        docker_volume_driver_config(&in_container, in_container.strategy())
            .expect("no driver")
            .is_none()
    );
}

#[test]
fn a_box_volume_prefixes_every_option_with_the_backend_name() {
    let mount = Mount::new(
        MountProvider::Box(BoxMount {
            path: Some("/Shared".to_owned()),
            box_config_file: Some("/etc/box.json".to_owned()),
            box_sub_type: ra_core::sandbox::BoxSubType::Enterprise,
            ..BoxMount::default()
        }),
        MountStrategy::docker_volume("rclone"),
    )
    .expect("supported");

    let config = docker_volume_driver_config(&mount, mount.strategy())
        .expect("config")
        .expect("a volume strategy");

    assert_eq!(
        config.options,
        BTreeMap::from([
            ("box-box-config-file".to_owned(), "/etc/box.json".to_owned()),
            ("box-box-sub-type".to_owned(), "enterprise".to_owned()),
            ("path".to_owned(), "Shared".to_owned()),
            ("type".to_owned(), "box".to_owned()),
        ])
    );
}

#[tokio::test]
async fn gcs_endpoint_fallback_skips_empty_values_in_both_layers() {
    for (provider, pattern_endpoint, expected) in [
        (None, Some(""), "https://storage.googleapis.com"),
        (Some(""), Some(""), "https://storage.googleapis.com"),
        (Some(""), None, "https://storage.googleapis.com"),
        (
            Some(""),
            Some("https://pattern.test"),
            "https://pattern.test",
        ),
        (
            Some("https://provider.test"),
            Some("https://pattern.test"),
            "https://provider.test",
        ),
    ] {
        let pattern = MountPattern::Mountpoint(MountpointOptions {
            endpoint_url: pattern_endpoint.map(str::to_owned),
            ..MountpointOptions::default()
        });
        let mount = in_container(
            MountProvider::Gcs(GcsMount {
                bucket: "bucket".to_owned(),
                endpoint_url: provider.map(str::to_owned),
                ..GcsMount::default()
            }),
            pattern.clone(),
        );
        let config = mountpoint_config(
            build_in_container_mount_config(&mount, &pattern, &Recorder::new(), false)
                .await
                .expect("config"),
        );
        assert_eq!(config.endpoint_url.as_deref(), Some(expected));
    }
}

#[tokio::test]
async fn tool_probes_allow_the_local_backend_to_select_a_non_login_shell() {
    for (provider, pattern, tool) in [
        (
            MountProvider::S3(S3Mount {
                bucket: "bucket".to_owned(),
                ..S3Mount::default()
            }),
            MountPattern::Mountpoint(MountpointOptions::default()),
            "mount-s3",
        ),
        (
            MountProvider::AzureBlob(AzureBlobMount {
                account: "account".to_owned(),
                container: "container".to_owned(),
                ..AzureBlobMount::default()
            }),
            MountPattern::Fuse(FuseOptions::default()),
            "blobfuse2",
        ),
        (
            MountProvider::S3Files(S3FilesMount {
                file_system_id: "fs-example".to_owned(),
                ..S3FilesMount::default()
            }),
            MountPattern::S3Files(S3FilesOptions::default()),
            "mount.s3files",
        ),
    ] {
        let session = Recorder::new();
        let mount = in_container(provider, pattern.clone());
        let config = build_in_container_mount_config(&mount, &pattern, &session, true)
            .await
            .expect("config");
        apply_pattern(
            &pattern,
            &session,
            &PosixPath::new("/workspace/remote"),
            &config,
        )
        .await
        .expect("apply");
        let requests = session.requests.lock().expect("requests");
        let probe = &requests[0];
        assert!(matches!(probe.shell, ShellInvocation::Login));
        assert_eq!(
            ra_sandbox::unix_local::prepare_exec_command(probe),
            vec![
                "sh".to_owned(),
                "-c".to_owned(),
                format!("command -v {tool} >/dev/null 2>&1")
            ]
        );
    }
}
