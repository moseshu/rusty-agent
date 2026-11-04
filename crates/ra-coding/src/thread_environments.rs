// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Selected environment discovery and the execution-step contexts retained for hook cleanup.
//!
//! This local product cannot route commands to remote environments yet, so those selections are
//! rejected before input admission. Unselected roots stay persisted without being discovered.

pub(crate) mod app_policy;

use crate::{
    config::Config,
    hooks::{ExecutorHookSourceProvider, ProductHookRuntime},
    plugins::ExecutorPluginHookSource,
    thread_settings::CodingThreadSettings,
};
use async_trait::async_trait;
use ra_core::{
    cancel::CancelScope,
    context::RunContext,
    error::{Error, Result},
};
use ra_executor::{EnvironmentManager, ExecutorCapabilityDiscoveryCache};
use ra_protocol::server::protocol::{CapabilityRootLocation, TurnEnvironmentParams};
use ra_runtime::runner::StepExecutionObserver;
use std::{
    collections::HashMap,
    path::Path,
    sync::{Arc, Mutex, PoisonError},
};

pub(crate) fn validate_local_selection(
    environments: &[TurnEnvironmentParams],
    cwd: &Path,
    manager: &EnvironmentManager,
) -> std::result::Result<(), String> {
    let mut ids = std::collections::HashSet::new();
    for environment in environments {
        let path = local_path(&environment.cwd)?;
        if path.inferred_native_path_string().len() > 8 * 1024 {
            return Err("turn environment working directory exceeds the maximum size".into());
        }
        if !ids.insert(&environment.environment_id) {
            return Err(format!(
                "duplicate turn environment id `{}`",
                environment.environment_id
            ));
        }
        if manager
            .get_environment(&environment.environment_id)
            .is_none()
        {
            return Err(format!(
                "unknown turn environment id `{}`",
                environment.environment_id
            ));
        }
    }
    for environment in environments {
        if environment.environment_id != "local" {
            return Err(format!(
                "unsupported environment `{}`: remote command routing and MCP environment projection are not implemented",
                environment.environment_id
            ));
        }
        let path = local_path(&environment.cwd)?;
        if path
            .to_abs_path()
            .map_err(|error| error.to_string())?
            .as_path()
            != cwd
        {
            return Err("unsupported environment cwd: changing the local execution directory is not implemented".into());
        }
        if let Some(roots) = &environment.runtime_workspace_roots {
            let [root] = roots.as_slice() else {
                return Err("unsupported runtimeWorkspaceRoots: changing execution workspace roots is not implemented".into());
            };
            if local_path(root)?
                .to_abs_path()
                .map_err(|error| error.to_string())?
                .as_path()
                != cwd
            {
                return Err("unsupported runtimeWorkspaceRoots: changing execution workspace roots is not implemented".into());
            }
        }
    }
    if environments.is_empty() {
        return Err("unsupported empty environment selection: disabling the local execution tools is not implemented".into());
    }
    Ok(())
}

fn local_path(
    value: &ra_paths::LegacyAppPathString,
) -> std::result::Result<ra_paths::PathUri, String> {
    if let Ok(path) = ra_paths::PathUri::parse(value.as_str()) {
        Ok(path)
    } else {
        ra_paths::AbsolutePathBuf::from_absolute_path(value.as_str())
            .map(|path| ra_paths::PathUri::from_abs_path(&path))
            .map_err(|error| error.to_string())
    }
}

/// One executing context per run; the key includes its owning turn so sibling runs never race for
/// a common last-step slot. Approval recovery retains this entry until the turn actually ends.
struct ExecutingStep {
    sources: Vec<ExecutorPluginHookSource>,
}

pub(crate) struct ThreadExecutorHooks {
    runtime: std::sync::Weak<ProductHookRuntime>,
    cache: ExecutorCapabilityDiscoveryCache,
    manager: Arc<EnvironmentManager>,
    environment: Arc<ra_executor::Environment>,
    config: Arc<Config>,
    mcp: Option<Arc<crate::mcp::CodingMcpRuntime>>,
    steps: Mutex<HashMap<(String, String), Arc<ExecutingStep>>>,
}

impl ThreadExecutorHooks {
    pub(crate) async fn build(
        config: &Config,
        host: &crate::host::CodingHost,
        runtime: &Arc<ProductHookRuntime>,
        ignore_user_config: bool,
        environments: Option<&[TurnEnvironmentParams]>,
    ) -> anyhow::Result<Arc<Self>> {
        let exe = config
            .codex_self_exe
            .clone()
            .map_or_else(std::env::current_exe, Ok)?;
        let paths = ra_executor::ExecServerRuntimeOptions::new(
            exe,
            config.codex_linux_sandbox_exe.clone(),
        )?;
        let prepared = if ignore_user_config {
            EnvironmentManager::prepare_from_env().await?
        } else {
            EnvironmentManager::prepare_from_codex_home(&config.codex_home).await?
        };
        let manager = Arc::new(prepared.build(Some(paths), config.http_client_factory())?);
        if let Some(environments) = environments {
            validate_local_selection(environments, host.workspace_root(), &manager)
                .map_err(ra_protocol::frame::JsonRpcErrorObject::invalid_request)?;
        }
        if environments.is_none() && manager.default_environment_ids() != ["local"] {
            anyhow::bail!(
                "unsupported default environment selection: this product currently routes execution only to local"
            );
        }
        Ok(Arc::new(Self {
            runtime: Arc::downgrade(runtime),
            environment: manager
                .try_local_environment()
                .ok_or_else(|| anyhow::anyhow!("local environment is unavailable"))?,
            cache: ExecutorCapabilityDiscoveryCache::new(Arc::clone(&manager)),
            manager,
            config: Arc::new(config.clone()),
            mcp: host.mcp_runtime().cloned(),
            steps: Mutex::new(HashMap::new()),
        }))
    }

