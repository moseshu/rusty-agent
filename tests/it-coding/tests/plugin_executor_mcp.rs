// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
use ra_coding::config::McpServerConfig;
use ra_coding::config::McpServerTransportConfig;
use ra_coding::plugins::PluginMcpProviderError;
use ra_coding::plugins::ResolvedPlugin;
use ra_coding::plugins::executor_mcp_test_api::DEFAULT_MCP_CONFIG_FILE;
use ra_coding::plugins::executor_mcp_test_api::load_from_file_system;
use ra_coding::plugins::model::PluginManifest;
use ra_coding::plugins::model::PluginManifestMcpServers;
use ra_coding::plugins::model::PluginManifestPaths;
use ra_executor::CopyOptions;
use ra_executor::CreateDirectoryOptions;
use ra_executor::ExecutorFileSystem;
use ra_executor::ExecutorFileSystemFuture;
use ra_executor::FileMetadata;
use ra_executor::FileSystemReadStream;
use ra_executor::FileSystemResult;
use ra_executor::FileSystemSandboxContext;
use ra_executor::GetMetadataOptions;
use ra_executor::ReadDirectoryEntry;
use ra_executor::ReadFileOptions;
use ra_executor::RemoveOptions;
use ra_executor::WalkOptions;
use ra_executor::WalkOutcome;
use ra_executor::WriteFileOptions;
use ra_paths::AbsolutePathBuf;
use ra_paths::LegacyAppPathString;
use ra_paths::PathUri;
use std::collections::HashMap;
use std::io;
use std::sync::Mutex;

const MCP_CONFIG_CONTENTS: &str = r#"{
  "mcpServers": {
    "demo": {"command": "demo-mcp", "environment_id": "local"},
    "hosted": {"url": "https://example.com/mcp"}
  }
}"#;

struct SyntheticExecutorFileSystem {
    config_path: AbsolutePathBuf,
    config_contents: Option<&'static str>,
    reads: Mutex<Vec<AbsolutePathBuf>>,
}

impl SyntheticExecutorFileSystem {
    fn unsupported<T>() -> FileSystemResult<T> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "operation is not used by plugin MCP provider tests",
        ))
    }
}

impl ExecutorFileSystem for SyntheticExecutorFileSystem {
    fn canonicalize<'a>(
        &'a self,
        _path: &'a PathUri,
        _sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, PathUri> {
        Box::pin(async { Self::unsupported() })
    }

    fn read_file<'a>(
        &'a self,
        path: &'a PathUri,
        _options: ReadFileOptions,
        _sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, Vec<u8>> {
        Box::pin(async move {
            let path = path.to_abs_path()?;
            self.reads
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(path.clone());
            if path != self.config_path {
                return Err(io::Error::new(io::ErrorKind::NotFound, "not found"));
            }
            self.config_contents
                .map(|contents| contents.as_bytes().to_vec())
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "not found"))
        })
    }

    fn read_file_stream<'a>(
        &'a self,
        _path: &'a PathUri,
        _sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, FileSystemReadStream> {
        Box::pin(async { Self::unsupported() })
    }

    fn write_file<'a>(
        &'a self,
        _path: &'a PathUri,
        _contents: Vec<u8>,
        _options: WriteFileOptions,
        _sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        Box::pin(async { Self::unsupported() })
    }

    fn create_directory<'a>(
        &'a self,
        _path: &'a PathUri,
        _options: CreateDirectoryOptions,
        _sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        Box::pin(async { Self::unsupported() })
    }

    fn get_metadata<'a>(
        &'a self,
        _path: &'a PathUri,
        _options: GetMetadataOptions,
        _sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, FileMetadata> {
        Box::pin(async { Self::unsupported() })
    }

    fn read_directory<'a>(
        &'a self,
        _path: &'a PathUri,
        _sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, Vec<ReadDirectoryEntry>> {
        Box::pin(async { Self::unsupported() })
    }

    fn walk<'a>(
        &'a self,
        _path: &'a PathUri,
        _options: WalkOptions,
        _sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, WalkOutcome> {
        Box::pin(async { Self::unsupported() })
    }

    fn remove<'a>(
        &'a self,
        _path: &'a PathUri,
        _options: RemoveOptions,
        _sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        Box::pin(async { Self::unsupported() })
    }

    fn copy<'a>(
        &'a self,
        _source_path: &'a PathUri,
        _destination_path: &'a PathUri,
        _options: CopyOptions,
        _sandbox: Option<&'a FileSystemSandboxContext>,
    ) -> ExecutorFileSystemFuture<'a, ()> {
        Box::pin(async { Self::unsupported() })
    }
}

