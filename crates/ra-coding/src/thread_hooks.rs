// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! A thread's product hooks: what Codex keeps as a session's `services.hooks`, built when the
//! session is created (`core/src/session/session.rs`) from the configuration
//! `build_hooks_config` (`core/src/session/mod.rs`) derives.
//!
//! The hooks belong to the thread. The thread factory builds them from the thread's own product
//! configuration and installs them as the thread's host hooks; the app server binds them to the
//! thread's event stream once it is loaded, where their runs are reported in order with the turn
//! events, and closes them once it has shut down.
//!
//! # Differences from Codex
//!
//! - A thread is built only with a product configuration; without one it has no product hooks,
//!   since there is no configuration layer stack to read them from.
//! - The thread turn each hook run belongs to is resolved by the hook bridge from the runs it
//!   binds, rather than kept here.

use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use futures::FutureExt;
use futures::future::BoxFuture;
use ra_config::Feature;
use ra_core::session::SessionId;
use ra_protocol::server::{ThreadProductBinding, ThreadProductRuntime};
use ra_runtime::thread::ThreadEvent;

use crate::config::Config;
use crate::hook_mcp_executor::CodingHookMcpExecutor;
use crate::hooks::{
    HookMcpCall, HookMcpExecutor, HooksConfig, ProductHookRuntime, ProductHookSession,
};
use crate::host::CodingHost;
use crate::shell_command::shell_detect::{DetectedShell, ShellType, default_user_shell};

/// Builds the hook configuration of one configuration snapshot, with the hooks of its enabled
/// plugins: Codex's `build_hooks_config`.
///
/// Plugins come from the host's package services, asked with this configuration's plugin input;
/// a host without them contributes none, and the user, project and managed layers still load.
/// The shell is the local environment's, as Codex derives it from the turn environment's shell.
pub(crate) async fn build_hooks_config(
    config: &Config,
    plugins: Option<&crate::plugins::PluginServices>,
    disabled_plugin_ids: &[String],
) -> HooksConfig {
    let (shell_program, shell_args) = hook_shell(&default_user_shell());
    let (plugin_hook_sources, plugin_hook_load_warnings) = match plugins {
        Some(plugins) => {
            let outcome = plugins
                .plugins_for_config(&config.plugins_config_input())
                .await
                .without_plugins(disabled_plugin_ids);
            (
                outcome.effective_plugin_hook_sources(),
                outcome.effective_plugin_hook_warnings(),
            )
        }
        None => (Vec::new(), Vec::new()),
    };
    HooksConfig {
        legacy_notify_argv: config.notify.clone(),
        feature_enabled: config.features.enabled(Feature::CodexHooks),
        bypass_hook_trust: config.bypass_hook_trust,
        config_layer_stack: Some(config.config_layer_stack.clone()),
        plugin_hook_sources,
        plugin_hook_load_warnings,
        shell_program: Some(shell_program),
        shell_args,
    }
}

/// Codex's `shell.derive_exec_args("", false)` with the program split off and the empty command
/// dropped.
fn hook_shell(shell: &DetectedShell) -> (String, Vec<String>) {
    let program = shell.shell_path.to_string_lossy().into_owned();
    let args = match shell.shell_type {
        ShellType::Zsh | ShellType::Bash | ShellType::Sh => vec!["-c".to_owned()],
        ShellType::PowerShell => vec!["-NoProfile".to_owned(), "-Command".to_owned()],
        ShellType::Cmd => vec!["/c".to_owned()],
    };
    (program, args)
}

/// One thread's product hooks, for the app server to bind and close with the thread.
pub struct CodingThreadHooks {
    runtime: Arc<ProductHookRuntime>,
    executor: Arc<crate::thread_environments::ThreadExecutorHooks>,
    /// Warnings from loading the hooks, shown once the thread can report them.
    startup_warnings: Mutex<Vec<String>>,
}

impl std::fmt::Debug for CodingThreadHooks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodingThreadHooks").finish_non_exhaustive()
    }
}

