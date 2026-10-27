//! Selected plugins retain executor authority across discovery, resources, policy and hook admission.
use base64::{Engine as _, engine::general_purpose::STANDARD};
use futures::{SinkExt, StreamExt};
use ra_coding::{
    config::{AppToolApproval, McpServerTransportConfig, PluginMcpServerConfig, PluginsConfigToml},
    plugins::{
        ExecutorPluginProvider, ExecutorPluginProviderError, PluginMcpProvider,
        PluginMcpProviderError, PluginResourceLocator, PluginRootOwnership,
        apply_configured_plugin_mcp_server_policies, executor_plugin_hook_sources,
    },
};
use ra_executor::{
    CapabilityRootsDiscoverParams, EnvironmentManager, ExecutorCapabilityDiscoveryCache,
    ExecutorFileSystem, GetMetadataOptions, LocalFileSystem, ReadFileOptions,
    capabilities::{CapabilityRootLocation, SelectedCapabilityRoot},
    discover_capability_roots,
};
use ra_http::{HttpClientFactory, OutboundProxyPolicy};
use ra_mcp::catalog::McpToolInfo;
use ra_paths::{PathConvention, PathUri};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    io,
    sync::{Arc, Mutex},
    time::Duration,
};
use tempfile::TempDir;
use tokio::sync::oneshot;
use tokio_util::task::AbortOnDropHandle;

const ENVIRONMENT: &str = "executor";
const CLEANUP_PLUGIN: &str = "computer-use@openai-bundled";
const BROWSER_PLUGIN: &str = "browser@openai-curated-remote";

type Requests = Arc<Mutex<Vec<(String, Value)>>>;

struct Backend {
    disk: TempDir,
    native_base: PathUri,
    remote_base: PathUri,
    filesystem: LocalFileSystem,
    requests: Requests,
}

impl Backend {
    fn native_uri(&self, path: &PathUri) -> io::Result<PathUri> {
        rebase(path, &self.remote_base, &self.native_base)
    }

    async fn request(&self, method: &str, params: Value) -> io::Result<Value> {
        self.requests
            .lock()
            .unwrap()
            .push((method.to_string(), params.clone()));
        if method.starts_with("fs/") && !params["sandbox"].is_null() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "fixture sandbox is unavailable",
            ));
        }
        match method {
            "initialize" => Ok(json!({
                "sessionId": "plugin-flow",
                "environmentInfo": {"shell": {"name": "sh", "path": "/bin/sh"}}
            })),
            "capabilityRoots/discoverV1" => {
                let mut params: CapabilityRootsDiscoverParams =
                    serde_json::from_value(params).map_err(io::Error::other)?;
                for root in &mut params.roots {
                    assert!(
                        root.sandbox.is_none(),
                        "this fixture does not implement native sandbox execution"
                    );
                    root.path = self.native_uri(&root.path)?;
                }
                let discovered = discover_capability_roots(&self.filesystem, params)
                    .await
                    .map_err(io::Error::other)?;
                let mut result = serde_json::to_value(discovered).map_err(io::Error::other)?;
                self.project_paths(&mut result)?;
                Ok(result)
            }
            "fs/getMetadata" => {
                let path =
                    PathUri::parse(params["path"].as_str().unwrap()).map_err(io::Error::other)?;
                let metadata = self
                    .filesystem
                    .get_metadata(
                        &self.native_uri(&path)?,
                        GetMetadataOptions {
                            follow_symlinks: params["followSymlinks"].as_bool().unwrap_or(true),
                        },
                        None,
                    )
                    .await?;
                Ok(json!({
                    "isDirectory": metadata.is_directory, "isFile": metadata.is_file,
                    "isSymlink": metadata.is_symlink, "size": metadata.size,
                    "createdAtMs": metadata.created_at_ms, "modifiedAtMs": metadata.modified_at_ms,
                }))
            }
            "fs/readFile" => {
                let path =
                    PathUri::parse(params["path"].as_str().unwrap()).map_err(io::Error::other)?;
                let bytes = self
                    .filesystem
                    .read_file(
                        &self.native_uri(&path)?,
                        ReadFileOptions {
                            follow_symlinks: params["followSymlinks"].as_bool().unwrap_or(true),
                        },
                        None,
                    )
                    .await?;
                Ok(json!({"dataBase64": STANDARD.encode(bytes)}))
            }
            other => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                other.to_string(),
            )),
        }
    }

    fn project_paths(&self, value: &mut Value) -> io::Result<()> {
        match value {
            Value::Object(fields) => {
                for (name, value) in fields {
                    if name == "path" {
                        let path =
                            PathUri::parse(value.as_str().unwrap()).map_err(io::Error::other)?;
                        *value = json!(rebase(&path, &self.native_base, &self.remote_base)?);
                    } else {
                        self.project_paths(value)?;
                    }
                }
            }
            Value::Array(values) => {
                for value in values {
                    self.project_paths(value)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
}

fn rebase(path: &PathUri, from: &PathUri, to: &PathUri) -> io::Result<PathUri> {
    if !path.starts_with(from) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path outside fixture volume",
        ));
    }
    let relative = path.relative_path_from(from).unwrap();
    let relative = if from.infer_path_convention() == Some(PathConvention::Windows) {
        relative.replace('\\', "/")
    } else {
        relative
    };
    if relative.is_empty() || relative == "." {
        Ok(to.clone())
    } else {
        to.join(&relative).map_err(io::Error::other)
    }
}