#[tokio::test]
async fn reads_declared_config_only_through_executor_file_system() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let plugin_root =
        AbsolutePathBuf::from_absolute_path_checked(temp_dir.path().join("executor-only-plugin"))
            .expect("absolute plugin root");
    assert!(!plugin_root.as_path().exists());
    let config_path = plugin_root.join("config/mcp.json");
    let plugin = resolved_plugin(
        &plugin_root,
        Some(PluginManifestMcpServers::Path(config_path.clone())),
    );
    let file_system = SyntheticExecutorFileSystem {
        config_path: config_path.clone(),
        config_contents: Some(MCP_CONFIG_CONTENTS),
        reads: Mutex::new(Vec::new()),
    };

    let plugin_root_uri = PathUri::from_abs_path(&plugin_root);
    let servers = load_from_file_system(&plugin, &plugin_root_uri, &file_system)
        .await
        .expect("load plugin MCP config");

    assert_eq!(
        servers,
        vec![
            (
                "demo".to_string(),
                McpServerConfig {
                    auth: Default::default(),
                    transport: McpServerTransportConfig::Stdio {
                        command: "demo-mcp".to_string(),
                        args: Vec::new(),
                        env: None,
                        env_vars: Vec::new(),
                        cwd: Some(LegacyAppPathString::from_path(plugin_root.as_path())),
                    },
                    environment_id: "executor-test".to_string(),
                    enabled: true,
                    required: false,
                    startup_readiness: Default::default(),
                    supports_parallel_tool_calls: false,
                    tool_input_schema_max_bytes: None,
                    omit_tools_from: None,
                    disabled_reason: None,
                    startup_timeout_sec: None,
                    tool_timeout_sec: None,
                    default_tools_approval_mode: None,
                    enabled_tools: None,
                    disabled_tools: None,
                    scopes: None,
                    oauth: None,
                    oauth_resource: None,
                    tools: HashMap::new(),
                },
            ),
            (
                "hosted".to_string(),
                McpServerConfig {
                    auth: Default::default(),
                    transport: McpServerTransportConfig::StreamableHttp {
                        url: "https://example.com/mcp".to_string(),
                        bearer_token_env_var: None,
                        http_headers: None,
                        env_http_headers: None,
                        http_headers_helper: None,
                    },
                    environment_id: "executor-test".to_string(),
                    enabled: true,
                    required: false,
                    startup_readiness: Default::default(),
                    supports_parallel_tool_calls: false,
                    tool_input_schema_max_bytes: None,
                    omit_tools_from: None,
                    disabled_reason: None,
                    startup_timeout_sec: None,
                    tool_timeout_sec: None,
                    default_tools_approval_mode: None,
                    enabled_tools: None,
                    disabled_tools: None,
                    scopes: None,
                    oauth: None,
                    oauth_resource: None,
                    tools: HashMap::new(),
                },
            ),
        ]
    );
    assert_eq!(reads(&file_system), vec![config_path]);
}

#[tokio::test]
async fn reads_manifest_object_config_without_executor_file_system_access() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let plugin_root = AbsolutePathBuf::from_absolute_path_checked(temp_dir.path().join("plugin"))
        .expect("absolute plugin root");
    let config_path = plugin_root.join(DEFAULT_MCP_CONFIG_FILE);
    let plugin = resolved_plugin(
        &plugin_root,
        Some(PluginManifestMcpServers::Object(
            r#"{"counter":{"command":"counter-mcp","environment_id":"local"}}"#.to_string(),
        )),
    );
    let file_system = SyntheticExecutorFileSystem {
        config_path,
        config_contents: None,
        reads: Mutex::new(Vec::new()),
    };

    let plugin_root_uri = PathUri::from_abs_path(&plugin_root);
    let servers = load_from_file_system(&plugin, &plugin_root_uri, &file_system)
        .await
        .expect("load manifest object plugin MCP config");

    assert_eq!(
        servers,
        vec![(
            "counter".to_string(),
            McpServerConfig {
                auth: Default::default(),
                transport: McpServerTransportConfig::Stdio {
                    command: "counter-mcp".to_string(),
                    args: Vec::new(),
                    env: None,
                    env_vars: Vec::new(),
                    cwd: Some(LegacyAppPathString::from_path(plugin_root.as_path())),
                },
                environment_id: "executor-test".to_string(),
                enabled: true,
                required: false,
                startup_readiness: Default::default(),
                supports_parallel_tool_calls: false,
                tool_input_schema_max_bytes: None,
                omit_tools_from: None,
                disabled_reason: None,
                startup_timeout_sec: None,
                tool_timeout_sec: None,
                default_tools_approval_mode: None,
                enabled_tools: None,
                disabled_tools: None,
                scopes: None,
                oauth: None,
                oauth_resource: None,
                tools: HashMap::new(),
            },
        )]
    );
    assert_eq!(reads(&file_system), Vec::new());
}

