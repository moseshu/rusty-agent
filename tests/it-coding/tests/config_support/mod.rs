// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Absolute path fixtures shared by ported configuration tests.
#![allow(
    dead_code,
    reason = "Shared test helpers support multiple configuration targets."
)]
use it_config::support::CloudConfigBundleFixture;
use ra_coding::config::{Config, ConfigBuilder, ConfigOverrides};
use ra_config::{
    CONFIG_TOML_FILE, ConfigToml, PermissionProfileToml, PermissionsToml, ProjectConfig, TrustLevel,
};
use ra_paths::AbsolutePathBuf;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tempfile::TempDir;
pub trait AbsoluteTestPath {
    fn abs(&self) -> AbsolutePathBuf;
}
impl AbsoluteTestPath for tempfile::TempDir {
    fn abs(&self) -> AbsolutePathBuf {
        self.path().abs()
    }
}
impl AbsoluteTestPath for Path {
    fn abs(&self) -> AbsolutePathBuf {
        AbsolutePathBuf::from_absolute_path(self).expect("absolute test path")
    }
}
impl AbsoluteTestPath for PathBuf {
    fn abs(&self) -> AbsolutePathBuf {
        self.as_path().abs()
    }
}
pub fn test_absolute_path(path: &str) -> AbsolutePathBuf {
    if cfg!(windows) && path.starts_with('/') {
        AbsolutePathBuf::from_absolute_path(format!("C:{path}")).expect("absolute Windows fixture")
    } else {
        AbsolutePathBuf::from_absolute_path(path).expect("absolute fixture")
    }
}

pub fn config_error_from_io(err: &std::io::Error) -> &ra_config::ConfigError {
    err.get_ref()
        .and_then(|err| err.downcast_ref::<ra_config::ConfigLoadError>())
        .map(ra_config::ConfigLoadError::config_error)
        .expect("ConfigLoadError")
}

pub async fn make_config_for_test(
    codex_home: &Path,
    project_path: &Path,
    trust_level: TrustLevel,
    project_root_markers: Option<Vec<String>>,
) -> std::io::Result<()> {
    tokio::fs::write(
        codex_home.join(CONFIG_TOML_FILE),
        toml::to_string(&ConfigToml {
            projects: Some(HashMap::from([(
                project_path.to_string_lossy().to_string(),
                ProjectConfig {
                    trust_level: Some(trust_level),
                },
            )])),
            project_root_markers,
            ..Default::default()
        })
        .expect("serialize config"),
    )
    .await
}
pub async fn load_workspace_permission_profile(
    profile: PermissionProfileToml,
) -> std::io::Result<Config> {
    let codex_home = TempDir::new()?;
    let cwd = TempDir::new()?;
    std::fs::write(cwd.path().join(".git"), "gitdir: nowhere")?;

    Config::load_from_base_config_with_overrides(
        ConfigToml {
            default_permissions: Some("dev".to_string()),
            permissions: Some(PermissionsToml {
                entries: BTreeMap::from([("dev".to_string(), profile)]),
            }),
            ..Default::default()
        },
        ConfigOverrides {
            cwd: Some(cwd.path().to_path_buf()),
            ..Default::default()
        },
        codex_home.abs(),
    )
    .await
}
pub async fn load_with_enterprise_requirement(
    codex_home: &TempDir,
    requirements: impl Into<String>,
) -> std::io::Result<Config> {
    ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(codex_home.path().to_path_buf()))
        .cloud_config_bundle(
            CloudConfigBundleFixture::loader_with_enterprise_requirement(requirements),
        )
        .build()
        .await
}
pub struct PrecedenceTestFixture {
    pub cwd: TempDir,
    pub codex_home: TempDir,
    pub cfg: ConfigToml,
}

impl PrecedenceTestFixture {
    pub fn cwd_path(&self) -> PathBuf {
        self.cwd.path().to_path_buf()
    }

    pub fn codex_home(&self) -> AbsolutePathBuf {
        self.codex_home.abs()
    }
}

pub fn create_test_fixture() -> std::io::Result<PrecedenceTestFixture> {
    let toml = r#"
model = "o3"
approval_policy = "on-request"

[analytics]
enabled = true

[model_providers.openai-custom]
name = "OpenAI custom"
base_url = "https://api.openai.com/v1"
env_key = "OPENAI_API_KEY"
wire_api = "responses"
request_max_retries = 4            # retry failed HTTP requests
stream_max_retries = 10            # retry dropped SSE streams
stream_idle_timeout_ms = 300000    # 5m idle timeout
websocket_connect_timeout_ms = 15000

[profiles.o3]
model = "o3"
model_provider = "openai"
approval_policy = "never"
model_reasoning_effort = "high"
model_reasoning_summary = "detailed"

[profiles.gpt3]
model = "gpt-3.5-turbo"
model_provider = "openai-custom"

[profiles.zdr]
model = "o3"
model_provider = "openai"
approval_policy = "on-request"

[profiles.zdr.analytics]
enabled = false

[profiles.gpt5]
model = "gpt-5.4"
model_provider = "openai"
approval_policy = "on-request"
model_reasoning_effort = "high"
model_reasoning_summary = "detailed"
model_verbosity = "high"
"#;

    let cfg: ConfigToml = toml::from_str(toml).expect("TOML deserialization should succeed");

    // Use a temporary directory for the cwd so it does not contain an
    // AGENTS.md file.
    let cwd_temp_dir = TempDir::new().unwrap();
    let cwd = cwd_temp_dir.path().to_path_buf();
    // Make it look like a Git repo so it does not search for AGENTS.md in
    // a parent folder, either.
    std::fs::write(cwd.join(".git"), "gitdir: nowhere")?;

    let codex_home_temp_dir = TempDir::new().unwrap();

    Ok(PrecedenceTestFixture {
        cwd: cwd_temp_dir,
        codex_home: codex_home_temp_dir,
        cfg,
    })
}
pub fn bundled_models_response() -> std::io::Result<ra_coding::config::ModelsResponse> {
    serde_json::from_str(catalog::CATALOG).map_err(std::io::Error::other)
}

mod catalog;