struct RemotePlugins {
    backend: Arc<Backend>,
    manager: Arc<EnvironmentManager>,
    task: AbortOnDropHandle<()>,
    stop: Option<oneshot::Sender<()>>,
}

impl RemotePlugins {
    async fn start(remote_base: PathUri) -> Self {
        let disk = tempfile::tempdir().unwrap();
        let backend = Arc::new(Backend {
            native_base: PathUri::from_host_native_path(disk.path().canonicalize().unwrap())
                .unwrap(),
            disk,
            remote_base,
            filesystem: LocalFileSystem::unsandboxed(),
            requests: Arc::new(Mutex::new(Vec::new())),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let (stop, mut stopped) = oneshot::channel();
        let served = backend.clone();
        let task = AbortOnDropHandle::new(tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            loop {
                let incoming = tokio::select! {
                    _ = &mut stopped => break,
                    incoming = socket.next() => incoming,
                };
                let Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text))) = incoming else {
                    break;
                };
                let request: Value = serde_json::from_str(&text).unwrap();
                let Some(id) = request.get("id") else {
                    continue;
                };
                let mut response = json!({"jsonrpc": "2.0", "id": id});
                match served
                    .request(
                        request["method"].as_str().unwrap(),
                        request["params"].clone(),
                    )
                    .await
                {
                    Ok(result) => response["result"] = result,
                    Err(error) => {
                        let code = match error.kind() {
                            io::ErrorKind::NotFound => -32004,
                            io::ErrorKind::InvalidInput => -32600,
                            _ => -32603,
                        };
                        response["error"] = json!({"code": code, "message": error.to_string()});
                    }
                }
                socket
                    .send(tokio_tungstenite::tungstenite::Message::text(
                        response.to_string(),
                    ))
                    .await
                    .unwrap();
            }
        }));
        let manager = Arc::new(EnvironmentManager::without_environments(
            HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
        ));
        manager
            .upsert_environment(ENVIRONMENT.into(), url, None)
            .unwrap();
        tokio::time::timeout(
            Duration::from_secs(3),
            manager
                .get_environment(ENVIRONMENT)
                .unwrap()
                .wait_until_ready(),
        )
        .await
        .unwrap()
        .unwrap();
        Self {
            backend,
            manager,
            task,
            stop: Some(stop),
        }
    }

    fn write(&self, path: &str, contents: impl AsRef<[u8]>) {
        let path = self.backend.disk.path().join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    fn root(&self, id: &str, directory: &str) -> SelectedCapabilityRoot {
        SelectedCapabilityRoot {
            id: id.to_string(),
            location: CapabilityRootLocation::Environment {
                environment_id: ENVIRONMENT.into(),
                path: self.backend.remote_base.join(directory).unwrap(),
            },
        }
    }

    fn calls(&self, method: &str) -> usize {
        self.backend
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|(name, _)| name == method)
            .count()
    }

    async fn finish(mut self) {
        self.stop.take().unwrap().send(()).unwrap();
        (&mut self.task).await.unwrap();
    }
}

