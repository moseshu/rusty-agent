// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! The coding product's per-thread collaboration version and per-model delegation gate.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use ra_core::{
    context::RunContext,
    error::{Error, Result},
    tool::{
        DecodedToolInput, FuncSchema, Tool, ToolContext, ToolOptions, ToolOrigin, ToolOutput,
        ToolSchema, ToolServices,
    },
};
use ra_protocol::server::{SessionSource, SubAgentSource, ThreadFactoryRequest};
use ra_runtime::runner::RunConfig;
use ra_tools::agent_ns::CollaborationTool;

use crate::config::{MultiAgentVersion, model_runtime::ProductModelRuntime};

pub(crate) struct ThreadCollaboration {
    runtime: Arc<ProductModelRuntime>,
    version: OnceLock<MultiAgentVersion>,
    /// A recorded value this build does not recognize. It is kept as recorded rather than replaced
    /// by a migrated value, and the thread is offered no collaboration tools.
    unrecognized: Option<String>,
    spawned: bool,
}

impl ThreadCollaboration {
    /// `legacy_collaboration_offered` is what the former host assembly offered the thread's own
    /// agent. It settles the version of a thread with sampled history and no recorded version.
    pub(crate) fn new(
        runtime: Arc<ProductModelRuntime>,
        request: &ThreadFactoryRequest,
        legacy_collaboration_offered: bool,
    ) -> Self {
        let override_version = runtime.multi_agent_version_override();
        let recorded = request.recorded_multi_agent_version();
        let unrecognized = recorded
            .filter(|version| override_version.is_none() && parse_version(version).is_none())
            .map(str::to_owned);
        let migrating =
            override_version.is_none() && recorded.is_none() && request.has_sampled_history();
        let version = override_version
            .or_else(|| {
                recorded
                    .map(|version| parse_version(version).unwrap_or(MultiAgentVersion::Disabled))
            })
            .or_else(|| {
                migrating.then_some(if legacy_collaboration_offered {
                    MultiAgentVersion::V2
                } else {
                    MultiAgentVersion::Disabled
                })
            });
        Self {
            runtime,
            version: version.map(OnceLock::from).unwrap_or_default(),
            unrecognized,
            spawned: request.thread_spawn().is_some()
                || matches!(
                    request.session_source(),
                    SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                        agent_path: Some(_),
                        ..
                    })
                ),
        }
    }

    /// The value to record for the thread, once it is known.
    pub(crate) fn recorded_version(&self) -> Option<&str> {
        self.unrecognized
            .as_deref()
            .or_else(|| self.version.get().copied().map(version_name))
    }

    /// Whether an agent assembled for this thread keeps the tools for per-turn checks.
    pub(crate) fn can_offer_tools(&self) -> bool {
        self.version
            .get()
            .is_none_or(|version| *version == MultiAgentVersion::V2)
    }

    pub(crate) fn install(self: &Arc<Self>, config: RunConfig) -> RunConfig {
        let thread = Arc::clone(self);
        config.with_turn_context_enricher(move |_, record| {
            let version = *thread.version.get_or_init(|| {
                thread
                    .runtime
                    .multi_agent_version_for_model(thread.model_version(record.model()))
            });
            match &thread.unrecognized {
                Some(recorded) => record.with_multi_agent_version(recorded.clone()),
                None => record.with_multi_agent_version(version_name(version)),
            }
        })
    }

    fn model_version(&self, model: Option<&str>) -> Option<MultiAgentVersion> {
        model.and_then(|model| self.runtime.catalog_multi_agent_version(model))
    }

    pub(crate) fn tool(
        self: &Arc<Self>,
        tool: CollaborationTool,
        spawned_in_tree: bool,
    ) -> Arc<dyn Tool> {
        Arc::new(ProductCollaborationTool {
            tool,
            thread: Arc::clone(self),
            spawned_in_tree,
        })
    }
}

fn parse_version(version: &str) -> Option<MultiAgentVersion> {
    match version {
        "disabled" => Some(MultiAgentVersion::Disabled),
        "v1" => Some(MultiAgentVersion::V1),
        "v2" => Some(MultiAgentVersion::V2),
        _ => None,
    }
}

const fn version_name(version: MultiAgentVersion) -> &'static str {
    match version {
        MultiAgentVersion::Disabled => "disabled",
        MultiAgentVersion::V1 => "v1",
        MultiAgentVersion::V2 => "v2",
    }
}

struct ProductCollaborationTool {
    tool: CollaborationTool,
    thread: Arc<ThreadCollaboration>,
    spawned_in_tree: bool,
}

#[async_trait]
impl Tool for ProductCollaborationTool {
    fn origin(&self) -> &ToolOrigin {
        self.tool.origin()
    }

    fn schema(&self) -> &ToolSchema {
        self.tool.schema()
    }

    fn func_schema(&self) -> Option<&FuncSchema> {
        self.tool.func_schema()
    }

    fn decode_input(&self, arguments: &serde_json::Value) -> Result<Option<DecodedToolInput>> {
        self.tool.decode_input(arguments)
    }

    fn options(&self) -> ToolOptions {
        self.tool.options()
    }

    async fn is_enabled(&self, context: &RunContext) -> Result<bool> {
        let locked = self.thread.version.get().copied();
        // Codex's V2 rule for any agent with an agent path, including a migrated thread's
        // children and a spawned thread loaded on its own: its current model must declare V2.
        let spawned = self.spawned_in_tree || self.thread.spawned;
        let model_version = if spawned || locked.is_none() {
            self.thread.model_version(context.resolved_model_name())
        } else {
            None
        };
        let version = locked.unwrap_or_else(|| {
            self.thread
                .runtime
                .multi_agent_version_for_model(model_version)
        });
        Ok(version == MultiAgentVersion::V2
            && (!spawned || model_version == Some(MultiAgentVersion::V2)))
    }

    async fn is_enabled_with_services(
        &self,
        context: &RunContext,
        services: &ToolServices,
    ) -> Result<bool> {
        Ok(services.agent_control().is_some() && self.is_enabled(context).await?)
    }

    async fn call(&self, context: ToolContext<'_>) -> Result<ToolOutput> {
        self.tool.call(context).await
    }

    async fn handle_failure(
        &self,
        context: &ToolContext<'_>,
        error: &Error,
    ) -> Result<Option<ToolOutput>> {
        self.tool.handle_failure(context, error).await
    }
}