impl CodingThreadHooks {
    /// Builds the hooks of the thread `thread_id` from its configuration, as Codex builds a
    /// session's.
    ///
    /// # Errors
    ///
    /// When a required managed hook fails to load, which fails the thread's creation.
    pub(crate) async fn build(
        config: &Config,
        host: &CodingHost,
        thread_id: SessionId,
        ignore_user_config: bool,
        environments: Option<&[ra_protocol::server::protocol::TurnEnvironmentParams]>,
    ) -> anyhow::Result<Self> {
        let hooks_config =
            build_hooks_config(config, host.plugin_services().map(Arc::as_ref), &[]).await;
        let cwd = ra_paths::AbsolutePathBuf::from_absolute_path(host.workspace_root())?;
        let mcp_executor: Arc<dyn HookMcpExecutor> = match host.mcp_runtime() {
            Some(runtime) => Arc::new(CodingHookMcpExecutor::new(
                Arc::clone(runtime),
                thread_id.clone(),
            )),
            None => Arc::new(NoMcpRuntime),
        };
        let runtime = ProductHookRuntime::new(
            hooks_config,
            ProductHookSession {
                session_id: thread_id,
                cwd,
                model: config.model.clone().unwrap_or_default(),
                approval_policy: config.permissions.approval_policy.value(),
                transcript_path: None,
            },
            mcp_executor,
        )?;
        let executor = crate::thread_environments::ThreadExecutorHooks::build(
            config,
            host,
            &runtime,
            ignore_user_config,
            environments,
        )
        .await?;
        runtime.set_executor_hook_sources(executor.clone());
        let startup_warnings = Mutex::new(runtime.startup_warnings());
        Ok(Self {
            runtime,
            executor,
            startup_warnings,
        })
    }

    pub(crate) fn install(
        &self,
        config: ra_runtime::runner::RunConfig,
    ) -> ra_runtime::runner::RunConfig {
        let mut config = config;
        if let Some(mut settings) = config
            .app_context()
            .and_then(|value| value.downcast_ref::<crate::thread_settings::CodingThreadSettings>())
            .cloned()
        {
            settings.executor_environments = Some(self.executor.manager());
            config = config.with_app_context(Arc::new(settings));
        }
        config
            .with_user_hooks(self.runtime.user_hooks())
            .with_step_execution_observer(self.executor.clone())
    }

    /// Tests the enabled projection of the production cleanup app policy.
    #[cfg(feature = "test-api")]
    #[doc(hidden)]
    #[must_use]
    pub fn app_tool_enabled_for_test(
        stack: &ra_config::ConfigLayerStack,
        info: &ra_mcp::catalog::McpToolInfo,
    ) -> bool {
        crate::thread_environments::app_policy::app_tool_enabled(stack, info)
    }

    /// Builds the production hook runtime for external integration tests.
    #[cfg(feature = "test-api")]
    #[doc(hidden)]
    pub async fn build_for_test(
        config: &Config,
        host: &CodingHost,
        thread_id: SessionId,
    ) -> anyhow::Result<Self> {
        Self::build(config, host, thread_id, false, None).await
    }

    /// Installs the same production hook and step bindings as the default factory.
    #[cfg(feature = "test-api")]
    #[doc(hidden)]
    pub fn install_for_test(
        &self,
        config: ra_runtime::runner::RunConfig,
    ) -> ra_runtime::runner::RunConfig {
        self.install(config)
    }

    /// The hooks, as the runtime's host hooks and the network approval path call them.
    #[must_use]
    pub fn runtime(&self) -> &Arc<ProductHookRuntime> {
        &self.runtime
    }
}

#[async_trait]
impl ThreadProductRuntime for CodingThreadHooks {
    fn bind_thread(&self, binding: ThreadProductBinding) {
        let events = binding.event_sender();
        // Codex shows the hooks' startup warnings among the session's first events, ahead of any
        // turn.
        let warnings = std::mem::take(
            &mut *self
                .startup_warnings
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        );
        for message in warnings {
            let _ = events.send(ThreadEvent::Warning { message });
        }
        self.runtime.bind_thread_events(events);
        self.runtime.bind_transcript(binding.thread_handle());
    }

    fn on_thread_event(&self, event: &ThreadEvent) {
        if let ThreadEvent::TurnComplete { turn_id, .. }
        | ThreadEvent::TurnAborted { turn_id, .. } = event
        {
            self.executor.finish_turn(turn_id);
        }
    }

    async fn close(&self) {
        self.executor.close();
        self.runtime.stop_background().await;
        self.runtime.release_thread_events();
    }
}

/// The MCP executor of a host without an MCP runtime: MCP hooks fail and command hooks still run.
struct NoMcpRuntime;

impl HookMcpExecutor for NoMcpRuntime {
    fn execute(&self, call: HookMcpCall) -> BoxFuture<'_, anyhow::Result<String>> {
        async move {
            anyhow::bail!(
                "MCP server '{}' is not available: this host runs no MCP servers",
                call.server
            )
        }
        .boxed()
    }
}