fn foreign_volume() -> PathUri {
    #[cfg(windows)]
    let path = "file:///executor-volume";
    #[cfg(not(windows))]
    let path = "file:///C:/executor-volume";
    PathUri::parse(path).unwrap()
}

fn cleanup_manifest() -> Value {
    json!({
        "name": "computer-use", "skills": "./skills", "mcpServers": "./servers.json",
        "hooks": {"hooks": {"Stop": [{"hooks": [
            {"type": "mcp_tool", "server": "node_repl", "tool": "turn_ended", "input": {}},
            {"type": "command", "command": "untrusted-command"}
        ]}]}}
    })
}

fn mcp_declarations(command: &str) -> Value {
    json!({"mcpServers": {
        "node_repl": {
            "command": command, "args": ["server.py"],
            "enabled_tools": ["read", "write", "turn_ended"], "disabled_tools": ["unsafe"],
            "default_tools_approval_mode": "writes",
            "tools": {"read": {"approval_mode": "auto", "output_token_limit": 12}}
        },
        "invalid-sibling": {"command": 42}
    }})
}

fn install_cleanup(fixture: &RemotePlugins, directory: &str, command: &str) {
    fixture.write(
        &format!("{directory}/.codex-plugin/plugin.json"),
        cleanup_manifest().to_string(),
    );
    fixture.write(
        &format!("{directory}/servers.json"),
        mcp_declarations(command).to_string(),
    );
    fixture.write(
        &format!("{directory}/skills/read/SKILL.md"),
        "---\nname: read\ndescription: executor skill\n---\nexecutor-owned instructions",
    );
}

#[tokio::test]
async fn foreign_executor_discovery_resources_mcp_policy_and_cleanup_form_one_chain() {
    let fixture = RemotePlugins::start(foreign_volume()).await;
    install_cleanup(&fixture, "plugin", "executor-worker");
    let root = fixture.root(CLEANUP_PLUGIN, "plugin");
    let cache = ExecutorCapabilityDiscoveryCache::new(fixture.manager.clone());
    let snapshot = cache
        .snapshot(std::slice::from_ref(&root), &HashMap::new())
        .await;
    let discovery = snapshot.roots()[0].result.as_ref().unwrap();
    assert!(discovery.error.is_none(), "{:?}", discovery.error);
    assert_eq!(discovery.skills.len(), 1);
    let provider = ExecutorPluginProvider::new(fixture.manager.clone());
    assert_eq!(
        provider
            .root_ownership(&root, Some(discovery), None)
            .await
            .unwrap(),
        PluginRootOwnership::Plugin
    );
    let bound = provider.resolve_bound(&root).await.unwrap().unwrap();
    let PluginResourceLocator::Environment {
        environment_id,
        path,
    } = &bound.plugin().manifest().paths.skills[0];
    assert_eq!(environment_id, ENVIRONMENT);
    assert!(
        path.to_abs_path().is_err(),
        "the selected path must be foreign to this test host"
    );
    assert!(
        bound
            .file_system()
            .read_file_text(
                &path.join("read/SKILL.md").unwrap(),
                ReadFileOptions::default(),
                None
            )
            .await
            .unwrap()
            .contains("executor-owned instructions")
    );
    let mut servers = PluginMcpProvider
        .load(&bound)
        .await
        .unwrap()
        .into_iter()
        .collect::<HashMap<_, _>>();
    assert_eq!(
        servers.len(),
        1,
        "an invalid sibling must not discard valid servers"
    );
    let declared = &servers["node_repl"];
    assert_eq!(declared.environment_id, ENVIRONMENT);
    let McpServerTransportConfig::Stdio {
        command, args, cwd, ..
    } = &declared.transport
    else {
        panic!("stdio")
    };
    assert_eq!(command, "executor-worker");
    assert_eq!(args, &["server.py"]);
    assert_eq!(
        cwd.as_ref().unwrap().as_str(),
        fixture
            .backend
            .remote_base
            .join("plugin")
            .unwrap()
            .inferred_native_path_string()
    );
    let overlay: PluginMcpServerConfig = serde_json::from_value(json!({
        "enabled": true, "enabled_tools": ["read", "turn_ended", "extra"], "disabled_tools": ["write"],
        "default_tools_approval_mode": "auto", "tools": {"read": {"output_token_limit": 20}}
    }))
    .unwrap();
    apply_configured_plugin_mcp_server_policies(
        &HashMap::from([("node_repl".into(), overlay)]),
        &mut servers,
    );
    let restricted = &servers["node_repl"];
    assert!(restricted.enabled);
    assert_eq!(
        restricted.enabled_tools.as_ref().unwrap(),
        &["read", "turn_ended"]
    );
    assert_eq!(
        restricted.disabled_tools.as_ref().unwrap(),
        &["unsafe", "write"]
    );
    assert_eq!(
        restricted.default_tools_approval_mode,
        Some(AppToolApproval::Prompt)
    );
    assert_eq!(
        restricted.tools["read"].output_token_limit.unwrap().get(),
        12
    );
    let hooks = executor_plugin_hook_sources(&snapshot, &PluginsConfigToml::default(), |_, _| None);
    assert_eq!(hooks.len(), 1);
    assert_eq!(hooks[0].environment_id, ENVIRONMENT);
    assert_eq!(
        hooks[0].hooks.stop[0].hooks.len(),
        1,
        "commands must not enter executor cleanup"
    );
    fixture.write("plugin/.codex-plugin/plugin.json", "{}");
    let later = cache.snapshot(&[root], &HashMap::new()).await;
    assert!(Arc::ptr_eq(
        discovery,
        later.roots()[0].result.as_ref().unwrap()
    ));
    assert_eq!(
        executor_plugin_hook_sources(&later, &PluginsConfigToml::default(), |_, _| None),
        hooks
    );
    assert_eq!(fixture.calls("capabilityRoots/discoverV1"), 1);
    fixture.finish().await;
}

