//! The coding product's thread factory: how the app server builds the coding agent's threads for
//! `thread/start`, `thread/resume` and `thread/fork`.
//!
//! Each thread runs the coding agent assembled for one role and tier from the host's workspace,
//! on the models the host resolves, with the coding run configuration and the coding settings
//! adapter, initialized with the host's selected policy. Each thread gets a process manager of its
//! own, as each Codex session has its own, so its background terminals are listed and stopped
//! apart from other threads'.

use ra_protocol::server::ThreadSettingsAdapter;
use std::sync::Arc;

use async_trait::async_trait;
use ra_core::{agent::AgentId, error::Result, model::ModelResolver, prompt::PromptRole};
use ra_protocol::server::{PreparedThread, ThreadFactory, ThreadFactoryRequest};
use ra_runtime::{agent::AgentBinding, thread::AgentThread};

use crate::{
    agent::{HOST_BACKED_PROFILE, assemble_agent},
    config::{
        ConfigBuilder, model_catalog_remote::RemoteModelCatalogs,
        model_runtime::ProductModelRuntime,
    },
    host::CodingHost,
    profile::CodingProfile,
    spawned,
    thread_hooks::CodingThreadHooks,
    thread_settings::{CodingThreadSettingsAdapter, ProductPermissionSource},
};

