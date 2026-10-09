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
    agent::{HOST_BACKED_PROFILE, build_agent_with_profile},
    host::CodingHost,
    profile::CodingProfile,
    thread_settings::CodingThreadSettingsAdapter,
};

/// Builds the coding agent's threads for the app server (see the module's documentation).
#[derive(Clone)]
pub struct CodingThreadFactory {
    host: CodingHost,
    models: Arc<dyn ModelResolver>,
    settings: CodingThreadSettingsAdapter,
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
            role: PromptRole::Main,
            profile: HOST_BACKED_PROFILE,
            agent_id: AgentId::new("coding"),
            agent_name: "Coding agent".to_owned(),
        }
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
    async fn prepare_thread(&self, request: &ThreadFactoryRequest) -> Result<PreparedThread> {
        let host = self.host.with_new_process_manager();
        let agent = build_agent_with_profile(
            self.agent_id.clone(),
            self.agent_name.clone(),
            &self.role,
            &host,
            self.profile,
        )
        .await?;
        let builder = AgentThread::builder(
            request.thread_id().clone(),
            AgentBinding::direct(agent),
            Arc::clone(&self.models),
        )
        .with_config(self.settings.initialize(host.build_run_config()))
        .with_services(host.build_tool_services());
        Ok(PreparedThread::new(builder)
            .with_settings_adapter(Arc::new(self.settings.clone()))
            .with_background_terminals(Arc::new(host.background_terminals())))
    }
}