#[tokio::test]
async fn host_path_collision_and_missing_executor_files_never_fall_back_to_host() {
    let host = tempfile::tempdir().unwrap();
    let host_plugin = host.path().join("plugin");
    std::fs::create_dir_all(host_plugin.join(".codex-plugin")).unwrap();
    std::fs::write(
        host_plugin.join(".codex-plugin/plugin.json"),
        cleanup_manifest().to_string(),
    )
    .unwrap();
    std::fs::write(
        host_plugin.join("servers.json"),
        mcp_declarations("host-impostor").to_string(),
    )
    .unwrap();
    let fixture = RemotePlugins::start(PathUri::from_host_native_path(host.path()).unwrap()).await;
    install_cleanup(&fixture, "plugin", "executor-worker");
    let root = fixture.root(CLEANUP_PLUGIN, "plugin");
    let provider = ExecutorPluginProvider::new(fixture.manager.clone());
    let bound = provider.resolve_bound(&root).await.unwrap().unwrap();
    let servers = PluginMcpProvider.load(&bound).await.unwrap();
    let McpServerTransportConfig::Stdio { command, .. } = &servers[0].1.transport else {
        panic!("stdio")
    };
    assert_eq!(command, "executor-worker");
    std::fs::remove_file(fixture.backend.disk.path().join("plugin/servers.json")).unwrap();
    assert!(matches!(
        PluginMcpProvider.load(&bound).await,
        Err(PluginMcpProviderError::ReadConfig { .. })
    ));
    assert!(host_plugin.join("servers.json").is_file());
    let missing = ExecutorPluginProvider::new(Arc::new(EnvironmentManager::without_environments(
        HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
    )));
    assert!(matches!(
        missing.resolve_bound(&root).await,
        Err(ExecutorPluginProviderError::UnavailableEnvironment { .. })
    ));
    assert!(matches!(
        missing.root_ownership(&root, None, None).await,
        Err(ExecutorPluginProviderError::UnavailableEnvironment { .. })
    ));
    fixture.finish().await;
}