#[tokio::test]
async fn missing_default_config_is_empty() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let plugin_root = AbsolutePathBuf::from_absolute_path_checked(temp_dir.path().join("plugin"))
        .expect("absolute plugin root");
    let config_path = plugin_root.join(DEFAULT_MCP_CONFIG_FILE);
    let plugin = resolved_plugin(&plugin_root, /*mcp_servers*/ None);
    let file_system = SyntheticExecutorFileSystem {
        config_path: config_path.clone(),
        config_contents: None,
        reads: Mutex::new(Vec::new()),
    };

    let plugin_root_uri = PathUri::from_abs_path(&plugin_root);
    let servers = load_from_file_system(&plugin, &plugin_root_uri, &file_system)
        .await
        .expect("missing default config should be ignored");

    assert_eq!(servers, Vec::new());
    assert_eq!(reads(&file_system), vec![config_path]);
}

#[tokio::test]
async fn malformed_declared_config_is_an_error() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let plugin_root = AbsolutePathBuf::from_absolute_path_checked(temp_dir.path().join("plugin"))
        .expect("absolute plugin root");
    let config_path = plugin_root.join("mcp.json");
    let plugin = resolved_plugin(
        &plugin_root,
        Some(PluginManifestMcpServers::Path(config_path.clone())),
    );
    let file_system = SyntheticExecutorFileSystem {
        config_path: config_path.clone(),
        config_contents: Some("{not-json"),
        reads: Mutex::new(Vec::new()),
    };

    let plugin_root_uri = PathUri::from_abs_path(&plugin_root);
    let err = load_from_file_system(&plugin, &plugin_root_uri, &file_system)
        .await
        .expect_err("malformed declared config should fail");

    let PluginMcpProviderError::ParseConfig {
        plugin_id,
        path,
        source: _,
    } = err
    else {
        panic!("expected parse error");
    };
    assert_eq!(
        (plugin_id, path),
        (
            "selected-root".to_string(),
            PathUri::from_abs_path(&config_path)
        )
    );
    assert_eq!(reads(&file_system), vec![config_path]);
}

#[tokio::test]
async fn malformed_manifest_object_config_reports_actual_manifest_path() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let plugin_root = AbsolutePathBuf::from_absolute_path_checked(temp_dir.path().join("plugin"))
        .expect("absolute plugin root");
    let plugin = resolved_plugin(
        &plugin_root,
        Some(PluginManifestMcpServers::Object("{not-json".to_string())),
    );
    let file_system = SyntheticExecutorFileSystem {
        config_path: plugin_root.join(DEFAULT_MCP_CONFIG_FILE),
        config_contents: None,
        reads: Mutex::new(Vec::new()),
    };

    let plugin_root_uri = PathUri::from_abs_path(&plugin_root);
    let err = load_from_file_system(&plugin, &plugin_root_uri, &file_system)
        .await
        .expect_err("malformed manifest object config should fail");

    let PluginMcpProviderError::ParseConfig {
        plugin_id,
        path,
        source: _,
    } = err
    else {
        panic!("expected parse error");
    };
    assert_eq!(
        (plugin_id, path),
        (
            "selected-root".to_string(),
            PathUri::from_abs_path(&plugin_root.join(".claude-plugin/plugin.json"))
        )
    );
    assert_eq!(reads(&file_system), Vec::new());
}

fn resolved_plugin(
    plugin_root: &AbsolutePathBuf,
    mcp_servers: Option<PluginManifestMcpServers<AbsolutePathBuf>>,
) -> ResolvedPlugin {
    let plugin_root_uri = PathUri::from_abs_path(plugin_root);
    let mcp_servers = mcp_servers.map(|mcp_servers| match mcp_servers {
        PluginManifestMcpServers::Path(path) => {
            PluginManifestMcpServers::Path(PathUri::from_abs_path(&path))
        }
        PluginManifestMcpServers::Object(config) => PluginManifestMcpServers::Object(config),
    });
    ResolvedPlugin::from_environment(
        "selected-root".to_string(),
        "executor-test".to_string(),
        plugin_root_uri.clone(),
        plugin_root_uri
            .join(".claude-plugin/plugin.json")
            .expect("manifest URI"),
        PluginManifest {
            name: "demo-plugin".to_string(),
            version: None,
            description: None,
            keywords: Vec::new(),
            paths: PluginManifestPaths {
                onboarding_skill: None,
                skills: Vec::new(),
                mcp_servers,
                apps: None,
                hooks: None,
            },
            interface: None,
        },
    )
    .expect("valid plugin descriptor")
}

fn reads(file_system: &SyntheticExecutorFileSystem) -> Vec<AbsolutePathBuf> {
    file_system
        .reads
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}