/// Builds the coding agent's threads for the app server (see the module's documentation).
#[derive(Clone)]
pub struct CodingThreadFactory {
    host: CodingHost,
    models: Option<Arc<dyn ModelResolver>>,
    settings: CodingThreadSettingsAdapter,
    product_config: Option<ConfigBuilder>,
    model_catalogs: Arc<RemoteModelCatalogs>,
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
            models: Some(models),
            settings,
            product_config: None,
            model_catalogs: Arc::default(),
            role: PromptRole::Main,
            profile: HOST_BACKED_PROFILE,
            agent_id: AgentId::new("coding"),
            agent_name: "Coding agent".to_owned(),
        }
    }

    /// Uses a thread's effective product configuration to resolve its models and Responses endpoint.
    #[must_use]
    pub fn from_product_config(
        host: CodingHost,
        settings: CodingThreadSettingsAdapter,
        builder: ConfigBuilder,
    ) -> Self {
        Self {
            host,
            models: None,
            settings,
            product_config: Some(builder),
            model_catalogs: Arc::default(),
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
    ///
    /// The resolver passed to [`Self::new`] retains ownership of provider and settings resolution.
    ///
    /// Each thread's product hooks are built from that configuration too, as Codex builds a
    /// session's: a thread without a product configuration has none.
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

impl CodingThreadFactory {
    async fn load_product_configuration(
        &self,
        mut builder: ConfigBuilder,
        host: &CodingHost,
        request: &ThreadFactoryRequest,
    ) -> Result<(
        CodingThreadSettingsAdapter,
        Arc<ProductModelRuntime>,
        Arc<CodingThreadHooks>,
    )> {
        let generation = host
            .plugin_services()
            .map_or(0, |plugins| plugins.hook_generation());
        builder.harness_overrides_mut().cwd = Some(host.workspace_root().to_path_buf());
        ProductPermissionSource::apply_initial_policy(&mut builder, request.policy())
            .map_err(ra_core::error::Error::config)?;
        let config = Arc::new(builder.clone().build().await.map_err(|error| {
            ra_core::error::Error::config(format!(
                "failed to load the product configuration: {error}"
            ))
        })?);
        let models = Arc::new(
            ProductModelRuntime::new(
                Arc::clone(&config),
                self.models.clone(),
                request.initial_model(),
                request.session_source().is_non_root_agent(),
                &self.model_catalogs,
            )
            .await?,
        );

        // A named profile requirements disallow falls back with a startup warning. A settings or
        // turn update refuses the same selection.
        let settings = self
            .settings
            .clone()
            .for_new_thread()
            .with_product_config(&config, ProductPermissionSource::new(builder.clone()));
        settings
            .start_network_proxy(&config)
            .await
            .map_err(|error| ra_core::error::Error::config(error.to_string()))?;
        // A required managed hook that fails to load fails the thread.
        let hooks = Arc::new(
            Box::pin(CodingThreadHooks::build_refreshing(
                (*config).clone(),
                builder,
                host,
                request.thread_id().clone(),
                generation,
                request.policy().environments(),
                hook_role(request),
            ))
            .await
            .map_err(|error| hook_construction_error(&error))?,
        );
        settings.attach_hook_runtime(Arc::clone(hooks.runtime()));
        Ok((settings, models, hooks))
    }

    /// The thread's agent tree. Its root is the thread: its session is what the agents' hooks and
    /// spawn records name as their parent's, whether or not the server records them, and a thread
    /// that was itself spawned continues its lineage.
    fn thread_tree(
        &self,
        host: &CodingHost,
        request: &ThreadFactoryRequest,
        services: spawned::ThreadServices,
    ) -> Result<ra_runtime::agent::control::AgentControl> {
        let mut control = spawned::agent_control(
            host,
            self.agent_id.clone(),
            self.agent_name.clone(),
            &self.role,
            self.profile,
            Some(crate::prompt_cache_key::session_id(request).clone()),
            services,
        )?
        .with_root_session_id(request.thread_id().clone())?;
        if let Some(spawn) = request.thread_spawn() {
            control = control.with_root_lineage(spawn)?;
        }
        Ok(control)
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
        let (settings, model_runtime, hooks) = match &self.product_config {
            Some(builder) => {
                let (settings, runtime, hooks) = self
                    .load_product_configuration(builder.clone(), &host, request)
                    .await?;
                (settings, Some(runtime), Some(hooks))
            }
            None => (self.settings.clone().for_new_thread(), None, None),
        };
        let assembled = assemble_agent(
            self.agent_id.clone(),
            self.agent_name.clone(),
            &self.role,
            &host,
            self.profile,
        )
        .await?;
        let (run_config, models): (_, Arc<dyn ModelResolver>) = match model_runtime {
            Some(runtime) => (
                runtime.configure_run(
                    CodingHost::build_run_config_with_compaction(runtime.clone())
                        .with_context_filter(runtime.clone()),
                ),
                runtime,
            ),
            None => (
                host.build_run_config(),
                self.models.clone().ok_or_else(|| {
                    ra_core::error::Error::config(
                        "a coding thread requires a model resolver or product configuration",
                    )
                })?,
            ),
        };
        let mut config = settings.initialize(
            run_config
                .with_prompt_cache_key_override(crate::prompt_cache_key::for_thread(request))
                .with_prepared_capabilities(assembled.capabilities, assembled.capability_tools),
        );
        if let Some(hooks) = &hooks {
            config = hooks.install(config);
            let mut selection = ra_protocol::server::protocol::ThreadPolicyOverrides::new();
            if let Some(environments) = request.policy().environments() {
                selection = selection.with_environments(environments.to_vec());
            }
            if let Some(roots) = request.policy().selected_capability_roots() {
                selection = selection.with_selected_capability_roots(roots.to_vec());
            }
            config = settings
                .prepare_async(&config, &selection)
                .await
                .map_err(|reason| {
                    ra_core::error::Error::config(reason.clone()).with_source(
                        ra_protocol::frame::JsonRpcErrorObject::invalid_request(reason),
                    )
                })?;
        }
        let mut builder = AgentThread::builder(
            request.thread_id().clone(),
            AgentBinding::direct(assembled.agent),
            models,
        )
        .with_config(config)
        .with_services(host.build_tool_services());
        // Each thread owns a tree of its own, as each Codex session owns its agents; the thread
        // shuts it down when it closes.
        if assembled.collaborates {
            let services = spawned::ThreadServices {
                hooks: hooks.clone(),
                network_proxy: Some(Arc::clone(settings.network_proxy())),
            };
            builder = builder.with_agent_control(self.thread_tree(&host, request, services)?);
        }
        let mut prepared = PreparedThread::new(builder)
            .with_settings_adapter(Arc::new(settings))
            .with_background_terminals(Arc::new(host.background_terminals()));
        if let Some(hooks) = hooks {
            prepared = prepared.with_product_runtime(hooks);
        }
        Ok(if self.product_config.is_some() {
            prepared.with_applied_policy()
        } else {
            prepared
        })
    }
}

/// Keeps typed request validation failures for the server without changing framework error kinds.
fn hook_construction_error(error: &anyhow::Error) -> ra_core::error::Error {
    if let Some(request) = error.downcast_ref::<ra_protocol::frame::JsonRpcErrorObject>() {
        ra_core::error::Error::config(request.message()).with_source(request.clone())
    } else {
        ra_core::error::Error::config(error.to_string())
    }
}

/// Which thread the hooks speak for: a spawned thread loaded on its own keeps its spawned identity,
/// as Codex's resumed sub-agent session does.
fn hook_role(request: &ThreadFactoryRequest) -> crate::hooks::HookThreadRole {
    let Some(spawn) = request.thread_spawn() else {
        return crate::hooks::HookThreadRole::Root;
    };
    let lookup: crate::hooks::ParentTranscriptLookup = match request.parent_transcript() {
        Some(lookup) => {
            let lookup = lookup.clone();
            Arc::new(move || {
                let lookup = lookup.clone();
                Box::pin(async move { lookup.path().await })
            })
        }
        None => Arc::new(|| Box::pin(async { None })),
    };
    crate::hooks::HookThreadRole::Child(crate::hooks::ChildHookIdentity::resumed(
        spawn.agent_type().map(ra_core::agent::AgentId::as_str),
        lookup,
    ))
}