fn browser_tool(connector: &str) -> McpToolInfo {
    McpToolInfo::new("codex_apps", serde_json::from_value(json!({
        "name": "browser.turn_ended", "inputSchema": {"type": "object"},
        "_meta": {"_codex_apps": {"resource_uri": "resource://browser/session", "origin": "enabled-catalog"}}
    })).unwrap()).with_connector_id(Some(connector.into()))
}

#[tokio::test]
async fn one_remote_snapshot_admits_only_inline_cleanup_with_trusted_catalog_routing() {
    let fixture = RemotePlugins::start(foreign_volume()).await;
    let browser = json!({"name": "browser", "hooks": {"hooks": {"Stop": [{"hooks": [
        {"type": "mcp_tool", "server": "codex_apps", "tool": "browser.turn_ended", "input": {}}
    ]}]}}});
    fixture.write("browser/.codex-plugin/plugin.json", browser.to_string());
    fixture.write(
        "file/.codex-plugin/plugin.json",
        json!({"name": "file", "hooks": "./hooks.json"}).to_string(),
    );
    fixture.write("file/hooks.json", json!({"hooks": {"Stop": [{"hooks": [{"type": "command", "command": "untrusted-command"}]}]}}).to_string());
    fixture.write("broken/.codex-plugin/plugin.json", "{");
    fixture.write(
        "broken/.claude-plugin/plugin.json",
        json!({"name": "valid-alternate"}).to_string(),
    );
    let roots = vec![
        fixture.root(BROWSER_PLUGIN, "browser"),
        fixture.root(CLEANUP_PLUGIN, "file"),
        fixture.root("broken@local", "broken"),
    ];
    let cache = ExecutorCapabilityDiscoveryCache::new(fixture.manager.clone());
    let snapshot = cache.snapshot(&roots, &HashMap::new()).await;
    assert!(snapshot.roots().iter().all(|entry| entry.result.is_ok()));
    let enabled_tool = browser_tool("connector_openai_browser");
    let lookup = |server: &str, tool: &str| {
        (server == "codex_apps" && tool == "browser.turn_ended").then_some(&enabled_tool)
    };
    let admitted = executor_plugin_hook_sources(&snapshot, &PluginsConfigToml::default(), lookup);
    assert_eq!(
        admitted.len(),
        1,
        "file-backed and malformed declarations must not enter cleanup"
    );
    assert_eq!(admitted[0].plugin_id.as_key(), BROWSER_PLUGIN);
    assert_eq!(admitted[0].environment_id, ENVIRONMENT);
    assert_eq!(admitted[0].mcp_environment_id.as_deref(), Some("local"));
    assert_eq!(
        admitted[0].mcp_metadata.as_ref().unwrap()["_codex_apps"]["origin"],
        "enabled-catalog"
    );
    assert!(
        executor_plugin_hook_sources(&snapshot, &PluginsConfigToml::default(), |_, _| None)
            .is_empty()
    );
    let collision = browser_tool("connector_unrelated");
    assert!(
        executor_plugin_hook_sources(&snapshot, &PluginsConfigToml::default(), |_, _| Some(
            &collision
        ))
        .is_empty()
    );
    let denied = toml::from_str::<PluginsConfigToml>("[_default]\nenabled = false").unwrap();
    assert!(executor_plugin_hook_sources(&snapshot, &denied, lookup).is_empty());
    let provider = ExecutorPluginProvider::new(fixture.manager.clone());
    let broken = snapshot.roots()[2].result.as_ref().unwrap();
    assert!(matches!(
        provider.root_ownership(&roots[2], Some(broken), None).await,
        Ok(PluginRootOwnership::Plugin)
    ));
    let error = provider.resolve_bound(&roots[2]).await.err().unwrap();
    let ExecutorPluginProviderError::ParseManifest { path, .. } = error else {
        panic!("the preferred manifest must fail without trying the valid alternate");
    };
    let CapabilityRootLocation::Environment { path: root, .. } = &roots[2].location;
    assert_eq!(path, root.join(".codex-plugin/plugin.json").unwrap());
    fixture.finish().await;
}