    pub(crate) fn manager(&self) -> Arc<EnvironmentManager> {
        Arc::clone(&self.manager)
    }

    async fn discovery_sandbox(
        &self,
        settings: &CodingThreadSettings,
    ) -> Result<ra_filesystem::FileSystemSandboxContext> {
        let mut sandbox = settings.discovery_sandbox()?;
        let info = self
            .environment
            .info()
            .await
            .map_err(|error| Error::config(error.to_string()))?;
        sandbox.user_home_dir = info.user_home_dir;
        sandbox.temporary_directories = info.temporary_directories;
        sandbox.windows_sandbox_selection = match self.config.effective_local_windows_sandbox_type()
        {
            ra_filesystem::sandbox::SandboxType::WindowsMxc => {
                ra_filesystem::WindowsSandboxSelection::Mxc
            }
            ra_filesystem::sandbox::SandboxType::WindowsRestrictedToken => {
                if self.config.permissions.windows_sandbox_mode
                    == Some(ra_config::types::WindowsSandboxModeToml::Elevated)
                {
                    ra_filesystem::WindowsSandboxSelection::Elevated
                } else {
                    ra_filesystem::WindowsSandboxSelection::RestrictedToken
                }
            }
            _ => ra_filesystem::WindowsSandboxSelection::Disabled,
        };
        Ok(sandbox)
    }

    pub(crate) fn close(&self) {
        self.steps
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
    }

    pub(crate) fn finish_turn(&self, turn: &str) {
        self.steps
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|(id, _), _| id != turn);
    }
}

#[async_trait]
impl StepExecutionObserver for ThreadExecutorHooks {
    async fn capture(&self, run: &RunContext, cancel: &CancelScope) -> Result<()> {
        let Some(settings) = run.app_context::<CodingThreadSettings>() else {
            return Err(Error::config("executor step has no product settings"));
        };
        let sandbox = self.discovery_sandbox(settings).await?;
        let restricted = !sandbox
            .permissions
            .file_system_sandbox_policy()
            .has_full_disk_read_access();
        let mut roots = Vec::new();
        let mut seen = HashMap::new();
        for root in &settings.selected_capability_roots {
            let CapabilityRootLocation::Environment { environment_id, .. } = &root.location;
            if !settings
                .environments
                .iter()
                .any(|environment| &environment.environment_id == environment_id)
            {
                tracing::warn!(root_id = %root.id, %environment_id, "ignoring capability root without a captured turn environment");
                continue;
            }
            if let Some(location) = seen.get(&root.id) {
                if location != &root.location {
                    tracing::warn!(root_id = %root.id, "ignoring selected capability root with conflicting location");
                }
                continue;
            }
            seen.insert(root.id.clone(), root.location.clone());
            roots.push(root.clone());
        }
        let sources = if !roots.is_empty()
            && (restricted
                || self
                    .config
                    .features
                    .enabled(ra_config::Feature::ExecutorCapabilityDiscovery))
        {
            let sandboxes = if restricted {
                HashMap::from([("local".to_owned(), sandbox)])
            } else {
                HashMap::new()
            };
            let snapshot = cancel.run(self.cache.snapshot(&roots, &sandboxes)).await?;
            let infos = match &self.mcp {
                Some(mcp) => cancel.run(mcp.hook_tool_infos(run)).await??,
                None => Vec::new(),
            };
            crate::plugins::executor_plugin_hook_sources(
                &snapshot,
                &self.config.plugins,
                |server, tool| {
                    infos.iter().find(|info| {
                        info.server_name() == server
                            && info.tool().name == tool
                            && (server != "codex_apps"
                                || app_policy::app_tool_enabled(
                                    &self.config.config_layer_stack,
                                    info,
                                ))
                    })
                },
            )
        } else {
            Vec::new()
        };
        cancel.ensure_not_cancelled()?;
        let Some(runtime) = self.runtime.upgrade() else {
            return Ok(());
        };
        let turn = runtime.turn_id(run);
        let mut steps = self.steps.lock().unwrap_or_else(PoisonError::into_inner);
        let key = (turn, run.run_id().to_string());
        steps.insert(key, Arc::new(ExecutingStep { sources }));
        Ok(())
    }
}

impl ExecutorHookSourceProvider for ThreadExecutorHooks {
    fn executor_hook_sources(&self, run: &RunContext) -> Vec<ExecutorPluginHookSource> {
        let Some(runtime) = self.runtime.upgrade() else {
            return Vec::new();
        };
        self.steps
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&(runtime.turn_id(run), run.run_id().to_string()))
            .map(|step| step.sources.clone())
            .unwrap_or_default()
    }
}
