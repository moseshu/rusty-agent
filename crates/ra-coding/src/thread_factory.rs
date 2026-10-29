//! The coding product's thread factory: how the app server builds the coding agent's threads for
//! `thread/start`, `thread/resume` and `thread/fork`.
//!
//! Each thread runs the coding agent assembled for one role and tier from the host's workspace,
//! on the models the host resolves, with the coding run configuration and the coding settings
//! adapter, initialized with the host's selected policy. Each thread gets a process manager of its
//! own, as each Codex session has its own, so its background terminals are listed and stopped
//! apart from other threads'.

use std::sync::Arc;

use async_trait::async_trait;
use ra_core::{agent::AgentId, error::Result, model::ModelResolver, prompt::PromptRole};
use ra_protocol::server::{PreparedThread, ThreadFactory, ThreadFactoryRequest};
use ra_runtime::{agent::AgentBinding, thread::AgentThread};

use crate::{
    agent::{HOST_BACKED_PROFILE, assemble_agent},
    config::ConfigBuilder,
    host::CodingHost,
    profile::CodingProfile,
    spawned,
    thread_settings::{CodingThreadSettingsAdapter, ProductPermissionSource},
};

/// Builds the coding agent's threads for the app server (see the module's documentation).
#[derive(Clone)]
pub struct CodingThreadFactory {
    host: CodingHost,
    models: Arc<dyn ModelResolver>,
    settings: CodingThreadSettingsAdapter,
    product_config: Option<ConfigBuilder>,
    role: PromptRole,
    profile: CodingProfile,
    agent_id: AgentId,
    agent_name: String,
}

impl std::fmt::Debug for CodingThreadFactory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodingThreadFactory")
            .field("host", &self.host)
            .field("role", &self.role)
            .field("profile", &self.profile)
            .field("agent_id", &self.agent_id)
            .finish_non_exhaustive()
    }
}

impl CodingThreadFactory {
    /// Threads of the main coding agent on the [`HOST_BACKED_PROFILE`] tier, working in `host`'s
    /// workspace on the models `models` resolves, with product settings applied through
    /// `settings`.
    #[must_use]
    pub fn new(
        host: CodingHost,
        models: Arc<dyn ModelResolver>,
        settings: CodingThreadSettingsAdapter,
    ) -> Self {
        Self {
            host,
            models,
            settings,
            product_config: None,
            role: PromptRole::Main,
            profile: HOST_BACKED_PROFILE,
            agent_id: AgentId::new("coding"),
            agent_name: "Coding agent".to_owned(),
        }
    }

    /// Loads each thread's product configuration with `builder`, as Codex builds a configuration
    /// for every `thread/start`, `thread/resume` and `thread/fork`: the thread's approval policy,
    /// reviewer and permissions — the canonical profile and its requirement constraints — come
    /// from it, with the host workspace as its working directory, and named permission profiles
    /// are later resolved through it. Without it the settings adapter's own policy applies.
    #[must_use]
    pub fn with_product_config(mut self, builder: ConfigBuilder) -> Self {
        self.product_config = Some(builder);
        self
    }

    /// Assembles the agent for `role`.
    #[must_use]
    pub fn with_role(mut self, role: PromptRole) -> Self {
        self.role = role;
        self
    }

    /// Assembles the agent on `profile`'s tier.
    #[must_use]
    pub const fn with_profile(mut self, profile: CodingProfile) -> Self {
        self.profile = profile;
        self
    }

    /// Enables Codex's optional Default-mode user questions. The app server supplies the port
    /// that routes each thread's questions to its own client, including after resume or fork.
    #[must_use]
    pub fn with_request_user_input(mut self) -> Self {
        self.host = self.host.with_client_user_input();
        self
    }

    /// Names the agent.
    #[must_use]
    pub fn with_agent(mut self, id: AgentId, name: impl Into<String>) -> Self {
        self.agent_id = id;
        self.agent_name = name.into();
        self
    }
}

#[async_trait]
impl ThreadFactory for CodingThreadFactory {
    fn mcp_runtime(&self) -> Option<Arc<dyn ra_protocol::server::McpRuntimeControl>> {
        self.host
            .mcp_runtime()
            .map(|runtime| runtime.clone() as Arc<dyn ra_protocol::server::McpRuntimeControl>)
    }

    async fn prepare_thread(&self, request: &ThreadFactoryRequest) -> Result<PreparedThread> {
        let host = self.host.with_new_process_manager();
        let settings = match &self.product_config {
            Some(builder) => {
                let mut builder = builder.clone();
                builder.harness_overrides_mut().cwd = Some(host.workspace_root().to_path_buf());
                ProductPermissionSource::apply_initial_policy(&mut builder, request.policy())
                    .map_err(ra_core::error::Error::config)?;
                let config = builder.clone().build().await.map_err(|error| {
                    ra_core::error::Error::config(format!(
                        "failed to load the product configuration: {error}"
                    ))
                })?;
                // As in Codex's thread start, resume and fork, a named profile requirements
                // disallow falls back to the required value with a startup warning; only a
                // settings or turn update refuses it.
                self.settings
                    .clone()
                    .with_product_config(&config, ProductPermissionSource::new(builder))
            }
            None => self.settings.clone(),
        };
        let assembled = assemble_agent(
            self.agent_id.clone(),
            self.agent_name.clone(),
            &self.role,
            &host,
            self.profile,
        )
        .await?;
        let mut builder = AgentThread::builder(
            request.thread_id().clone(),
            AgentBinding::direct(assembled.agent),
            Arc::clone(&self.models),
        )
        .with_config(
            settings.initialize(
                host.build_run_config()
                    .with_prompt_cache_key_override(crate::prompt_cache_key::for_thread(request))
                    .with_prepared_capabilities(assembled.capabilities, assembled.capability_tools),
            ),
        )
        .with_services(host.build_tool_services());
        // Each thread owns a tree of its own, as each Codex session owns its agents; the thread
        // shuts it down when it closes.
        if assembled.collaborates {
            builder = builder.with_agent_control(spawned::agent_control(
                &host,
                self.agent_id.clone(),
                self.agent_name.clone(),
                &self.role,
                self.profile,
                Some(crate::prompt_cache_key::session_id(request).clone()),
            )?);
        }
        let prepared = PreparedThread::new(builder)
            .with_settings_adapter(Arc::new(settings))
            .with_background_terminals(Arc::new(host.background_terminals()));
        Ok(if self.product_config.is_some() {
            prepared.with_applied_policy()
        } else {
            prepared
        })
    }
}
